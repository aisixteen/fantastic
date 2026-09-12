// file_bridge — the gated filesystem edge: argument validation, the symlink
// clamp, refusal codes and the reflect posture. The 2026-09-02 closing sweep;
// mirrors py `file_bridge/tests/test_file.py` and rust `fantastic-file/tests.rs`.

import FantasticFile
import FantasticIoBridge
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@Suite("file_bridge (validation, symlinks, codes, posture)")
struct FileBundleTests {

    /// A file_bridge `fs` rooted at a fresh cwd-relative dir (running-dir law),
    /// OPEN unless `sealed`. Returns (kernel, served dir).
    private func openBridge(meta extra: OrderedDictionary<String, JSON> = [:]) async -> (
        Kernel, URL
    ) {
        let registry = BundleRegistry()
        registry.register("file_bridge.tools", FileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let fm = FileManager.default
        let rel = "fantastic-filebundle-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        var meta: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "core",
            "handler_module": "file_bridge.tools", "id": "fs", "root": .string(rel),
            "ingress_rule": "allow_all_ingress",
        ]
        for (k, v) in extra { meta[k] = v }
        let r = await kernel.send([Item("core", .object(meta))])[0].result
        #expect(r["id"].asString == meta["id"]?.asString, "\(r)")
        return (kernel, dir)
    }

    private func call(_ k: Kernel, _ payload: OrderedDictionary<String, JSON>) async -> JSON {
        await k.send([Item(AgentId("fs"), .object(payload))])[0].result
    }

    private func bin(_ k: Kernel, _ header: OrderedDictionary<String, JSON>, _ blob: Data = Data())
        async -> (JSON, Data)
    {
        await k.sendWithBinary(AgentId("fs"), .object(header), blob)
    }

    private func write(_ dir: URL, _ path: String, _ text: String) {
        let url = dir.appendingPathComponent(path)
        try? FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? Data(text.utf8).write(to: url)
    }

    private func lexists(_ url: URL) -> Bool {
        (try? FileManager.default.attributesOfItem(atPath: url.path)) != nil
    }

    @Test func negativeOffsetAndZeroLengthAreRefusedNotFatal() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.txt", "abc")
        // Every one of these used to reach `UInt64(Int64)` — a process trap.
        let cases: [OrderedDictionary<String, JSON>] = [
            ["type": "read_stream", "path": "a.txt", "offset": .integer(-1)],
            ["type": "read_stream", "path": "a.txt", "length": .integer(0)],
            ["type": "read_stream", "path": "a.txt", "offset": "x"],
            ["type": "read_stream", "path": "a.txt", "offset": .bool(true)],
            ["type": "read_stream", "path": "a.txt", "length": .double(1.5)],
            ["type": "write_stream", "path": "a.txt", "offset": .integer(-5)],
        ]
        for header in cases {
            let (r, body) = await bin(k, header, Data("z".utf8))
            #expect(r["reason"].asString == "bad_arg", "\(header) → \(r)")
            #expect(header[r["arg"].asString ?? "?"] != nil, "\(r)")
            #expect(body.isEmpty)
        }
        let (r, _) = await call(k, ["type": "pump", "source_path": "a.txt", "sink_path": "b.txt", "chunk": .integer(-1)]).pair
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "chunk", "\(r)")
        // the legal shape still streams; header keys match python (no bytes_len)
        let (m, b) = await bin(k, ["type": "read_stream", "path": "a.txt", "offset": .integer(1), "length": .integer(1)])
        #expect(String(decoding: b, as: UTF8.self) == "b", "\(m)")
        #expect(m["next_offset"].asInt == 2)
        #expect(m["bytes_len"].isNull)
        // a huge length reads only what exists
        let (m2, b2) = await bin(k, ["type": "read_stream", "path": "a.txt", "length": .integer(Int64.max)])
        #expect(b2.count == 3, "\(m2)")
        #expect(m2["eof"].asBool == true)
    }

    @Test func anInRootSymlinkPointingOutsideRefuses() async throws {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let fm = FileManager.default
        let outside = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-outside-\(UUID().uuidString)")
        try fm.createDirectory(at: outside, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: outside) }
        try Data("outside".utf8).write(to: outside.appendingPathComponent("secret.txt"))
        try fm.createSymbolicLink(at: dir.appendingPathComponent("docs"), withDestinationURL: outside)
        // The lexical form is in-root; the canonical form is not.
        let (m, b) = await bin(k, ["type": "read_stream", "path": "docs/secret.txt"])
        #expect(m["reason"].asString == "transport_error", "\(m)")
        #expect(m["error"].asString?.contains("escapes root") == true)
        #expect(b.isEmpty)
        let l = await call(k, ["type": "list", "path": "docs"])
        #expect(l["reason"].asString == "transport_error", "\(l)")
        let (w, _) = await bin(k, ["type": "write_stream", "path": "docs/new.txt"], Data("x".utf8))
        #expect(w["reason"].asString == "transport_error", "\(w)")
        #expect(!fm.fileExists(atPath: outside.appendingPathComponent("new.txt").path))
        // An in-root symlink still serves.
        write(dir, "real.txt", "r")
        try fm.createSymbolicLink(
            at: dir.appendingPathComponent("alias.txt"),
            withDestinationURL: dir.appendingPathComponent("real.txt"))
        let (m2, b2) = await bin(k, ["type": "read_stream", "path": "alias.txt"])
        #expect(m2["error"].isNull, "\(m2)")
        #expect(String(decoding: b2, as: UTF8.self) == "r")
    }

    @Test func deleteOnSymlinkRemovesTheLinkOnly() async throws {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let fm = FileManager.default
        write(dir, "data/keep.txt", "k")
        try fm.createSymbolicLink(
            at: dir.appendingPathComponent("link"),
            withDestinationURL: dir.appendingPathComponent("data"))
        let r = await call(k, ["type": "delete", "path": "link", "recursive": .bool(true)])
        #expect(r["deleted"].asBool == true, "\(r)")
        #expect(!lexists(dir.appendingPathComponent("link")), "link must be gone")
        #expect(fm.fileExists(atPath: dir.appendingPathComponent("data/keep.txt").path))
        // A DANGLING symlink is still a deletable name.
        try fm.createSymbolicLink(
            at: dir.appendingPathComponent("dangling"),
            withDestinationURL: dir.appendingPathComponent("gone.txt"))
        let r2 = await call(k, ["type": "delete", "path": "dangling"])
        #expect(r2["deleted"].asBool == true, "\(r2)")
        #expect(!lexists(dir.appendingPathComponent("dangling")))
    }

    @Test func bareDeleteRefusesAPopulatedDirRecursiveRemovesIt() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "src/a.txt", "x")
        let r = await call(k, ["type": "delete", "path": "src"])
        #expect(r["reason"].asString == "transport_error", "\(r)")
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("src/a.txt").path))
        let r2 = await call(k, ["type": "delete", "path": "src", "recursive": .bool(true)])
        #expect(r2["deleted"].asBool == true, "\(r2)")
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("src").path))
        // an EMPTY dir needs no flag
        _ = await call(k, ["type": "mkdir", "path": "empty"])
        let r3 = await call(k, ["type": "delete", "path": "empty"])
        #expect(r3["deleted"].asBool == true, "\(r3)")
    }

    @Test func emptyPathDeleteRenameMkdirAreRefused() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "keep.txt", "k")
        _ = await call(k, ["type": "mkdir", "path": "a"])
        let cases: [(OrderedDictionary<String, JSON>, String, String)] = [
            (["type": "delete", "path": "", "recursive": .bool(true)], "path", "missing_arg"),
            (["type": "delete", "recursive": .bool(true)], "path", "missing_arg"),
            (["type": "delete", "path": ".", "recursive": .bool(true)], "path", "missing_arg"),
            (["type": "delete", "path": "a/..", "recursive": .bool(true)], "path", "bad_arg"),
            (["type": "rename", "old_path": "", "new_path": "x"], "old_path", "missing_arg"),
            (["type": "rename", "old_path": "a", "new_path": ""], "new_path", "missing_arg"),
            (["type": "rename", "old_path": ".", "new_path": "x"], "old_path", "missing_arg"),
            (["type": "rename", "old_path": "a", "new_path": "a/.."], "new_path", "bad_arg"),
            (["type": "mkdir", "path": ""], "path", "missing_arg"),
            (["type": "mkdir"], "path", "missing_arg"),
            (["type": "mkdir", "path": "."], "path", "missing_arg"),
            (["type": "mkdir", "path": "a/.."], "path", "bad_arg"),
        ]
        for (payload, arg, reason) in cases {
            let r = await call(k, payload)
            #expect(r["reason"].asString == reason && r["arg"].asString == arg, "\(payload) → \(r)")
        }
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("keep.txt").path))
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("a").path))
    }

    @Test func absolutePathIsRefusedWithBadArg() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "README.md", "r")
        let abs = dir.appendingPathComponent("README.md").path
        var replies: [JSON] = [
            await bin(k, ["type": "read_stream", "path": .string(abs)]).0,
            await bin(k, ["type": "write_stream", "path": "/x"], Data("b".utf8)).0,
        ]
        replies.append(await call(k, ["type": "list", "path": "/"]))
        replies.append(await call(k, ["type": "delete", "path": .string(abs)]))
        replies.append(await call(k, ["type": "mkdir", "path": "/x"]))
        replies.append(await call(k, ["type": "rename", "old_path": .string(abs), "new_path": "y"]))
        for r in replies {
            #expect(r["reason"].asString == "bad_arg", "\(r)")
            #expect(r["error"].asString?.contains("root-relative") == true, "\(r)")
        }
        #expect(FileManager.default.fileExists(atPath: abs))
    }

    @Test func listRefusalsCarryCodes() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "f.txt", "f")
        let r = await call(k, ["type": "list", "path": "nope"])
        #expect(r["reason"].asString == "not_found" && r["path"].asString == "nope", "\(r)")
        let r2 = await call(k, ["type": "list", "path": "f.txt"])
        #expect(r2["reason"].asString == "bad_arg" && r2["arg"].asString == "path", "\(r2)")
        let r3 = await call(k, ["type": "rename", "old_path": "nope", "new_path": "x"])
        #expect(r3["reason"].asString == "not_found" && r3["path"].asString == "nope", "\(r3)")
        let r4 = await call(k, ["type": "delete", "path": "nope"])
        #expect(r4["reason"].asString == "not_found", "\(r4)")
        let r5 = await call(k, ["type": "frobnicate"])
        #expect(r5["reason"].asString == "unhandled" && r5["verb"].asString == "frobnicate", "\(r5)")
    }

    @Test func listShowsDotfilesAndHonoursHiddenMeta() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, ".gitignore", "g")
        write(dir, ".git/HEAD", "h")
        write(dir, "visible.txt", "v")
        let names = { (r: JSON) -> [String] in
            (r["files"].asArray ?? []).compactMap { $0["name"].asString }
        }
        // default: dotfiles show, DEFAULT_HIDDEN (.git) does not
        let r = await call(k, ["type": "list"])
        #expect(names(r) == [".gitignore", "visible.txt"], "\(r)")
        // an operator's `hidden` override is honoured — `.git` unhidden
        _ = await k.send([Item("core", .object(["type": "update_agent", "id": "fs", "hidden": .array([])]))])[0].result
        let r2 = await call(k, ["type": "list"])
        #expect(names(r2) == [".git", ".gitignore", "visible.txt"], "\(r2)")
        _ = await k.send([Item("core", .object(["type": "update_agent", "id": "fs", "hidden": .array(["visible.txt"])]))])[0].result
        let r3 = await call(k, ["type": "list"])
        #expect(names(r3) == [".git", ".gitignore"], "\(r3)")
        let refl = await call(k, ["type": "reflect"])
        #expect(refl["hidden_count"].asInt == 1, "\(refl)")
    }

    @Test func writeStreamReportsFileSizeNotCursor() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "big.bin", String(repeating: "x", count: 100))
        // 5 bytes at offset 0, no truncate: the FILE is still 100 bytes.
        let (w, _) = await bin(k, ["type": "write_stream", "path": "big.bin", "offset": .integer(0)], Data("hello".utf8))
        #expect(w["written"].asInt == 5, "\(w)")
        #expect(w["offset"].asInt == 0)
        #expect(w["size"].asInt == 100, "size must be the file's, not the cursor: \(w)")
        // append: cursor and size agree
        let (w2, _) = await bin(k, ["type": "write_stream", "path": "big.bin"], Data("!!".utf8))
        #expect(w2["offset"].asInt == 100 && w2["size"].asInt == 102, "\(w2)")
    }

    @Test func reflectCarriesPostureAndBootIsNull() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, ["type": "reflect"])
        #expect(r["ingress_rule"].asString == "allow_all_ingress", "\(r)")
        #expect(r["egress_rule"].asString == "silent_egress")
        #expect(r["sealed"].asBool == false)
        #expect(r["root_error"].isNull)
        #expect(r["sentence"].asString?.hasPrefix("Filesystem edge of the io family") == true)
        #expect(r["hidden_count"].asInt == 5)
        let b = await call(k, ["type": "boot"])
        #expect(b.isNull, "\(b)")
        // sealed + a root outside the running dir: reflect never refuses, it REPORTS
        let (k2, dir2) = await openBridge(meta: ["id": "fs2", "root": "/", "ingress_rule": .null])
        defer { try? FileManager.default.removeItem(at: dir2) }
        let r2 = await k2.send([Item(AgentId("fs2"), .object(["type": "reflect"]))])[0].result
        #expect(r2["sealed"].asBool == true && r2["ingress_rule"].asString == "deny_ingress", "\(r2)")
        #expect(r2["root_error"].asString?.contains("escapes the running dir") == true, "\(r2)")
    }

    /// THE STREAM VERBS ON THE TEXT CHANNEL, as python serves them: a model
    /// writes a document VERBATIM between `<bytes>` and `</bytes>` and the tag
    /// grammar delivers it as utf-8 text to the same handler; a chunk read back
    /// renders utf-8 in the reply. Swift refused both with "raw bytes on the
    /// binary channel" until 2026-09-13 — the refusal the `subai_graph` routine
    /// died on, the same gap rust closed the day before.
    @Test func textChannelStreamVerbsServeUtf8LikePython() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let w = await call(k, ["type": "write_stream", "path": "a.txt", "bytes": "hello\nwörld"])
        #expect(w["error"].isNull && w["path"].asString == "a.txt", "\(w)")
        let r = await call(k, ["type": "read_stream", "path": "a.txt"])
        #expect(r["bytes"].asString == "hello\nwörld" && r["eof"].asBool == true, "\(r)")
        // A bare number is a document too (the tag grammar types it).
        let n = await call(k, ["type": "write_stream", "path": "n.txt", "bytes": .integer(42), "truncate": .bool(true)])
        #expect(n["error"].isNull, "\(n)")
        #expect((await call(k, ["type": "read_stream", "path": "n.txt"]))["bytes"].asString == "42")
        // No chunk at all is python's `missing` refusal, never a channel lecture.
        let m = await call(k, ["type": "write_stream", "path": "a.txt"])
        #expect(m["reason"].asString == "missing_arg" && m["error"].asString == "write_stream: bytes (one raw chunk) required", "\(m)")
        let nf = await call(k, ["type": "read_stream", "path": "nope.txt"])
        #expect(nf["reason"].asString == "not_found", "\(nf)")
    }

    @Test func passwordIngressIsRefusedOnFsAndProcessLegs() async {
        // The fs half (the process half lives in FantasticExecBridgeTests).
        let b = FileBundle()
        let specs: [(String, JSON)] = [
            ("ingress_rule", "password_ingress"),
            ("ingress_rule", .object(["type": "password_ingress", "env": "X"])),
            ("auth", "password"),
        ]
        for (field, spec) in specs {
            let e = b.validateMeta([field: spec], record: [:])
            #expect(e?.contains("wire envelope") == true, "\(field) → \(String(describing: e))")
            #expect(e?.contains("allow_all_ingress") == true)
        }
        #expect(b.validateMeta(["ingress_rule": "allow_all_ingress"], record: [:]) == nil)
        // and at the write: create_agent refuses with invalid_meta
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await k.send([Item("core", .object([
            "type": "create_agent", "parent_id": "core", "handler_module": "file_bridge.tools",
            "id": "pw", "ingress_rule": "password_ingress",
        ]))])[0].result
        #expect(r["reason"].asString == "invalid_meta", "\(r)")
    }
}

extension JSON {
    /// `(self, Data())` — lets a text-channel reply share the binary tuple shape.
    fileprivate var pair: (JSON, Data) { (self, Data()) }
}
