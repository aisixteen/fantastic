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
        let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
        rootPath: URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
            .appendingPathComponent(".fantastic"))
        kernel.register(root)
        kernel.setRoot(root)
        let fm = FileManager.default
        let rel = "fantastic-filebundle-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        var meta: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "file_bridge.tools", "allow": ["."], "id": "fs", "root": .string(rel),
            "ingress_rule": "allow_all_ingress",
        ]
        for (k, v) in extra { meta[k] = v }
        let r = await kernel.send([Item("kernel_state", .object(meta))])[0].result
        #expect(r["id"].asString == meta["id"]?.asString, "\(r)")
        return (kernel, dir)
    }

    private func call(_ k: Kernel, _ payload: OrderedDictionary<String, JSON>) async -> JSON {
        await k.send([Item(AgentId("fs"), .object(payload))])[0].result
    }

    /// `blob` is the request's chunk (at the payload's `bytes`); `nil` asks for
    /// the reply's chunk raw (a `read_stream`).
    private func bin(_ k: Kernel, _ header: OrderedDictionary<String, JSON>, _ blob: Data? = nil)
        async -> (JSON, Data?)
    {
        let item = Item(
            AgentId("fs"), .object(header), binary: blob.map { Blob("bytes", $0) },
            rawReply: blob == nil)
        let out = await k.send([item])[0]
        return (out.result, out.body)
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
            #expect(body == nil, "a refusal carries no chunk")
        }
        let (r, _) = await call(
            k,
            ["type": "pump", "source_path": "a.txt", "sink_path": "b.txt", "chunk": .integer(-1)]
        ).pair
        #expect(r["reason"].asString == "bad_arg" && r["arg"].asString == "chunk", "\(r)")
        // the legal shape still streams; header keys match python (no bytes_len)
        let (m, b) = await bin(
            k,
            ["type": "read_stream", "path": "a.txt", "offset": .integer(1), "length": .integer(1)])
        #expect(String(decoding: b ?? Data(), as: UTF8.self) == "b", "\(m)")
        #expect(m["next_offset"].asInt == 2)
        #expect(m["bytes_len"].isNull)
        // a huge length reads only what exists
        let (m2, b2) = await bin(
            k, ["type": "read_stream", "path": "a.txt", "length": .integer(Int64.max)])
        #expect(b2?.count == 3, "\(m2)")
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
        try fm.createSymbolicLink(
            at: dir.appendingPathComponent("docs"), withDestinationURL: outside)
        // The lexical form is in-root; the canonical form is not.
        let (m, b) = await bin(k, ["type": "read_stream", "path": "docs/secret.txt"])
        #expect(m["reason"].asString == "transport_error", "\(m)")
        #expect(m["error"].asString?.contains("escapes root") == true)
        #expect(b == nil, "a refusal carries no chunk")
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
        #expect(String(decoding: b2 ?? Data(), as: UTF8.self) == "r")
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
        #expect(
            FileManager.default.fileExists(atPath: dir.appendingPathComponent("src/a.txt").path))
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
        replies.append(
            await call(k, ["type": "rename", "old_path": .string(abs), "new_path": "y"]))
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
        #expect(
            r5["reason"].asString == "unhandled" && r5["verb"].asString == "frobnicate", "\(r5)")
    }

    @Test func listShowsEveryEntryUnderTheRoot() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, ".gitignore", "g")
        write(dir, ".git/HEAD", "h")
        write(dir, "visible.txt", "v")
        let r = await call(k, ["type": "list"])
        #expect(
            (r["files"].asArray ?? []).compactMap { $0["name"].asString }
                == [".git", ".gitignore", "visible.txt"], "\(r)")
    }

    @Test func writeStreamReportsFileSizeNotCursor() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "big.bin", String(repeating: "x", count: 100))
        // 5 bytes at offset 0, no truncate: the FILE is still 100 bytes.
        let (w, _) = await bin(
            k, ["type": "write_stream", "path": "big.bin", "offset": .integer(0)],
            Data("hello".utf8))
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
        let b = await call(k, ["type": "boot"])
        #expect(b.isNull, "\(b)")
        // sealed + a root outside the running dir: reflect never refuses, it REPORTS
        let (k2, dir2) = await openBridge(meta: ["id": "fs2", "root": "/", "ingress_rule": .null])
        defer { try? FileManager.default.removeItem(at: dir2) }
        let r2 = await k2.send([Item(AgentId("fs2"), .object(["type": "reflect"]))])[0].result
        #expect(
            r2["sealed"].asBool == true && r2["ingress_rule"].asString == "deny_ingress", "\(r2)")
        #expect(r2["root_error"].asString?.contains("escapes the running dir") == true, "\(r2)")
    }

    /// A13.19 — `bytes` ON THE TEXT CHANNEL IS NEVER A CHUNK. Python types the
    /// argument (`isinstance(data, (bytes, bytearray))`) and JSON has no bytes,
    /// so a JSON string, a number or a null from a wire client is `missing_arg`
    /// and NOTHING is written. Measured `text_call_write_stream_str_bytes`:
    /// python refuses and `ws_text.txt` is absent; swift wrote `3` bytes.
    ///
    /// A model's `<bytes>` document keeps working because it rides the BINARY
    /// channel (D15) — the grammar's one bytes-typed argument becomes the item's
    /// blob, which is exactly the type python's parser produces.
    @Test func textWriteStreamWithStringBytesIsMissingArg() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        for value: JSON in ["hello\nwörld", .integer(42), .double(1.5), .null, .bool(true)] {
            let w = await call(k, ["type": "write_stream", "path": "a.txt", "bytes": value])
            #expect(
                w["reason"].asString == "missing_arg"
                    && w["error"].asString == "write_stream: bytes (one raw chunk) required",
                "\(value) → \(w)")
        }
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("a.txt").path))
        // The SAME document through the carrier a model's `<bytes>` rides.
        let (w, _) = await bin(
            k, ["type": "write_stream", "path": "a.txt"], Data("hello\nwörld".utf8))
        #expect(w["error"].isNull && w["path"].asString == "a.txt", "\(w)")
        // A chunk read back still renders utf-8 in the TEXT reply (A2.6) — that
        // half is how a model reads a document it wrote.
        let r = await call(k, ["type": "read_stream", "path": "a.txt"])
        #expect(r["bytes"].asString == "hello\nwörld" && r["eof"].asBool == true, "\(r)")
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
        let r = await k.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "file_bridge.tools", "allow": ["."],
                    "id": "pw", "ingress_rule": "password_ingress",
                ]))
        ])[0].result
        #expect(r["reason"].asString == "invalid_meta", "\(r)")
    }

    // ─── python parity (A1, A2) ─────────────────────────────────

    /// A2.1 — reflect key order is python's: the posture keys sit AFTER `verbs`
    /// and `root_error` is last.
    @Test func reflectKeyOrderMatchesPython() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, ["type": "reflect"])
        // The BUNDLE's keys, in python's order (the kernel frame — can_call,
        // kernel_verbs, tree — is appended after them by the substrate).
        #expect(
            Array(r.asObject?.keys ?? []).prefix(10) == [
                "id", "sentence", "root", "readonly", "allow_count", "edit_grammar", "compose",
                "verbs", "ingress_rule", "egress_rule",
            ], "\(Array(r.asObject?.keys ?? []))")
        let (k2, dir2) = await openBridge(meta: ["id": "fs2", "root": "../nope"])
        defer { try? FileManager.default.removeItem(at: dir2) }
        let r2 = await k2.send([Item(AgentId("fs2"), .object(["type": "reflect"]))])[0].result
        // `root_error` is last of the bundle's own keys.
        #expect(
            Array(r2.asObject?.keys ?? []).prefix(12).last == "root_error",
            "\(Array(r2.asObject?.keys ?? []))")
    }

    /// A2.2 — the root-escape text is python's, `{!r}` on the CONFIGURED string.
    @Test func rootEscapeErrorIsPythonText() async {
        let (k, dir) = await openBridge(meta: ["id": "fs2", "root": "../x"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await k.send([Item(AgentId("fs2"), .object(["type": "reflect"]))])[0].result
        #expect(r["root_error"].asString == "fs: root '../x' escapes the running dir", "\(r)")
        #expect(r["root"].asString == "../x", "the configured string, verbatim: \(r)")
        let l = await k.send([Item(AgentId("fs2"), .object(["type": "list"]))])[0].result
        #expect(l["error"].asString == "fs: root '../x' escapes the running dir", "\(l)")
        #expect(l["reason"].asString == "transport_error", "\(l)")
    }

    /// A2.3 — every refusal quotes with python's `{x!r}`, not swift's `"…"`.
    @Test func refusalTextsUsePythonRepr() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "f.txt", "f")
        #expect(
            (await call(k, ["type": "nope"]))["error"].asString
                == "file_bridge: unknown type 'nope'")
        // (A `type: null` never reaches the bundle here: the swift KERNEL refuses
        // it first with `send: payload.type must be a non-empty string`, where
        // python dispatches it and file_bridge answers `unknown type None`. That
        // is a kernel-side difference, not this bundle's — the bundle's own
        // rendering of a null verb is `pyRepr(.null)` == `None`.)
        #expect(pyRepr(JSON.null) == "None")
        #expect(
            (await call(k, ["type": "list", "path": "nope"]))["error"].asString
                == "path 'nope' does not exist")
        #expect(
            (await call(k, ["type": "list", "path": "f.txt"]))["error"].asString
                == "list: path must be a directory; 'f.txt' is not one")
        #expect(
            (await call(k, ["type": "read_stream", "path": "no pe.txt"]))["error"].asString
                == "file 'no pe.txt' not found")
        #expect(
            (await call(k, ["type": "list", "path": "../x"]))["error"].asString
                == "fs: path '../x' escapes root")
        // validate_meta names the SPEC as written, not the resolved rule name.
        #expect(
            FileBundle().validateMeta(["ingress_rule": "bogus_ingress"], record: [:])
                == "ingress_rule='bogus_ingress' is not a rule this bundle implements — "
                + "legal values: allow_all_ingress, deny_ingress")
        #expect(
            FileBundle().validateMeta(
                ["ingress_rule": .object(["type": "password_ingress", "env": "X"])], record: [:])
                == "ingress_rule={'type': 'password_ingress', 'env': 'X'} is not applicable to "
                + "the fs edge: a leg with no wire envelope can never present a token — "
                + "legal values: allow_all_ingress, deny_ingress")
    }

    /// A2.4 / A2.5 — an unknown verb is `unhandled` BEFORE the gate; `boot` is
    /// gated (a sealed leg denies it) and `shutdown` is not a verb at all.
    @Test func unknownVerbOnSealedBridgeIsUnhandled() async {
        let (k, dir) = await openBridge(meta: ["ingress_rule": .null])
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, ["type": "frobnicate"])
        #expect(r["reason"].asString == "unhandled" && r["verb"].asString == "frobnicate", "\(r)")
        let s = await call(k, ["type": "shutdown"])
        #expect(s["reason"].asString == "unhandled" && s["verb"].asString == "shutdown", "\(s)")
        let b = await call(k, ["type": "boot"])
        #expect(b["reason"].asString == "unauthorized", "a sealed boot is denied: \(b)")
        // reflect is the ONE ungated verb.
        #expect((await call(k, ["type": "reflect"]))["sealed"].asBool == true)
    }

    /// A2.5 — the same on an OPEN leg: `boot` answers null, `shutdown` unhandled.
    @Test func sealedBootIsDeniedAndShutdownIsUnhandled() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect((await call(k, ["type": "boot"])).isNull)
        #expect((await call(k, ["type": "shutdown"]))["reason"].asString == "unhandled")
    }

    /// A2.6 — the text-channel `read_stream` reply key order is python's.
    @Test func textReadStreamReplyKeyOrder() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.txt", "abc")
        let r = await call(k, ["type": "read_stream", "path": "a.txt"])
        #expect(
            Array(r.asObject?.keys ?? [])
                == ["path", "offset", "bytes", "next_offset", "eof", "size", "origin"], "\(r)")
    }

    /// A2.7 — an ABSENT `bytes` key writes an EMPTY chunk (python
    /// `payload.get("bytes", b"")`); the check order is readonly, path, bytes.
    @Test func writeStreamWithoutBytesWritesAnEmptyChunk() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let w = await call(k, ["type": "write_stream", "path": "empty.txt"])
        #expect(w["written"].asInt == 0 && w["size"].asInt == 0, "\(w)")
        #expect(
            FileManager.default.fileExists(atPath: dir.appendingPathComponent("empty.txt").path))
        // a bad path still wins over the absent chunk
        let bad = await call(k, ["type": "write_stream", "path": .integer(5)])
        #expect(bad["reason"].asString == "bad_arg" && bad["arg"].asString == "path", "\(bad)")
    }

    /// A2.8 — the bundle readme is python's `readme.md`, byte for byte.
    @Test func readmeIsPythonVerbatim() throws {
        let expected = try String(
            contentsOf: pythonSource("io/file_bridge/src/file_bridge/readme.md"), encoding: .utf8)
        #expect(FileBundle().readme == expected)
    }

    /// A2.9 — the pre-disk order: readonly wins over a root that escapes.
    @Test func readonlyWinsOverRootEscape() async {
        let (k, dir) = await openBridge(meta: ["root": "../x", "readonly": .bool(true)])
        defer { try? FileManager.default.removeItem(at: dir) }
        for payload: OrderedDictionary<String, JSON> in [
            ["type": "delete", "path": "a"], ["type": "mkdir", "path": "a"],
            ["type": "rename", "old_path": "a", "new_path": "b"],
            ["type": "write_stream", "path": "a", "bytes": "x"],
        ] {
            let r = await call(k, payload)
            #expect(r["error"].asString == "agent is readonly", "\(payload) → \(r)")
            #expect(r["reason"].asString == "unauthorized", "\(r)")
        }
    }

    /// A2.10 — the pump's `or` fallbacks are python's, and a source that
    /// answers no chunk is refused rather than looped on.
    @Test func pumpFalsyArgsFallBackLikePython() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.txt", "abcdef")
        // empty source/sink = self; empty source_path falls through to `path`;
        // absent sink_path = the source path.
        let r = await call(
            k,
            [
                "type": "pump", "source": "", "sink": "", "source_path": "", "path": "a.txt",
                "sink_path": "b.txt",
            ])
        #expect(r["source"].asString == "a.txt" && r["sink"].asString == "b.txt", "\(r)")
        #expect(r["bytes"].asInt == 6 && r["chunks"].asInt == 1, "\(r)")
        #expect(
            (try? String(contentsOf: dir.appendingPathComponent("b.txt"), encoding: .utf8))
                == "abcdef")
        // a SOURCE that is not a stream provider: refused, named with `{src!r}`.
        let bad = await call(k, ["type": "pump", "source": "kernel_state", "source_path": "a.txt"])
        #expect(bad["reason"].asString == "transport_error", "\(bad)")
        #expect(
            bad["error"].asString?.hasPrefix("pump: read from 'kernel_state' failed: ") == true,
            "\(bad)")
    }

    /// Every chunk the pump moves is an EXECUTION — python sends each read and
    /// write through `kernel.send`, so each has a call row (parented to the
    /// pump's own execution) and a settle. Swift used a bypass that wrote none.
    @Test func pumpChunksAreExecutionsParentedToThePump() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "a.txt", "abcdef")
        let out = await k.send([
            Item(AgentId("fs"), ["type": "pump", "source_path": "a.txt", "sink_path": "b.txt"])
        ])[0]
        #expect(out.result["chunks"].asInt == 1, "\(out.result)")
        let pumpId = out.pid?.value
        let calls = k.rows.all().filter {
            $0["agent"]?.asString == "fs" && $0["kind"]?.asString == "call"
                && $0["parent"]?.asInt == pumpId
        }
        #expect(
            calls.map { $0["verb"]?.asString ?? "" } == ["read_stream", "write_stream"],
            "\(calls)")
        #expect(calls.allSatisfy { $0["outcome"]?["status"].asString == "completed" }, "\(calls)")
    }

    /// A2.11 — a `list` entry's `path` is relative to the RESOLVED root, and a
    /// stat failure reports `-1`.
    @Test func listPathsAreRelativeToResolvedRoot() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "x/a.txt", "a")
        let r = await call(k, ["type": "list", "path": "sub/../x"])
        #expect(r["files"][0]["path"].asString == "x/a.txt", "\(r)")
        #expect(r["files"][0]["size"].asInt == 1, "\(r)")
        let dot = await call(k, ["type": "list", "path": "."])
        #expect(dot["files"][0]["path"].asString == "x", "\(dot)")
        // a dangling symlink cannot be stat'ed: python reports -1, not an error
        try? FileManager.default.createSymbolicLink(
            at: dir.appendingPathComponent("x/gone"),
            withDestinationURL: dir.appendingPathComponent("x/missing.txt"))
        let r2 = await call(k, ["type": "list", "path": "x"])
        let gone = (r2["files"].asArray ?? []).first { $0["name"].asString == "gone" }
        #expect(gone?["size"].asInt == -1, "\(r2)")
    }

    /// A2.12 — an OS failure renders as python's `str(OSError)`.
    @Test func osErrorTextIsPythonOSErrorStr() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "src/a.txt", "x")
        let r = await call(k, ["type": "delete", "path": "src"])
        #expect(
            r["error"].asString
                == "[Errno 66] Directory not empty: \(pyRepr(dir.appendingPathComponent("src").path))",
            "\(r)")
        let m = await call(k, ["type": "mkdir", "path": "src/a.txt/deeper"])
        #expect(
            m["error"].asString
                == "[Errno 20] Not a directory: \(pyRepr(dir.appendingPathComponent("src/a.txt/deeper").path))",
            "\(m)")
    }

    /// A2.13 — `truncate`, `recursive` and `readonly` use python truthiness.
    @Test func truncateAndRecursiveUsePythonTruthiness() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        write(dir, "t.txt", "abcdef")
        // a truthy STRING truncates. The chunk rides the binary channel — a
        // text-channel `bytes` is `missing_arg` since A13.19.
        let (w, _) = await bin(
            k, ["type": "write_stream", "path": "t.txt", "truncate": "yes"], Data("z".utf8))
        #expect(w["size"].asInt == 1, "\(w)")
        // `0` is falsy: no truncate
        let (w2, _) = await bin(
            k,
            [
                "type": "write_stream", "path": "t.txt", "offset": .integer(0),
                "truncate": .integer(0),
            ],
            Data("q".utf8))
        #expect(w2["size"].asInt == 1, "\(w2)")
        write(dir, "d/a.txt", "x")
        #expect(
            (await call(k, ["type": "delete", "path": "d", "recursive": .integer(0)]))["reason"]
                .asString == "transport_error")
        #expect(
            (await call(k, ["type": "delete", "path": "d", "recursive": "sure"]))["deleted"].asBool
                == true)
        // `readonly` too: the string "no" is TRUE in python
        _ = await k.send([
            Item("kernel_state", .object(["type": "update_agent", "id": "fs", "readonly": "no"]))
        ])[0].result
        #expect(
            (await call(k, ["type": "mkdir", "path": "q"]))["error"].asString == "agent is readonly"
        )
        _ = await k.send([
            Item(
                "kernel_state",
                .object(["type": "update_agent", "id": "fs", "readonly": .integer(0)]))
        ])[0].result
        #expect((await call(k, ["type": "mkdir", "path": "q"]))["created"].asBool == true)
    }

    /// A2.15 — a DANGLING symlink leaf pointing outside the root escapes
    /// (python's `realpath(strict=False)` follows it), and nothing is created
    /// through it.
    @Test func danglingSymlinkPointingOutsideRootRefuses() async throws {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let fm = FileManager.default
        let outside = URL(fileURLWithPath: NSTemporaryDirectory())
            .appendingPathComponent("fantastic-dangling-\(UUID().uuidString)")
        try fm.createSymbolicLink(
            at: dir.appendingPathComponent("out.txt"), withDestinationURL: outside)
        for payload: OrderedDictionary<String, JSON> in [
            ["type": "delete", "path": "out.txt"],
            ["type": "read_stream", "path": "out.txt"],
        ] {
            let r = await call(k, payload)
            #expect(r["reason"].asString == "transport_error", "\(payload) → \(r)")
            #expect(r["error"].asString == "fs: path 'out.txt' escapes root", "\(r)")
        }
        // The write carries its chunk on the binary channel (A13.19).
        let (wr, _) = await bin(
            k, ["type": "write_stream", "path": "out.txt"], Data("x".utf8))
        #expect(wr["reason"].asString == "transport_error", "\(wr)")
        #expect(wr["error"].asString == "fs: path 'out.txt' escapes root", "\(wr)")
        #expect(!fm.fileExists(atPath: outside.path), "nothing was created through the link")
    }

    /// A2.20 — reflect's `root`: the resolved path (keeping `/private`), the
    /// configured string verbatim on escape.
    @Test func reflectRootIsVerbatimOnEscapeAndResolvedOtherwise() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, ["type": "reflect"])
        #expect(r["root"].asString == dir.path, "\(r)")
        let (k2, dir2) = await openBridge(meta: ["id": "fs2", "root": "~/nope"])
        defer { try? FileManager.default.removeItem(at: dir2) }
        let r2 = await k2.send([Item(AgentId("fs2"), .object(["type": "reflect"]))])[0].result
        // `~` is NOT expanded: it is a literal directory name under the workdir.
        #expect(r2["root"].asString?.hasSuffix("/~/nope") == true, "\(r2)")
        #expect(r2["root_error"].isNull, "\(r2)")
    }

    /// A2.20 — an ABSENT root serves the workdir, for reflect AND data verbs.
    @Test func absentRootServesTheWorkdir() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await k.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "file_bridge.tools", "allow": ["."],
                    "id": "wd", "ingress_rule": "allow_all_ingress",
                ]))
        ])[0].result
        #expect(r["id"].asString == "wd", "\(r)")
        let cwd = FileManager.default.currentDirectoryPath
        let refl = await k.send([Item(AgentId("wd"), .object(["type": "reflect"]))])[0].result
        #expect(refl["root"].asString == pyRealpath(cwd), "\(refl)")
        let l = await k.send([Item(AgentId("wd"), .object(["type": "list"]))])[0].result
        let names = (l["files"].asArray ?? []).compactMap { $0["name"].asString }
        #expect(names.contains("Package.swift"), "the workdir itself is served: \(names)")
    }

    /// A2.21 — symlinks are stat'ed THROUGH: a linked file reads its target's
    /// size, a linked dir lists as a dir.
    @Test func symlinkedFileReadsTargetSizeAndSymlinkedDirListsAsDir() async throws {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let fm = FileManager.default
        write(dir, "real.txt", "abcdef")
        write(dir, "d/inner.txt", "i")
        try fm.createSymbolicLink(
            at: dir.appendingPathComponent("alias.txt"),
            withDestinationURL: dir.appendingPathComponent("real.txt"))
        try fm.createSymbolicLink(
            at: dir.appendingPathComponent("dlink"),
            withDestinationURL: dir.appendingPathComponent("d"))
        let r = await call(k, ["type": "read_stream", "path": "alias.txt"])
        #expect(r["size"].asInt == 6 && r["bytes"].asString == "abcdef", "\(r)")
        let l = await call(k, ["type": "list"])
        let entries = l["files"].asArray ?? []
        let alias = entries.first { $0["name"].asString == "alias.txt" }
        let dlink = entries.first { $0["name"].asString == "dlink" }
        #expect(alias?["size"].asInt == 6, "\(l)")
        #expect(dlink?["type"].asString == "dir" && dlink?["size"].isNull == true, "\(l)")
    }

    /// A1.1 — an `auth` stem that already carries its suffix is reported as
    /// written, not scoped a second time.
    @Test func reflectPostureKeepsAnAlreadyScopedAuthStem() async {
        let (k, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        // NO `ingress_rule` key at all: the `auth` stem is what resolves.
        let made = await k.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "file_bridge.tools", "allow": ["."],
                    "id": "fs2", "auth": "allow_all_ingress",
                ]))
        ])[0].result
        #expect(made["id"].asString == "fs2", "\(made)")
        let r = await k.send([Item(AgentId("fs2"), .object(["type": "reflect"]))])[0].result
        // Reported as the GATE resolves it (python 2026-09-16): an `auth` stem
        // always gets the manifold suffix, so this one is an unknown rule — sealed.
        #expect(r["ingress_rule"].asString == "allow_all_ingress_ingress", "\(r)")
        #expect(r["sealed"].asBool == true, "\(r)")
        #expect(r["egress_rule"].asString == "allow_all_ingress_egress", "\(r)")
    }

    /// A1.3 — a present `ingress_rule: null` is a SEAL: python tests key
    /// presence, so it never falls back to the `auth` stem.
    @Test func nullIngressRuleDoesNotFallBackToAuth() async {
        let (k, dir) = await openBridge(meta: [
            "id": "fs2", "ingress_rule": .null, "auth": "allow_all",
        ])
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await k.send([Item(AgentId("fs2"), .object(["type": "reflect"]))])[0].result
        #expect(r["ingress_rule"].asString == "deny_ingress" && r["sealed"].asBool == true, "\(r)")
        let l = await k.send([Item(AgentId("fs2"), .object(["type": "list"]))])[0].result
        #expect(l["reason"].asString == "unauthorized", "\(l)")
    }
}

/// The canonical python source tree — the parity reference these tests read
/// their expected bytes from (python is immutable, so a copy here would be the
/// thing that drifts).
func pythonSource(_ relative: String) -> URL {
    URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // FantasticBundlesTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // swift
        .deletingLastPathComponent()  // lib
        .appendingPathComponent("python/bundled_agents")
        .appendingPathComponent(relative)
}

extension JSON {
    /// `(self, Data())` — lets a text-channel reply share the binary tuple shape.
    fileprivate var pair: (JSON, Data) { (self, Data()) }
}
