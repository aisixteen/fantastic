// file_bridge is DENY-ALL at the path level — python
// `file_bridge/tests/test_allow_list.py` is the oracle. The record's `allow` list
// names what the bridge exposes (a file, or everything under a directory; `.` =
// the whole root); no list exposes nothing, and anything outside it is
// `not_found` whether or not it exists.

import FantasticFile
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

private func site(allow: JSON?) async -> (Kernel, URL) {
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
    let rel = "fantastic-allow-\(UUID().uuidString)"
    let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
    try? fm.createDirectory(at: dir.appendingPathComponent("docs"), withIntermediateDirectories: true)
    try? Data("%PDF-a".utf8).write(to: dir.appendingPathComponent("docs/a.pdf"))
    try? Data("%PDF-b".utf8).write(to: dir.appendingPathComponent("docs/b.pdf"))
    try? Data("private\n".utf8).write(to: dir.appendingPathComponent("secret.txt"))
    var meta: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state",
        "handler_module": "file_bridge.tools", "id": "fs", "root": .string(rel),
        "ingress_rule": "allow_all_ingress",
    ]
    if let allow { meta["allow"] = allow }
    let r = await kernel.send([Item("kernel_state", .object(meta))])[0].result
    #expect(r["id"] == "fs", "\(r)")
    return (kernel, dir)
}

private func call(_ k: Kernel, _ p: OrderedDictionary<String, JSON>) async -> JSON {
    await k.send([Item(AgentId("fs"), .object(p))])[0].result
}

private func read(_ k: Kernel, _ path: String) async -> JSON {
    await call(k, ["type": "read_stream", "path": .string(path)])
}

private func names(_ r: JSON) -> [String] {
    (r["files"].asArray ?? []).compactMap { $0["name"].asString }
}

@Suite("file_bridge allow list (deny-all at the path level)")
struct FileAllowListTests {

    @Test func aBridgeWithNoListExposesNothing() async {
        let (k, dir) = await site(allow: nil)
        defer { try? FileManager.default.removeItem(at: dir) }
        for path in ["docs/a.pdf", "secret.txt", "missing.txt"] {
            let r = await read(k, path)
            #expect(r["reason"] == "not_found", "\(path): \(r)")
            #expect((r["error"].asString ?? "").contains("not exposed"), "\(r)")
        }
        #expect(await call(k, ["type": "list"])["reason"] == "not_found")
        let w = await k.send([
            Item(
                AgentId("fs"), .object(["type": "write_stream", "path": "new.txt"]),
                binary: Blob("bytes", Data("x".utf8)))
        ])[0].result
        #expect(w["reason"] == "not_found", "\(w)")
        #expect(!FileManager.default.fileExists(atPath: dir.appendingPathComponent("new.txt").path))
    }

    @Test func oneFileExposesThatFileAndTheWayToIt() async {
        let (k, dir) = await site(allow: ["docs/a.pdf"])
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(await read(k, "docs/a.pdf")["size"] == 6)
        #expect(await read(k, "docs/b.pdf")["reason"] == "not_found")
        #expect(await read(k, "secret.txt")["reason"] == "not_found")
        #expect(names(await call(k, ["type": "list"])) == ["docs"])
        #expect(names(await call(k, ["type": "list", "path": "docs"])) == ["a.pdf"])
    }

    @Test func aDirectoryExposesEverythingUnderItAndDotTheWholeRoot() async {
        let (k, dir) = await site(allow: ["docs"])
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(await read(k, "docs/b.pdf")["size"] == 6)
        #expect(await read(k, "secret.txt")["reason"] == "not_found")
        let (k2, dir2) = await site(allow: ["."])
        defer { try? FileManager.default.removeItem(at: dir2) }
        #expect(await read(k2, "secret.txt")["size"] == 8)
    }

    @Test func allowDisallowAndAllowedEditTheRecordOnePathAtATime() async {
        let (k, dir) = await site(allow: [])
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(
            await call(k, ["type": "allow", "path": "docs/a.pdf"])
                == .object(["path": "docs/a.pdf", "allowed": true, "count": 1]))
        #expect(await call(k, ["type": "allow", "path": "./docs//a.pdf"])["count"] == 1)
        _ = await call(k, ["type": "allow", "path": "docs/b.pdf"])
        #expect(k.agent(AgentId("fs"))?.metaValue(forKey: "allow") == ["docs/a.pdf", "docs/b.pdf"])
        #expect(await read(k, "docs/b.pdf")["size"] == 6)
        #expect(
            await call(k, ["type": "allowed", "limit": 1])
                == .object(["paths": ["docs/a.pdf"], "next": "docs/a.pdf", "total": 2]))
        let rest = await call(k, ["type": "allowed", "after": "docs/a.pdf"])
        #expect(rest["paths"] == ["docs/b.pdf"] && rest["next"] == .null)
        #expect(await call(k, ["type": "disallow", "path": "docs/b.pdf"])["count"] == 1)
        #expect(await read(k, "docs/b.pdf")["reason"] == "not_found")
        #expect(await call(k, ["type": "disallow", "path": "nope"])["reason"] == "not_found")
    }

    @Test func nothingOutsideTheRootCanBeAllowedOrRead() async {
        let (k, dir) = await site(allow: ["."])
        defer { try? FileManager.default.removeItem(at: dir) }
        for bad in ["../x", "docs/../../x", "/etc/hosts"] {
            #expect(await call(k, ["type": "allow", "path": .string(bad)])["reason"] == "bad_arg", "\(bad)")
        }
        let r = await read(k, "../../etc/passwd")
        #expect((r["error"].asString ?? "").contains("escape"), "the clamp answers first: \(r)")
    }

    @Test func theListIsValidatedAtWrite() {
        for bad: JSON in ["docs", ["../up"], [""], ["/abs"]] {
            #expect(FileBundle().validateMeta(["allow": bad], record: [:]) != nil, "\(bad)")
        }
        #expect(FileBundle().validateMeta(["allow": [".", "docs/a.pdf"]], record: [:]) == nil)
    }

    @Test func writesStayInsideTheListAndReadonlyStillHolds() async {
        let (k, dir) = await site(allow: ["docs"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let out = await call(k, ["type": "rename", "old_path": "docs/a.pdf", "new_path": "a.pdf"])
        #expect(out["reason"] == "not_found", "the target is outside the list: \(out)")
        #expect(await call(k, ["type": "delete", "path": "secret.txt"])["reason"] == "not_found")
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("secret.txt").path))
        #expect(await call(k, ["type": "mkdir", "path": "docs/sub"])["created"] == true)
    }

    // ── symlinks: BOTH ends must be exposed (audit 2026-09-28) ──────────────
    // The list was checked on the path AS ASKED while the fs follows links, so a
    // link inside an exposed dir served whatever in-root file it pointed at.

    private func linked(_ allow: JSON) async -> (Kernel, URL) {
        let (k, dir) = await site(allow: allow)
        let fm = FileManager.default
        try? fm.createSymbolicLink(
            atPath: dir.appendingPathComponent("docs/leak").path, withDestinationPath: "../secret.txt")
        try? fm.createSymbolicLink(
            atPath: dir.appendingPathComponent("docs/sib").path, withDestinationPath: "b.pdf")
        try? fm.createDirectory(at: dir.appendingPathComponent("shared"), withIntermediateDirectories: true)
        try? Data("c\n".utf8).write(to: dir.appendingPathComponent("shared/c.txt"))
        try? fm.createSymbolicLink(
            atPath: dir.appendingPathComponent("docs/shared").path, withDestinationPath: "../shared")
        return (k, dir)
    }

    @Test func aLinkInsideTheListToAFileOutsideItServesNothing() async {
        let (k, dir) = await linked(["docs"])
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(await read(k, "docs/leak")["reason"] == "not_found")
        let listed = names(await call(k, ["type": "list", "path": "docs"]))
        #expect(!listed.contains("leak") && !listed.contains("shared"), "\(listed)")
        let out = await call(k, ["type": "rename", "old_path": "docs/leak", "new_path": "docs/moved"])
        #expect(out["reason"] == "not_found", "\(out)")
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("secret.txt").path))
    }

    @Test func aLinkThatStaysInsideTheListStillServes() async {
        let (k, dir) = await linked(["docs"])
        defer { try? FileManager.default.removeItem(at: dir) }
        #expect(await read(k, "docs/sib")["size"] == 6)
    }

    @Test func anExposedLinkedDirListsWhatItServes() async {
        let (k, dir) = await linked(["docs", "shared"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let listed = await call(k, ["type": "list", "path": "docs/shared"])
        let paths = (listed["files"].asArray ?? []).compactMap { $0["path"].asString }
        #expect(paths == ["docs/shared/c.txt"], "\(listed)")
        #expect(await read(k, "docs/shared/c.txt")["size"] == 2)
    }

    // ─── granted allow, web serving, posture, running dir (audit d6/d7/d10/c6) ──

    /// d6 — `allow` is PRIVILEGED (a12): a NAMED grant reaches it; a `*` that the
    /// bridge's `callable_by` does not accept does not.
    @Test func anAgentGrantedAllowPublishesAFile() async {
        let (k, dir) = await site(allow: [])
        defer { try? FileManager.default.removeItem(at: dir) }
        k.update(AgentId("fs"), ["callable_by": ["ai": ["allow", "disallow", "allowed"]]])
        k.register(
            Agent(
                id: "ai", handlerModule: nil, parentId: "kernel_state",
                meta: ["can_call": ["fs": ["allow", "disallow", "allowed"]]]))
        k.register(
            Agent(
                id: "stranger", handlerModule: nil, parentId: "kernel_state",
                meta: ["can_call": ["fs": "*"]]))
        let r = await KernelTaskLocals.$currentSender.withValue(AgentId("ai")) {
            await call(k, ["type": "allow", "path": "docs/a.pdf"])
        }
        #expect(r["allowed"] == true, "\(r.serialize())")
        let s = await KernelTaskLocals.$currentSender.withValue(AgentId("stranger")) {
            await call(k, ["type": "allow", "path": "."])
        }
        #expect(s["reason"] == "unauthorized", "\(s.serialize())")
    }

    /// d6 — the web serves only what the list exposes (the file proxy runs AS
    /// `web`, so the bridge's own `callable_by` and `allow` both decide).
    @Test func theWebServesOnlyWhatTheListExposes() async throws {
        let kernel = try await startWebKernel()
        let fm = FileManager.default
        let rel = "fantastic-allowweb-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try fm.createDirectory(at: dir.appendingPathComponent("docs"), withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: dir) }
        try Data("%PDF-a".utf8).write(to: dir.appendingPathComponent("docs/a.pdf"))
        try Data("%PDF-b".utf8).write(to: dir.appendingPathComponent("docs/b.pdf"))
        try Data("private\n".utf8).write(to: dir.appendingPathComponent("secret.txt"))
        _ = await kernel.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state", "id": "fb",
                    "handler_module": "file_bridge.tools", "root": .string(rel),
                    "allow": ["docs/a.pdf"], "ingress_rule": "allow_all_ingress",
                    "callable_by": ["web": ["read_stream"]],
                ])
        ])
        kernel.update(AgentId("web"), ["can_call": ["fb": ["read_stream"]]])
        _ = await kernel.send([Item(AgentId("web"), ["type": "boot"])])
        defer { Task { _ = await kernel.send([Item(AgentId("web"), ["type": "stop"])]) } }
        let port = kernel.httpPort()
        let session = URLSession(configuration: .ephemeral)
        func get(_ path: String) async throws -> (Int, Data) {
            let (data, resp) = try await session.data(
                from: URL(string: "http://127.0.0.1:\(port)/fb/file/\(path)")!)
            return ((resp as! HTTPURLResponse).statusCode, data)
        }
        let (code, body) = try await get("docs/a.pdf")
        #expect(code == 200 && body == Data("%PDF-a".utf8))
        for path in ["docs/b.pdf", "secret.txt", "missing.txt"] {
            #expect(try await get(path).0 == 404, "\(path)")
        }
    }

    /// d7 — `allow` is posture: what a file_bridge exposes is readable from the
    /// ONE root reflect (`detail:full`), like its root and its lock.
    @Test func theRootReflectCarriesABridgesAllow() async {
        let (k, dir) = await site(allow: ["docs", "secret.txt"])
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await k.send([Item("kernel_state", ["type": "reflect", "detail": "full"])])[0].result
        let node = (r["tree"]["children"].asArray ?? []).first { $0["id"] == "fs" }
        #expect(node?["allow"] == ["docs", "secret.txt"], "\(r["tree"].serialize())")
        #expect(node?["file_bridge_id"].isNull == true)
    }

    /// d10 — the TEXT-channel `read_stream` reply carries `origin`, as py/rs do.
    @Test func theTextChannelReadStreamCarriesOrigin() async {
        let (k, dir) = await site(allow: ["."])
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await read(k, "docs/a.pdf")
        #expect(Array(r.asObject?.keys ?? []) == [
            "path", "offset", "bytes", "next_offset", "eof", "size", "origin",
        ], "\(r.serialize())")
        #expect(r["origin"].asString?.hasPrefix("fs docs/a.pdf bytes 0-6 of 6") == true)
    }

    /// c6 — the clamp base is the kernel's NAMED running dir (the dir holding its
    /// root's `.fantastic`), never the process cwd; a root that named none refuses.
    @Test func theClampBaseIsTheNamedRunningDir() async {
        let fm = FileManager.default
        let work = fm.temporaryDirectory.appendingPathComponent("fantastic-c6-\(UUID().uuidString)")
        try? fm.createDirectory(at: work, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: work) }
        func kernel(rootPath: URL?) async -> Kernel {
            let registry = BundleRegistry()
            registry.register("file_bridge.tools", FileBundle())
            let k = Kernel(storage: .inMemory, bundles: registry)
            let root =
                rootPath.map { Agent(id: "kernel_state", handlerModule: nil, parentId: nil, rootPath: $0) }
                ?? Agent(id: "kernel_state", handlerModule: nil, parentId: nil)
            k.register(root)
            k.setRoot(root)
            _ = await k.send([
                Item(
                    "kernel_state",
                    [
                        "type": "create_agent", "parent_id": "kernel_state", "id": "fs",
                        "handler_module": "file_bridge.tools", "root": "d", "allow": ["."],
                        "ingress_rule": "allow_all_ingress",
                    ])
            ])
            return k
        }
        let named = await kernel(rootPath: work.appendingPathComponent(".fantastic"))
        let w = await named.send([
            Item(AgentId("fs"), ["type": "write", "path": "f.txt", "bytes": "x"])
        ])[0].result
        #expect(w["size"] == 1, "\(w.serialize())")
        #expect(fm.fileExists(atPath: work.appendingPathComponent("d/f.txt").path))
        let unnamed = await kernel(rootPath: nil)
        let r = await unnamed.send([Item(AgentId("fs"), ["type": "list"])])[0].result
        #expect(r["reason"] == "transport_error", "\(r.serialize())")
        #expect(
            r["error"] == "kernel has no root — no running dir to clamp disk to", "\(r.serialize())")
    }
}
