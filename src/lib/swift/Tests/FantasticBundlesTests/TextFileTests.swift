// text_file — the stateless text-document agent, every byte a GATED send.
//
// The patch cases are the file_bridge `patch_text_file` suite, ported to the
// send-path: the editor never touches disk; it sends stream verbs to the
// file_bridge named by its `target_id`. Mirrors the python suite (the
// immutable reference, `bundled_agents/text_file/tests/`).

import FantasticFile
import FantasticJSON
import FantasticKernel
import FantasticTextFile
import Foundation
import OrderedCollections
import Testing

@Suite("text_file (editor over the gated stream)")
struct TextFileTests {

    /// A file_bridge rooted at a fresh cwd-relative dir (running-dir law) + an
    /// editor wired to it, both gate ends granted. Returns (kernel, dir).
    private func openEditor(sealed: Bool = false) async -> (Kernel, URL) {
        let registry = BundleRegistry()
        registry.register("file_bridge.tools", FileBundle())
        registry.register("text_file.tools", TextFileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let fm = FileManager.default
        let rel = "fantastic-textfile-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        var meta: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "core",
            "handler_module": "file_bridge.tools", "id": "fs", "root": .string(rel),
            "callable_by": .object(["ed": .string("*")]),
        ]
        if !sealed { meta["ingress_rule"] = "allow_all_ingress" }
        _ = await kernel.send([Item("core", .object(meta))])[0].result
        _ = await kernel.send([Item("core", .object([
                "type": "create_agent", "parent_id": "core",
                "handler_module": "text_file.tools", "id": "ed",
                "target_id": .string("fs"),
                "can_call": .object(["fs": .string("*")]),
            ]))])[0].result
        return (kernel, dir)
    }

    private func callBin(_ k: Kernel, _ header: JSON, _ blob: String) async -> JSON {
        await k.sendWithBinary(AgentId("ed"), header, Data(blob.utf8)).0
    }

    private func call(_ k: Kernel, _ payload: JSON) async -> JSON {
        await k.send([Item(AgentId("ed"), payload)])[0].result
    }

    private func seed(_ k: Kernel, _ path: String, _ text: String) async -> JSON {
        await callBin(k, .object(["type": .string("write"), "path": .string(path)]), text)
    }

    private func block(_ old: String, _ new: String) -> String {
        "<<<<<<< SEARCH\n\(old)\n=======\n\(new)\n>>>>>>> REPLACE"
    }

    private func read(_ dir: URL, _ path: String) -> String {
        (try? String(contentsOf: dir.appendingPathComponent(path), encoding: .utf8)) ?? "<none>"
    }

    // MARK: - patch, ported

    @Test func patchSingleHunkAppliesAndWrites() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "m.py", "a = 1\nb = 2\nc = 3\n")
        let r = await callBin(
            k, .object(["type": .string("patch"), "path": .string("m.py")]),
            block("b = 2", "b = 20"))
        #expect(r["patched"].asBool == true)
        #expect(r["hunks"].asInt == 1)
        #expect(read(dir, "m.py") == "a = 1\nb = 20\nc = 3\n")
    }

    @Test func patchMultiHunkEvolvingAndMultiline() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "m.py", "def f():\n    return 1\n\ndef g():\n    return 2\n")
        let p =
            block("def f():\n    return 1", "def f():\n    return 99") + "\n"
            + block("return 2", "return 22")
        let r = await callBin(
            k, .object(["type": .string("patch"), "path": .string("m.py")]), p)
        #expect(r["patched"].asBool == true && r["hunks"].asInt == 2)
        #expect(read(dir, "m.py") == "def f():\n    return 99\n\ndef g():\n    return 22\n")
    }

    @Test func patchFailuresAreLoudAndWriteNothing() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "m.py", "v = 1\nv = 1\n")
        var r = await callBin(
            k, .object(["type": .string("patch"), "path": .string("m.py")]),
            block("v = 1", "v = 2"))
        #expect(r["reason"].asString == "ambiguous")
        let p = block("v = 1\nv = 1", "w = 1") + "\n" + block("missing", "x")
        r = await callBin(k, .object(["type": .string("patch"), "path": .string("m.py")]), p)
        #expect(r["reason"].asString == "no_match")
        #expect(r["failed"].asArray?.first?["index"].asInt == 1)
        #expect(read(dir, "m.py") == "v = 1\nv = 1\n")
        r = await callBin(
            k, .object(["type": .string("patch"), "path": .string("m.py")]),
            "<<<<<<< SEARCH\nv = 1\n=======\nv = 2\n")
        #expect(r["reason"].asString == "bad_arg")
        r = await callBin(
            k, .object(["type": .string("patch"), "path": .string("m.py")]), "no markers")
        #expect(r["reason"].asString == "bad_arg")
        r = await callBin(k, .object(["type": .string("patch")]), block("a", "b"))
        #expect(r["reason"].asString == "missing_arg")
        r = await callBin(
            k, .object(["type": .string("patch"), "path": .string("nope.py")]),
            block("a", "b"))
        #expect(r["reason"].asString == "not_found")
        #expect(read(dir, "m.py") == "v = 1\nv = 1\n")
    }

    @Test func sealedLegDeniesAndEditorPassesItThrough() async {
        let (k, dir) = await openEditor(sealed: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try? "a = 1\n".write(
            to: dir.appendingPathComponent("m.py"), atomically: true, encoding: .utf8)
        let r = await callBin(
            k, .object(["type": .string("patch"), "path": .string("m.py")]),
            block("a = 1", "a = 2"))
        #expect(r["reason"].asString == "unauthorized")
        #expect(read(dir, "m.py") == "a = 1\n")
    }

    // MARK: - read / write / append / version / if_match

    @Test func writeThenReadLinesAndVersion() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let w = await seed(k, "doc.md", "alpha\nbeta\ngamma\n")
        #expect(w["size"].asInt == 17)
        #expect(w["version"].asString?.count == 16)
        var r = await call(k, .object(["type": .string("read"), "path": .string("doc.md")]))
        let texts = (r["lines"].asArray ?? []).compactMap { $0["text"].asString }
        #expect(texts == ["alpha", "beta", "gamma"])
        #expect(r["lines"].asArray?.first?["n"].asInt == 1)
        #expect(r["total"].asInt == 3 && r["eof"].asBool == true)
        #expect(r["version"].asString == w["version"].asString)
        r = await call(
            k,
            .object([
                "type": .string("read"), "path": .string("doc.md"),
                "offset": .integer(1), "limit": .integer(1),
            ]))
        #expect(r["lines"].asArray?.first?["text"].asString == "beta")
        #expect(r["lines"].asArray?.first?["n"].asInt == 2)
        #expect(r["eof"].asBool == false)
    }

    @Test func appendCreatesThenGrowsAndNoTmpDebris() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r1 = await callBin(
            k, .object(["type": .string("append"), "path": .string("log.txt")]), "one\n")
        #expect(r1["size"].asInt == 4)
        let r2 = await callBin(
            k, .object(["type": .string("append"), "path": .string("log.txt")]), "two\n")
        #expect(r2["size"].asInt == 8)
        #expect(read(dir, "log.txt") == "one\ntwo\n")
        #expect(r1["version"].asString != r2["version"].asString)
        _ = await seed(k, "a.txt", "x\n")
        let names =
            (try? FileManager.default.contentsOfDirectory(atPath: dir.path)) ?? []
        #expect(!names.contains { $0.contains(".tmp.") }, "tmp debris: \(names)")
    }

    @Test func ifMatchGuardsWriteAndPatch() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let w = await seed(k, "v.txt", "one\n")
        let stale = String(repeating: "0", count: 16)
        var r = await callBin(
            k,
            .object([
                "type": .string("write"), "path": .string("v.txt"),
                "if_match": .string(stale),
            ]), "two\n")
        #expect(r["reason"].asString == "version_mismatch")
        #expect(r["actual"].asString == w["version"].asString)
        #expect(read(dir, "v.txt") == "one\n")
        r = await callBin(
            k,
            .object([
                "type": .string("write"), "path": .string("v.txt"),
                "if_match": w["version"],
            ]), "two\n")
        #expect(r["version"].asString != w["version"].asString)
        r = await callBin(
            k,
            .object([
                "type": .string("patch"), "path": .string("v.txt"),
                "if_match": .string(stale),
            ]), block("two", "three"))
        #expect(r["reason"].asString == "version_mismatch")
        r = await callBin(
            k,
            .object([
                "type": .string("patch"), "path": .string("v.txt"),
                "dry_run": .bool(true),
            ]), block("two", "three"))
        #expect(r["dry_run"].asBool == true)
        #expect(read(dir, "v.txt") == "two\n")
    }

    // MARK: - checkpoints

    @Test func checkpointedWriteRestoresByteIdentical() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "c.txt", "original\n")
        let w = await callBin(
            k,
            .object([
                "type": .string("write"), "path": .string("c.txt"),
                "checkpoint": .bool(true),
            ]), "mutated\n")
        let ck = w["ckpt_id"].asString ?? ""
        #expect(ck.contains(":"), "task pid shape: \(w)")
        #expect(read(dir, "c.txt") == "mutated\n")
        let r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("c.txt"),
                "ckpt_id": .string(ck),
            ]))
        #expect(r["restored"].asBool == true)
        #expect(read(dir, "c.txt") == "original\n")
    }

    @Test func checkpointOfAbsentFileRestoresToAbsence() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let w = await callBin(
            k,
            .object([
                "type": .string("write"), "path": .string("new.txt"),
                "checkpoint": .bool(true),
            ]), "born\n")
        let ck = w["ckpt_id"].asString ?? ""
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("new.txt").path))
        let r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("new.txt"),
                "ckpt_id": .string(ck),
            ]))
        #expect(r["restored"].asBool == true && r["deleted"].asBool == true)
        #expect(
            !FileManager.default.fileExists(atPath: dir.appendingPathComponent("new.txt").path))
    }

    @Test func checkpointListDropRestoreLifecycle() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "p.txt", "a = 1\n")
        let c = await call(
            k, .object(["type": .string("checkpoint"), "path": .string("p.txt")]))
        let ck = c["ckpt_id"].asString ?? ""
        _ = await callBin(
            k, .object(["type": .string("patch"), "path": .string("p.txt")]),
            block("a = 1", "a = 2"))
        var ls = await call(
            k, .object(["type": .string("checkpoints"), "path": .string("p.txt")]))
        #expect((ls["ckpts"].asArray ?? []).compactMap { $0["id"].asString } == [ck])
        var r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("p.txt"),
                "ckpt_id": .string(ck),
            ]))
        #expect(r["version"].asString == c["version"].asString)
        #expect(read(dir, "p.txt") == "a = 1\n")
        r = await call(
            k,
            .object([
                "type": .string("drop"), "path": .string("p.txt"), "ckpt_id": .string(ck),
            ]))
        #expect(r["dropped"].asBool == true)
        ls = await call(
            k, .object(["type": .string("checkpoints"), "path": .string("p.txt")]))
        #expect((ls["ckpts"].asArray ?? []).isEmpty)
        r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("p.txt"),
                "ckpt_id": .string(ck),
            ]))
        #expect(r["reason"].asString == "not_found")
    }

    // MARK: - unwired failfast + reflect

    @Test func unwiredEditorFailsLoudOnEveryVerb() async {
        let registry = BundleRegistry()
        registry.register("text_file.tools", TextFileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("core"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([Item("core", .object([
                "type": "create_agent", "parent_id": "core",
                "handler_module": "text_file.tools", "id": "bare",
            ]))])[0].result
        for verb in ["read", "checkpoint", "checkpoints", "restore", "drop"] {
            let r = await kernel.send([
                Item(AgentId("bare"), .object(["type": .string(verb), "path": .string("x")]))
            ])[0].result
            #expect(r["reason"].asString == "not_found", "\(verb): \(r)")
            #expect(r["error"].asString?.contains("target") == true, "\(verb): \(r)")
        }
        for verb in ["write", "patch", "append"] {
            let (r, _) = await kernel.sendWithBinary(
                AgentId("bare"),
                .object(["type": .string(verb), "path": .string("x")]), Data("y".utf8))
            #expect(r["reason"].asString == "not_found", "\(verb): \(r)")
        }
    }

    @Test func reflectNamesSurfaceGrammarAndByteVerbsRedirect() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, .object(["type": .string("reflect")]))
        #expect(r["id"].asString == "ed")
        #expect(r["target_id"].asString == "fs")
        #expect(r["patch_grammar"].asString?.contains("SEARCH") == true)
        for v in ["read", "write", "patch", "append", "checkpoint", "restore", "drop"] {
            #expect(!r["verbs"][v].isNull, "verb \(v) missing")
        }
        let t = await call(
            k, .object(["type": .string("write"), "path": .string("x")]))
        #expect(t["error"].asString?.contains("binary channel") == true)
    }
}
