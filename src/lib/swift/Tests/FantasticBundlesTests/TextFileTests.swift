// text_file — the stateless text-document agent, every byte a GATED send.
//
// The patch cases are the file_bridge `patch_text_file` suite, ported to the
// send-path: the editor never touches disk; it sends stream verbs to the
// file_bridge named by its `target_id`. Mirrors the python suite (the
// immutable reference, `bundled_agents/text_file/tests/`).

import CryptoKit
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
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let fm = FileManager.default
        let rel = "fantastic-textfile-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        var meta: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "kernel_state",
            "handler_module": "file_bridge.tools", "id": "fs", "root": .string(rel),
            "callable_by": .object(["ed": .string("*")]),
        ]
        if !sealed { meta["ingress_rule"] = "allow_all_ingress" }
        _ = await kernel.send([Item("kernel_state", .object(meta))])[0].result
        _ = await kernel.send([Item("kernel_state", .object([
                "type": "create_agent", "parent_id": "kernel_state",
                "handler_module": "text_file.tools", "id": "ed",
                "target_id": .string("fs"),
                "can_call": .object(["fs": .string("*")]),
            ]))])[0].result
        return (kernel, dir)
    }

    /// The BATCH binary item, not `sendWithBinary`: the batch is what mints the
    /// task row and the pid, and a checkpoint id IS that pid (python has no
    /// batch-free binary path at all).
    private func callBin(_ k: Kernel, _ header: JSON, _ blob: String) async -> JSON {
        await k.send([Item(AgentId("ed"), header, binary: Data(blob.utf8))])[0].result
    }

    private func call(_ k: Kernel, _ payload: JSON) async -> JSON {
        await k.send([Item(AgentId("ed"), payload)])[0].result
    }

    /// C37 — EVERY LEG BYTE IS AN EXECUTION. The two seam bodies
    /// (`legReadChunk`/`legWriteChunk`) send ONE binary item through the batch
    /// instead of `sendWithBinary`, so each leg read and each leg write leaves a
    /// call row on the leg and a settle row on the root, exactly like python's
    /// `kernel.send([...])`. `sendWithBinary` minted none of it, so the bytes a
    /// model moved left no trace the checkpoint ids could be read back from.
    @Test func legCallsAreBatchRows() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let w = await callBin(
            k, .object(["type": .string("write"), "path": .string("a.txt")]), "hello\n")
        #expect(w["error"].isNull, "\(w)")
        let rows = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("rows_read"), "agent": .string("fs"),
                    "limit": .integer(200),
                ]))
        ])[0].result
        let calls = (rows["rows"].asArray ?? []).filter { $0["kind"].asString == "call" }
        let verbs = calls.compactMap { $0["verb"].asString }
        #expect(verbs.contains("write_stream"), "no leg call row: \(rows.serialize())")
        #expect(verbs.contains("rename"), "no leg rename row: \(rows.serialize())")
        // Each one SETTLED — the outcome is stamped on the call row.
        #expect(
            calls.allSatisfy { $0["outcome"]["status"].asString != nil },
            "a leg call never settled: \(rows.serialize())")
        // …and the read half too.
        let r = await call(k, .object(["type": .string("read"), "path": .string("a.txt")]))
        #expect(r["error"].isNull, "\(r)")
        let after = await k.send([
            Item(
                AgentId("kernel"),
                .object([
                    "type": .string("rows_read"), "agent": .string("fs"),
                    "limit": .integer(200),
                ]))
        ])[0].result
        #expect(
            (after["rows"].asArray ?? []).contains {
                $0["kind"].asString == "call" && $0["verb"].asString == "read_stream"
            }, "no leg read row: \(after.serialize())")
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
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("m.py"), "hunks": .integer(1)])])]),
            block("b = 2", "b = 20"))
        #expect(r["patched"].asBool == true)
        #expect(r["edits"][0]["hunks"].asInt == 1)
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
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("m.py"), "hunks": .integer(2)])])]), p)
        #expect(r["patched"].asBool == true && r["edits"][0]["hunks"].asInt == 2)
        #expect(read(dir, "m.py") == "def f():\n    return 99\n\ndef g():\n    return 22\n")
    }

    @Test func patchFailuresAreLoudAndWriteNothing() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "m.py", "v = 1\nv = 1\n")
        var r = await callBin(
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("m.py"), "hunks": .integer(1)])])]),
            block("v = 1", "v = 2"))
        #expect(r["reason"].asString == "ambiguous")
        let p = block("v = 1\nv = 1", "w = 1") + "\n" + block("missing", "x")
        r = await callBin(k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("m.py"), "hunks": .integer(2)])])]), p)
        #expect(r["reason"].asString == "no_match")
        #expect(r["failed"].asArray?.first?["index"].asInt == 1)
        #expect(read(dir, "m.py") == "v = 1\nv = 1\n")
        r = await callBin(
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("m.py"), "hunks": .integer(1)])])]),
            "<<<<<<< SEARCH\nv = 1\n=======\nv = 2\n")
        #expect(r["reason"].asString == "bad_arg")
        r = await callBin(
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("m.py"), "hunks": .integer(1)])])]), "no markers")
        #expect(r["reason"].asString == "bad_arg")
        r = await callBin(k, .object(["type": .string("patch")]), block("a", "b"))
        #expect(r["reason"].asString == "missing_arg")
        r = await callBin(
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("nope.py"), "hunks": .integer(1)])])]),
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
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("m.py"), "hunks": .integer(1)])])]),
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
                "type": .string("patch"),
                "edits": .array([.object([
                    "path": .string("v.txt"), "hunks": .integer(1),
                    "if_match": .string(stale),
                ])]),
            ]), block("two", "three"))
        #expect(r["reason"].asString == "version_mismatch")
        r = await callBin(
            k,
            .object([
                "type": .string("patch"), "dry_run": .bool(true),
                "edits": .array([.object([
                    "path": .string("v.txt"), "hunks": .integer(1),
                ])]),
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
        // A ckpt id IS the pid — an int from the one counter (python `_pid`).
        let ck = w["ckpt_id"]
        #expect(w["ckpt_id"].asInt != nil, "ckpt id is a pid (int): \(w)")
        #expect(read(dir, "c.txt") == "mutated\n")
        let r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("c.txt"),
                "ckpt_id": ck,
            ]))
        #expect(r["restored"].asBool == true)
        #expect(read(dir, "c.txt") == "original\n")
    }

    /// A ckpt id IS a pid, and a pid becomes a NUMBER — so a model writing
    /// `<ckpt_id>29</ckpt_id>` hands over an integer, and reading it as a string
    /// only refuses the model its own checkpoint back. Measured 2026-09-10 on
    /// `undo:rust`: twenty consecutive "restore: ckpt_id required".
    @Test func restoreTakesTheCkptIdAsTheNotationTypedIt() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "c.txt", "before\n")
        let w = await callBin(
            k,
            .object([
                "type": .string("write"), "path": .string("c.txt"),
                "checkpoint": .bool(true),
            ]), "after\n")
        let ck = w["ckpt_id"]
        #expect(read(dir, "c.txt") == "after\n")
        // The model writes the id it was shown; the notation types it as an int
        // whenever it looks like one.
        let typed: JSON = ck
        let r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("c.txt"), "ckpt_id": typed,
            ]))
        #expect(r["error"].asString == nil, "\(r.serialize())")
        #expect(read(dir, "c.txt") == "before\n")
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
        let ck = w["ckpt_id"]
        #expect(FileManager.default.fileExists(atPath: dir.appendingPathComponent("new.txt").path))
        let r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("new.txt"),
                "ckpt_id": ck,
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
        let ck = c["ckpt_id"]
        _ = await callBin(
            k, .object(["type": .string("patch"), "edits": .array([.object(["path": .string("p.txt"), "hunks": .integer(1)])])]),
            block("a = 1", "a = 2"))
        var ls = await call(
            k, .object(["type": .string("checkpoints"), "path": .string("p.txt")]))
        #expect((ls["ckpts"].asArray ?? []).map { $0["id"] } == [ck])
        var r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("p.txt"),
                "ckpt_id": ck,
            ]))
        #expect(r["version"].asString == c["version"].asString)
        #expect(read(dir, "p.txt") == "a = 1\n")
        r = await call(
            k,
            .object([
                "type": .string("drop"), "path": .string("p.txt"), "ckpt_id": ck,
            ]))
        #expect(r["dropped"].asBool == true)
        ls = await call(
            k, .object(["type": .string("checkpoints"), "path": .string("p.txt")]))
        #expect((ls["ckpts"].asArray ?? []).isEmpty)
        r = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("p.txt"),
                "ckpt_id": ck,
            ]))
        #expect(r["reason"].asString == "not_found")
    }

    // MARK: - unwired failfast + reflect

    @Test func unwiredEditorFailsLoudOnEveryVerb() async {
        let registry = BundleRegistry()
        registry.register("text_file.tools", TextFileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([Item("kernel_state", .object([
                "type": "create_agent", "parent_id": "kernel_state",
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
            let r = await kernel.send([
                Item(
                    AgentId("bare"), .object(["type": .string(verb), "path": .string("x")]),
                    binary: Data("y".utf8))
            ])[0].result
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
        #expect(t["reason"].asString == "missing_arg")
        #expect(t["arg"].asString?.hasPrefix("bytes") == true)
    }

    // MARK: - the TRANSACTION (multi-file edits in ONE patch)

    @Test func multifilePatchAppliesBothAtomically() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.py", "x = 1\n")
        _ = await seed(k, "b.py", "y = 2\nz = 3\n")
        let blob =
            block("x = 1", "x = 10") + "\n" + block("y = 2", "y = 20") + "\n"
            + block("z = 3", "z = 30")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([
                    .object(["path": .string("a.py"), "hunks": .integer(1)]),
                    .object(["path": .string("b.py"), "hunks": .integer(2)]),
                ]),
            ]), blob)
        #expect(r["patched"].asBool == true, "\(r)")
        #expect(r["edits"][1]["hunks"].asInt == 2)
        #expect(read(dir, "a.py") == "x = 10\n")
        #expect(read(dir, "b.py") == "y = 20\nz = 30\n")
        // committed transaction self-cleans its pre-images
        let ck = dir.appendingPathComponent(".ckpt")
        let files = (FileManager.default.enumerator(atPath: ck.path)?.allObjects ?? [])
            .compactMap { $0 as? String }
            .filter { !$0.hasSuffix("/") && !ck.appendingPathComponent($0).hasDirectoryPath }
        #expect(files.isEmpty, "pre-image debris: \(files)")
    }

    @Test func multifileDoomedSecondEditWritesNothing() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.py", "x = 1\n")
        _ = await seed(k, "b.py", "y = 2\n")
        let blob = block("x = 1", "x = 10") + "\n" + block("missing", "nope")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([
                    .object(["path": .string("a.py"), "hunks": .integer(1)]),
                    .object(["path": .string("b.py"), "hunks": .integer(1)]),
                ]),
            ]), blob)
        #expect(r["reason"].asString == "no_match", "\(r)")
        #expect(r["failed"].asArray?.first?["path"].asString == "b.py")
        #expect(read(dir, "a.py") == "x = 1\n")
        #expect(read(dir, "b.py") == "y = 2\n")
    }

    @Test func hunkCountsMustSumToTheBlob() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.py", "x = 1\n")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([.object(["path": .string("a.py"), "hunks": .integer(2)])]),
            ]), block("x = 1", "x = 2"))
        #expect(r["reason"].asString == "bad_arg", "\(r)")
        #expect(r["error"].asString?.contains("sum") == true)
        #expect(read(dir, "a.py") == "x = 1\n")
    }

    @Test func multifileCheckpointTrueKeepsPreimages() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.py", "one\n")
        _ = await seed(k, "b.py", "two\n")
        let blob = block("one", "ONE") + "\n" + block("two", "TWO")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"), "checkpoint": .bool(true),
                "edits": .array([
                    .object(["path": .string("a.py"), "hunks": .integer(1)]),
                    .object(["path": .string("b.py"), "hunks": .integer(1)]),
                ]),
            ]), blob)
        #expect(r["patched"].asBool == true, "\(r)")
        let cks = r["ckpt_ids"].asArray ?? []
        #expect(cks.count == 2)
        for row in cks {
            let rr = await call(
                k,
                .object([
                    "type": .string("restore"), "path": row["path"],
                    "ckpt_id": row["ckpt_id"],
                ]))
            #expect(rr["restored"].asBool == true, "\(rr)")
        }
        #expect(read(dir, "a.py") == "one\n")
        #expect(read(dir, "b.py") == "two\n")
    }

    @Test func perEditIfMatchGuardsTheWholeTransaction() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.py", "one\n")
        _ = await seed(k, "b.py", "two\n")
        let blob = block("one", "ONE") + "\n" + block("two", "TWO")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([
                    .object(["path": .string("a.py"), "hunks": .integer(1)]),
                    .object([
                        "path": .string("b.py"), "hunks": .integer(1),
                        "if_match": .string(String(repeating: "0", count: 16)),
                    ]),
                ]),
            ]), blob)
        #expect(r["reason"].asString == "version_mismatch", "\(r)")
        #expect(r["path"].asString == "b.py")
        #expect(read(dir, "a.py") == "one\n")
    }

    // MARK: - per-document serialization + payload hardening (sweep I-5)

    private func sha16(_ text: String) -> String {
        let d = SHA256.hash(data: Data(text.utf8))
        return d.map { String(format: "%02x", $0) }.joined().prefix(16).lowercased()
    }

    @Test func patchRefusesTheSamePathTwice() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.md", "one\ntwo\n")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([
                    .object(["path": .string("a.md"), "hunks": .integer(1)]),
                    .object(["path": .string("a.md"), "hunks": .integer(1)]),
                ]),
            ]), block("one", "1") + "\n" + block("two", "2"))
        #expect(r["reason"].asString == "bad_arg")
        #expect(r["arg"].asString == "edits")
        #expect(r["error"].asString?.contains("at most once") == true)
        #expect(read(dir, "a.md") == "one\ntwo\n")
    }

    @Test func concurrentPatchesOnOneFileBothLand() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "f.py", "a = 1\nb = 2\n")
        let header: JSON = .object([
            "type": .string("patch"),
            "edits": .array([.object(["path": .string("f.py"), "hunks": .integer(1)])]),
        ])
        async let one = callBin(k, header, block("a = 1", "a = 10"))
        async let two = callBin(k, header, block("b = 2", "b = 20"))
        let (r1, r2) = await (one, two)
        #expect(r1["patched"].asBool == true, "\(r1)")
        #expect(r2["patched"].asBool == true, "\(r2)")
        let final = read(dir, "f.py")
        #expect(final == "a = 10\nb = 20\n")
        // each reply's version is the document as it stood at THAT commit: one
        // is the final image, the other the intermediate with only its own hunk.
        let vFinal = sha16(final)
        let versions = [r1["edits"][0]["version"].asString ?? "", r2["edits"][0]["version"].asString ?? ""]
        #expect(versions.contains(vFinal), "\(versions) vs \(vFinal)")
        let intermediates = [sha16("a = 10\nb = 2\n"), sha16("a = 1\nb = 20\n")]
        for v in versions {
            #expect(v == vFinal || intermediates.contains(v), "\(v)")
        }
    }

    /// A3.1 — the byte verbs name their OWN `bytes` argument, and the guard sits
    /// inside the verb (python `tools.py:358-372,392-402,467-477`), never in
    /// `handle`: the order is unwired → path/edits → bytes.
    @Test func textChannelBytesGuardIsMissingArg() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let cases: [(JSON, String)] = [
            (
                .object(["type": .string("write"), "path": .string("x")]),
                "bytes (the document — str is utf-8-encoded)"
            ),
            (
                .object(["type": .string("append"), "path": .string("x")]),
                "bytes (one chunk — str is utf-8-encoded)"
            ),
            (
                .object([
                    "type": .string("patch"),
                    "edits": .array([.object(["path": .string("x"), "hunks": .integer(1)])]),
                ]),
                "bytes (the utf-8 patch — all hunks, in edits order)"
            ),
        ]
        for (payload, arg) in cases {
            let r = await call(k, payload)
            #expect(r["reason"].asString == "missing_arg", "\(payload.serialize()): \(r)")
            #expect(r["arg"].asString == arg, "\(payload.serialize()): \(r)")
            #expect(r["error"].asString?.hasSuffix(": \(arg) required") == true, "\(r)")
        }
    }

    /// A3.1 — the precheck ORDER: a bytes-less `write` on an UNWIRED editor is
    /// `not_found`, not `missing_arg`, and a wired one complains about `path`
    /// before `bytes`. Python refuses in verb order; `handle` refuses nothing.
    @Test func bytesGuardComesAfterUnwiredAndPath() async {
        let registry = BundleRegistry()
        registry.register("text_file.tools", TextFileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "text_file.tools", "id": "bare",
                ]))
        ])[0].result
        for verb in ["write", "patch", "append"] {
            let r = await kernel.send([
                Item(AgentId("bare"), .object(["type": .string(verb)]))
            ])[0].result
            #expect(r["reason"].asString == "not_found", "\(verb): \(r)")
        }
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        for verb in ["write", "append"] {
            let r = await call(k, .object(["type": .string(verb)]))
            #expect(r["arg"].asString == "path", "\(verb): \(r)")
        }
        let p = await call(k, .object(["type": .string("patch")]))
        #expect(p["arg"].asString == "edits (a list of {path, hunks, if_match?})", "\(p)")
    }

    /// A3.2 — python's `VERBS` has no `shutdown`, so it is an unknown type like
    /// any typo (it used to answer a silent `null`).
    @Test func shutdownIsUnhandled() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, .object(["type": .string("shutdown")]))
        #expect(r["reason"].asString == "unhandled", "\(r)")
        #expect(r["error"].asString == "text_file: unknown type 'shutdown'", "\(r)")
        #expect(r["verb"].asString == "shutdown", "\(r)")
    }

    /// A3.3 — every quoted value in a refusal is python's `repr`: single quotes
    /// on a str, bare on an int.
    @Test func refusalTextsQuoteLikePythonRepr() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "c.txt", "keep\n")
        let unknown = await call(k, .object(["type": .string("frobnicate")]))
        #expect(unknown["error"].asString == "text_file: unknown type 'frobnicate'", "\(unknown)")
        let noCkpt = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("c.txt"),
                "ckpt_id": .integer(999_999),
            ]))
        #expect(
            noCkpt["error"].asString == "restore: no checkpoint 999999 for 'c.txt'", "\(noCkpt)")
        #expect(noCkpt["reason"].asString == "not_found", "\(noCkpt)")
        let hunks = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([.object(["path": .string("a.py"), "hunks": .string("2")])]),
            ]), block("x", "y"))
        #expect(
            hunks["error"].asString == "patch: edits must be edit 0 ('a.py'): `hunks` (int >= 1)",
            "\(hunks)")
        // a document that is not utf-8 names itself with `repr`, too
        try? Data([0xff, 0xfe, 0xff]).write(to: dir.appendingPathComponent("b.bin"))
        let binary = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([.object(["path": .string("b.bin"), "hunks": .integer(1)])]),
            ]), block("x", "y"))
        #expect(
            binary["error"].asString == "patch: edits must be 'b.bin': a utf-8 text file",
            "\(binary)")
    }

    /// A3.4 — every edit is validated (python has no `break`), so the reported
    /// sum is the WHOLE claim, not the prefix that happened to fit.
    @Test func hunkClaimSumsEveryEdit() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.py", "one\n")
        _ = await seed(k, "b.py", "two\n")
        let blob = block("one", "ONE") + "\n" + block("two", "TWO")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([
                    .object(["path": .string("a.py"), "hunks": .integer(3)]),
                    .object(["path": .string("b.py"), "hunks": .integer(1)]),
                ]),
            ]), blob)
        #expect(
            r["error"].asString == "patch: edits must be hunk counts that sum to the blob's "
                + "blocks — edits claim 4, the patch holds 2", "\(r)")
        #expect(read(dir, "a.py") == "one\n")
    }

    /// A3.5 — the per-edit mismatch body puts `path` between `reason` and
    /// `expected` (python `tools.py:544-550`).
    @Test func perEditVersionMismatchKeyOrder() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "a.py", "one\n")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([
                    .object([
                        "path": .string("a.py"), "hunks": .integer(1),
                        "if_match": .string(String(repeating: "0", count: 16)),
                    ])
                ]),
            ]), block("one", "ONE"))
        #expect(
            Array(r.asObject?.keys ?? []) == ["error", "reason", "path", "expected", "actual"],
            "\(r.serialize())")
    }

    /// A3.6 — `validate_meta` refuses a `target_id` this bundle cannot use.
    @Test func emptyTargetIdIsInvalidMeta() async {
        let registry = BundleRegistry()
        registry.register("text_file.tools", TextFileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        let r = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "text_file.tools", "id": "ed2",
                    "target_id": .string(""),
                ]))
        ])[0].result
        #expect(r["reason"].asString == "invalid_meta", "\(r.serialize())")
        #expect(
            r["error"].asString == "target_id must be a non-empty file_bridge agent id",
            "\(r.serialize())")
    }

    /// A3.7 — `checkpoint` / `dry_run` are python-TRUTHY, `if_match` guards on
    /// any non-null value (a non-string never matches and echoes raw), and a
    /// FALSY `ckpt_id` is missing.
    @Test func argTypingFollowsPythonTruthiness() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "t.txt", "one\n")
        let w = await callBin(
            k,
            .object([
                "type": .string("write"), "path": .string("t.txt"),
                "checkpoint": .integer(1),
            ]), "two\n")
        #expect(w["ckpt_id"].asInt != nil, "\(w.serialize())")
        let mism = await callBin(
            k,
            .object([
                "type": .string("write"), "path": .string("t.txt"),
                "if_match": .integer(12345),
            ]), "three\n")
        #expect(mism["reason"].asString == "version_mismatch", "\(mism.serialize())")
        #expect(mism["expected"].asInt == 12345, "\(mism.serialize())")
        #expect(read(dir, "t.txt") == "two\n")
        let zero = await call(
            k,
            .object([
                "type": .string("restore"), "path": .string("t.txt"), "ckpt_id": .integer(0),
            ]))
        #expect(zero["reason"].asString == "missing_arg", "\(zero.serialize())")
        #expect(zero["arg"].asString == "ckpt_id", "\(zero.serialize())")
        let dry = await callBin(
            k,
            .object([
                "type": .string("patch"), "dry_run": .string("yes"),
                "edits": .array([.object(["path": .string("t.txt"), "hunks": .integer(1)])]),
            ]), block("two", "TWO"))
        #expect(dry["dry_run"].asBool == true, "\(dry.serialize())")
        #expect(read(dir, "t.txt") == "two\n")
    }

    /// A3.8 — the leg-reply check: a reply that is not an object IS the failure
    /// (`str(None)` = `"None"`), and an object fails only on a TRUTHY `error`,
    /// so `{"error": ""}` is a SUCCESS.
    @Test func legFailureFollowsPythonTruthiness() async {
        let registry = BundleRegistry()
        registry.register("probe_leg.tools", ProbeLegBundle())
        registry.register("text_file.tools", TextFileBundle())
        let kernel = Kernel(storage: .inMemory, bundles: registry)
        let root = Agent(id: AgentId("kernel_state"), handlerModule: nil, parentId: nil)
        kernel.register(root)
        kernel.setRoot(root)
        for (id, mode) in [("nul", "null"), ("blank", "blank_error")] {
            _ = await kernel.send([
                Item(
                    "kernel_state",
                    .object([
                        "type": "create_agent", "parent_id": "kernel_state",
                        "handler_module": "probe_leg.tools", "id": .string(id),
                        "mode": .string(mode), "callable_by": .object(["ed": .string("*")]),
                        "ingress_rule": "allow_all_ingress",
                    ]))
            ])[0].result
        }
        _ = await kernel.send([
            Item(
                "kernel_state",
                .object([
                    "type": "create_agent", "parent_id": "kernel_state",
                    "handler_module": "text_file.tools", "id": "ed",
                    "can_call": .object(["nul": .string("*"), "blank": .string("*")]),
                ]))
        ])[0].result
        let nul = await kernel.send([
            Item(
                AgentId("ed"),
                .object([
                    "type": .string("read"), "path": .string("x"), "on": .string("nul"),
                ]))
        ])[0].result
        #expect(nul["error"].asString == "None", "\(nul.serialize())")
        let blank = await kernel.send([
            Item(
                AgentId("ed"),
                .object([
                    "type": .string("read"), "path": .string("x"), "on": .string("blank"),
                ]))
        ])[0].result
        #expect(blank["total"].asInt == 0, "\(blank.serialize())")
        #expect(blank["eof"].asBool == true, "\(blank.serialize())")
    }

    /// A3.9 — a truthy non-string `path` travels to the leg VERBATIM (the tag
    /// grammar types `<path>2024</path>` as an int) and earns the LEG's bad_arg;
    /// only a falsy path is `missing_arg`.
    @Test func nonStringPathReachesTheLeg() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, .object(["type": .string("read"), "path": .integer(2024)]))
        #expect(r["error"].asString == "read_stream: path must be a string", "\(r.serialize())")
        #expect(r["reason"].asString == "bad_arg", "\(r.serialize())")
        let empty = await call(k, .object(["type": .string("read"), "path": .string("")]))
        #expect(empty["reason"].asString == "missing_arg", "\(empty.serialize())")
    }

    /// A3.10 — a ckpt id is an INT everywhere it appears: the `write` reply, the
    /// `checkpoint` reply, the `checkpoints` listing and the patch's `ckpt_ids`;
    /// `drop` echoes the caller's value as given.
    @Test func checkpointIdsAreIntsEverywhere() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "p.txt", "a = 1\n")
        let c = await call(k, .object(["type": .string("checkpoint"), "path": .string("p.txt")]))
        #expect(c["ckpt_id"].asInt != nil, "\(c.serialize())")
        let ls = await call(k, .object(["type": .string("checkpoints"), "path": .string("p.txt")]))
        #expect(ls["ckpts"].asArray?.count == 1, "\(ls.serialize())")
        #expect(ls["ckpts"][0]["id"].asInt == c["ckpt_id"].asInt, "\(ls.serialize())")
        if case .integer = ls["ckpts"][0]["id"] {} else {
            Issue.record("ckpt listing id is not an int: \(ls.serialize())")
        }
        let p = await callBin(
            k,
            .object([
                "type": .string("patch"), "checkpoint": .bool(true),
                "edits": .array([.object(["path": .string("p.txt"), "hunks": .integer(1)])]),
            ]), block("a = 1", "a = 2"))
        #expect(p["ckpt_ids"][0]["ckpt_id"].asInt != nil, "\(p.serialize())")
        let d = await call(
            k,
            .object([
                "type": .string("drop"), "path": .string("p.txt"),
                "ckpt_id": c["ckpt_id"],
            ]))
        #expect(d["dropped"].asBool == true, "\(d.serialize())")
        #expect(d["ckpt_id"].asInt == c["ckpt_id"].asInt, "\(d.serialize())")
    }

    /// A3.13 — the bundle readme is python's `readme.md`, byte for byte.
    @Test func readmeIsPythonVerbatim() throws {
        let expected = try String(
            contentsOf: pythonSource("text_file/src/text_file/readme.md"), encoding: .utf8)
        #expect(TextFileBundle().readme == expected)
    }

    /// A3.14 — python `rec.get("description") or <default>`: an EMPTY
    /// description is falsy and reflects the default sentence.
    @Test func emptyDescriptionReflectsTheDefaultSentence() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await k.send([
            Item(
                "kernel_state",
                .object([
                    "type": "update_agent", "id": "ed", "description": .string(""),
                ]))
        ])[0].result
        let r = await call(k, .object(["type": .string("reflect")]))
        #expect(
            r["sentence"].asString?.hasPrefix("Text documents on a file surface") == true,
            "\(r["sentence"].serialize())")
    }

    /// A3.15 — an EMPTY SEARCH hunk counts `len(s) + 1` CODE POINTS (`"a\r\nb"`
    /// is 4 code points, not 3 graphemes) and inserts at the start.
    @Test func emptySearchHunkCountsCodePoints() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "e.txt", "")
        let r = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([.object(["path": .string("e.txt"), "hunks": .integer(1)])]),
            ]), block("", "hello"))
        #expect(r["patched"].asBool == true, "\(r.serialize())")
        #expect(read(dir, "e.txt") == "hello")
        _ = await seed(k, "crlf.txt", "a\r\nb")
        let amb = await callBin(
            k,
            .object([
                "type": .string("patch"),
                "edits": .array([.object(["path": .string("crlf.txt"), "hunks": .integer(1)])]),
            ]), block("", "x"))
        #expect(amb["reason"].asString == "ambiguous", "\(amb.serialize())")
        #expect(
            amb["failed"][0]["reason"].asString == "search text matches 5 places",
            "\(amb.serialize())")
    }

    @Test func unknownTypeCarriesVerb() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await call(k, .object(["type": .string("frobnicate")]))
        #expect(r["reason"].asString == "unhandled", "\(r)")
        #expect(r["verb"].asString == "frobnicate", "\(r)")
    }

    @Test func readWithMaxLimitDoesNotTrap() async {
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "r.txt", "l1\nl2\n")
        let r = await call(
            k,
            .object([
                "type": .string("read"), "path": .string("r.txt"),
                "offset": .integer(1), "limit": .integer(Int64.max),
            ]))
        #expect(r["eof"].asBool == true, "\(r)")
        #expect(r["lines"].asArray?.count == 1, "\(r)")
        #expect(r["lines"][0]["text"].asString == "l2", "\(r)")
    }

    @Test func nonIntArgsAreBadArgRefusals() async {
        // #136 — a present-but-wrong number is a TYPO, not a new default.
        // `asInt ?? 0` used to swallow `offset: "12"` and silently page from
        // the top.
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "n.txt", (0..<10).map { "L\($0)\n" }.joined())
        let bads: [JSON] = [
            .string("ten"), .double(1.5), .bool(true), .null, .integer(-1), .array([.integer(0)]),
        ]
        for badValue in bads {
            let r = await call(
                k,
                .object([
                    "type": .string("read"), "path": .string("n.txt"), "offset": badValue,
                ]))
            #expect(r["reason"].asString == "bad_arg", "\(badValue.serialize()): \(r.serialize())")
            #expect(r["arg"].asString == "offset", "\(badValue.serialize()): \(r.serialize())")
        }
        for badValue in [JSON.string("200"), .double(2.5), .bool(true), .null, .integer(0), .integer(-1)] {
            let r = await call(
                k,
                .object([
                    "type": .string("read"), "path": .string("n.txt"), "limit": badValue,
                ]))
            #expect(r["reason"].asString == "bad_arg", "\(badValue.serialize()): \(r.serialize())")
            #expect(r["arg"].asString == "limit", "\(badValue.serialize()): \(r.serialize())")
        }
        // ABSENT still takes the documented defaults (offset 0, limit 200).
        let r = await call(k, .object(["type": .string("read"), "path": .string("n.txt")]))
        #expect(r["total"].asInt == 10, "\(r.serialize())")
        #expect(r["lines"][0]["n"].asInt == 1, "\(r.serialize())")
        #expect(r["lines"][9]["text"].asString == "L9", "\(r.serialize())")
        #expect(r["eof"].asBool == true, "\(r.serialize())")
    }

    @Test func patchHunkCountMustBeAnInt() async {
        // #136 — the per-edit `hunks` count is typed too: a string count would
        // slice the shared blob by nothing at all.
        let (k, dir) = await openEditor()
        defer { try? FileManager.default.removeItem(at: dir) }
        _ = await seed(k, "m.py", "a = 1\n")
        let blob = block("a = 1", "a = 2")
        for badValue in [JSON.string("1"), .double(1.5), .bool(true), .null, .integer(0), .integer(-1)] {
            let r = await callBin(
                k,
                .object([
                    "type": .string("patch"),
                    "edits": .array([.object(["path": .string("m.py"), "hunks": badValue])]),
                ]), blob)
            #expect(r["reason"].asString == "bad_arg", "\(badValue.serialize()): \(r.serialize())")
            #expect(r["arg"].asString == "edits", "\(badValue.serialize()): \(r.serialize())")
        }
        // …and the file was not touched by any of them.
        #expect(read(dir, "m.py") == "a = 1\n")
    }
}

/// A stand-in file leg whose reply SHAPE is the thing under test: `mode:null`
/// answers a non-object reply, `mode:blank_error` an object carrying a FALSY
/// `error`. Python's leg check reads both through `str(r)` / truthiness.
struct ProbeLegBundle: AgentBundle {
    let name = "probe_leg"

    private func reply(_ agentId: AgentId, _ kernel: Kernel) -> JSON {
        let mode = kernel.agent(agentId)?.metaValue(forKey: "mode")?.asString ?? "null"
        if mode == "blank_error" {
            return .object(["error": .string(""), "eof": .bool(true), "size": .integer(0)])
        }
        return .null
    }

    func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        reply(agentId, kernel)
    }

    func handleBinary(agentId: AgentId, header: JSON, blob: Data, kernel: Kernel)
        async throws -> (JSON?, Data?)
    {
        (reply(agentId, kernel), nil)
    }
}
