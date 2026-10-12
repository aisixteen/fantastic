// file_bridge's TEXT verbs — `read` / `write` / `edit` / `append` / `undo`.
// Python `file_bridge/tests/test_text_verbs.py` is the oracle, case for case.
//
// A caller edits with its own grants on the bridge, so there is no deputy. The
// posture cases (readonly, sealed) are answered by the bridge's own gate. `undo`
// is one RAM slot per path — a second undo is redo.

import CryptoKit
import FantasticFile
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

/// One open file_bridge `fs` rooted at `sd` under a fresh running dir (the
/// kernel's root lives at `<dir>/.fantastic`, so `Kernel.workdir` is `<dir>`).
private func bridge(_ meta: OrderedDictionary<String, JSON> = [:]) async -> (Kernel, URL) {
    let registry = BundleRegistry()
    registry.register("file_bridge.tools", FileBundle())
    let kernel = Kernel(storage: .inMemory, bundles: registry)
    let dir = FileManager.default.temporaryDirectory
        .appendingPathComponent("fantastic-textverbs-\(UUID().uuidString)")
    try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    let root = Agent(
        id: AgentId("kernel_state"), handlerModule: nil, parentId: nil,
        rootPath: dir.appendingPathComponent(".fantastic"))
    kernel.register(root)
    kernel.setRoot(root)
    var create: OrderedDictionary<String, JSON> = [
        "type": "create_agent", "parent_id": "kernel_state", "id": "fs",
        "handler_module": "file_bridge.tools", "allow": ["."], "root": "sd",
        "ingress_rule": "allow_all_ingress",
    ]
    for (k, v) in meta { create[k] = v }
    let r = await kernel.send([Item("kernel_state", .object(create))])[0].result
    #expect(r["id"] == "fs", "\(r.serialize())")
    return (kernel, dir.appendingPathComponent("sd"))
}

private func call(_ k: Kernel, _ p: OrderedDictionary<String, JSON>) async -> JSON {
    await k.send([Item(AgentId("fs"), .object(p))])[0].result
}

/// A verb whose document rides the BINARY channel — python hands the handler
/// `bytes`; so does this.
private func callBody(_ k: Kernel, _ p: OrderedDictionary<String, JSON>, _ body: Data) async
    -> JSON
{
    await k.send([Item(AgentId("fs"), .object(p), binary: Blob("bytes", body))])[0].result
}

private func block(_ old: String, _ new: String) -> String {
    "<<<<<<< SEARCH\n\(old)\n=======\n\(new)\n>>>>>>> REPLACE"
}

/// One-file edit through the TRANSACTION shape — even a single file rides `edits`.
private func patch(
    _ k: Kernel, _ path: String, _ patch: String?, hunks: Int64? = nil, ifMatch: String? = nil,
    dryRun: Bool = false
) async -> JSON {
    var edit: OrderedDictionary<String, JSON> = [
        "path": .string(path),
        "hunks": .integer(
            hunks ?? Int64(max(1, (patch ?? "").components(separatedBy: "<<<<<<< SEARCH").count - 1))),
    ]
    if let ifMatch { edit["if_match"] = .string(ifMatch) }
    var p: OrderedDictionary<String, JSON> = ["type": "edit", "edits": .array([.object(edit)])]
    if dryRun { p["dry_run"] = true }
    guard let patch else { return await call(k, p) }
    return await callBody(k, p, Data(patch.utf8))
}

private func seed(_ k: Kernel, _ path: String, _ text: String) async -> JSON {
    await callBody(k, ["type": "write", "path": .string(path)], Data(text.utf8))
}

private func text(_ dir: URL, _ path: String) -> String? {
    try? String(contentsOf: dir.appendingPathComponent(path), encoding: .utf8)
}

private func put(_ dir: URL, _ path: String, _ s: String) {
    let url = dir.appendingPathComponent(path)
    try? FileManager.default.createDirectory(
        at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
    try? Data(s.utf8).write(to: url)
}

private func v16(_ s: String) -> String {
    SHA256.hash(data: Data(s.utf8)).map { String(format: "%02x", $0) }.joined().prefix(16)
        .lowercased()
}

@Suite("file_bridge text verbs")
struct FileTextVerbsTests {
    // ─── edit: happy paths ─────────────────────────────────────

    @Test func singleHunkAppliesAndWrites() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "a = 1\nb = 2\nc = 3\n")
        let r = await patch(k, "m.py", block("b = 2", "b = 20"))
        #expect(r["edited"] == true && r["edits"][0]["hunks"] == 1, "\(r.serialize())")
        #expect(text(d, "m.py") == "a = 1\nb = 20\nc = 3\n")
    }

    @Test func multipleHunksApplyInOrder() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "x = 1\ny = 2\nz = 3\n")
        let r = await patch(k, "m.py", block("x = 1", "x = 11") + "\n" + block("z = 3", "z = 33"))
        #expect(r["edited"] == true && r["edits"][0]["hunks"] == 2, "\(r.serialize())")
        #expect(text(d, "m.py") == "x = 11\ny = 2\nz = 33\n")
    }

    @Test func hunkMatchesContentAPriorHunkCreated() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.txt", "one\n")
        let r = await patch(k, "m.txt", block("one", "two") + "\n" + block("two", "three"))
        #expect(r["edited"] == true && r["edits"][0]["hunks"] == 2, "\(r.serialize())")
        #expect(text(d, "m.txt") == "three\n")
    }

    @Test func hunkWithQuotesBackslashesNewlinesIsVerbatim() async {
        let (k, d) = await bridge()
        _ = await seed(k, "p.py", "print(\"hi\")\npath = \"c:\\\\tmp\"\n")
        let r = await patch(k, "p.py", block("print(\"hi\")", "print(\"bye\", end=\"\")"))
        #expect(r["edited"] == true, "\(r.serialize())")
        #expect(text(d, "p.py") == "print(\"bye\", end=\"\")\npath = \"c:\\\\tmp\"\n")
    }

    @Test func unicodeContentAndHunk() async {
        let (k, d) = await bridge()
        _ = await seed(k, "u.txt", "café — over\nnext\n")
        let r = await patch(k, "u.txt", block("café — over", "tea — done"))
        #expect(r["edited"] == true, "\(r.serialize())")
        #expect(text(d, "u.txt") == "tea — done\nnext\n")
    }

    @Test func multilineHunk() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "def f():\n    return 1\n\ndef g():\n    return 2\n")
        let r = await patch(
            k, "m.py", block("def f():\n    return 1", "def f():\n    return 99"))
        #expect(r["edited"] == true, "\(r.serialize())")
        #expect(text(d, "m.py") == "def f():\n    return 99\n\ndef g():\n    return 2\n")
    }

    // ─── edit: fail-loud, all-or-nothing ───────────────────────

    @Test func noMatchFailsAndWritesNothing() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "a = 1\n")
        let r = await patch(k, "m.py", block("nope = 0", "x = 0"))
        #expect(r["edited"] == nil && r["reason"] == "no_match", "\(r.serialize())")
        #expect(r["failed"][0]["index"] == 0)
        #expect(text(d, "m.py") == "a = 1\n")
    }

    @Test func ambiguousMatchFailsAndWritesNothing() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "v = 1\nv = 1\n")
        let r = await patch(k, "m.py", block("v = 1", "v = 2"))
        #expect(r["edited"] == nil && r["reason"] == "ambiguous", "\(r.serialize())")
        #expect(r["failed"][0]["reason"].asString?.contains("2 places") == true)
        #expect(text(d, "m.py") == "v = 1\nv = 1\n")
    }

    @Test func secondHunkFailureRevertsTheWholePatch() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "a = 1\nb = 2\n")
        let r = await patch(k, "m.py", block("a = 1", "a = 10") + "\n" + block("missing", "x"))
        #expect(r["edited"] == nil && r["failed"][0]["index"] == 1, "\(r.serialize())")
        #expect(text(d, "m.py") == "a = 1\nb = 2\n")
    }

    // ─── edit: bad input ───────────────────────────────────────

    @Test func malformedPatchUnterminatedBlock() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "a = 1\n")
        let r = await patch(k, "m.py", "<<<<<<< SEARCH\na = 1\n=======\na = 2\n")
        #expect(r["edited"] == nil && r["reason"] == "bad_arg", "\(r.serialize())")
        #expect(text(d, "m.py") == "a = 1\n")
    }

    @Test func emptyPatchNoBlocks() async {
        let (k, _) = await bridge()
        _ = await seed(k, "m.py", "a = 1\n")
        let r = await patch(k, "m.py", "just some prose, no markers")
        #expect(r["edited"] == nil && r["reason"] == "bad_arg", "\(r.serialize())")
    }

    @Test func missingBytes() async {
        let (k, _) = await bridge()
        _ = await seed(k, "m.py", "a = 1\n")
        let r = await patch(k, "m.py", nil)
        #expect(r["edited"] == nil && r["error"].asString?.contains("bytes") == true)
    }

    @Test func fileNotFound() async {
        let (k, _) = await bridge()
        let r = await patch(k, "nope.py", block("a", "b"))
        #expect(r["edited"] == nil && r["reason"] == "not_found", "\(r.serialize())")
    }

    @Test func missingPath() async {
        let (k, _) = await bridge()
        let r = await callBody(k, ["type": "edit"], Data(block("a", "b").utf8))
        #expect(r["edited"] == nil && r["error"].asString?.contains("edits") == true)
    }

    @Test func badUtf8Patch() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "a = 1\n")
        let r = await callBody(
            k, ["type": "edit", "edits": [["path": "m.py", "hunks": 1]]],
            Data([0xff, 0xfe, 0xff]))
        #expect(r["edited"] == nil && r["reason"] == "bad_arg", "\(r.serialize())")
        #expect(text(d, "m.py") == "a = 1\n")
    }

    @Test func malformedMissingDivider() async {
        let (k, d) = await bridge()
        _ = await seed(k, "m.py", "a = 1\n")
        let r = await patch(k, "m.py", "<<<<<<< SEARCH\na = 1\n>>>>>>> REPLACE")
        #expect(r["edited"] == nil && r["reason"] == "bad_arg", "\(r.serialize())")
        #expect(text(d, "m.py") == "a = 1\n")
    }

    // ─── the leg's own posture ─────────────────────────────────

    @Test func readonlyLegRefusesTheWrite() async {
        let (k, d) = await bridge(["readonly": true])
        put(d, "m.py", "a = 1\n")
        let r = await patch(k, "m.py", block("a = 1", "a = 2"))
        #expect(r["reason"] == "unauthorized", "\(r.serialize())")
        #expect(text(d, "m.py") == "a = 1\n")
    }

    @Test func sealedLegDeniesEverything() async {
        let (k, d) = await bridge(["ingress_rule": .null])
        put(d, "m.py", "a = 1\n")
        let r = await patch(k, "m.py", block("a = 1", "a = 2"))
        #expect(r["reason"] == "unauthorized", "\(r.serialize())")
        #expect(text(d, "m.py") == "a = 1\n")
    }

    // ─── read / write / append / version ───────────────────────

    @Test func writeThenReadLines() async {
        let (k, _) = await bridge()
        let w = await seed(k, "doc.md", "alpha\nbeta\ngamma\n")
        #expect(w["size"] == 17 && w["version"].asString?.count == 16, "\(w.serialize())")
        let r = await call(k, ["type": "read", "path": "doc.md"])
        #expect(r["lines"].asArray?.map { $0["text"] } == ["alpha", "beta", "gamma"])
        #expect(r["lines"].asArray?.map { $0["n"] } == [1, 2, 3])
        #expect(r["total"] == 3 && r["eof"] == true)
        #expect(r["version"] == w["version"])
    }

    @Test func readPagesByLineOffset() async {
        let (k, _) = await bridge()
        _ = await seed(k, "n.txt", (0..<10).map { "L\($0)\n" }.joined())
        let r = await call(k, ["type": "read", "path": "n.txt", "offset": 4, "limit": 3])
        #expect(r["lines"].asArray?.map { $0["text"] } == ["L4", "L5", "L6"])
        #expect(r["lines"][0]["n"] == 5 && r["eof"] == false && r["total"] == 10)
    }

    @Test func readRefusesBinary() async {
        let (k, _) = await bridge()
        _ = await callBody(k, ["type": "write", "path": "b.bin"], Data([0x61, 0x00, 0x62]))
        let r = await call(k, ["type": "read", "path": "b.bin"])
        #expect(r["reason"] == "bad_arg" && r["error"].asString?.contains("binary") == true)
    }

    @Test func appendCreatesThenGrows() async {
        let (k, d) = await bridge()
        let r1 = await callBody(k, ["type": "append", "path": "log.txt"], Data("one\n".utf8))
        #expect(r1["size"] == 4, "\(r1.serialize())")
        let r2 = await callBody(k, ["type": "append", "path": "log.txt"], Data("two\n".utf8))
        #expect(r2["size"] == 8, "\(r2.serialize())")
        #expect(text(d, "log.txt") == "one\ntwo\n")
        #expect(r1["version"] != r2["version"])
    }

    @Test func writeIsAtomicNoTmpDebrisOnSuccess() async {
        let (k, d) = await bridge()
        _ = await seed(k, "a.txt", "x\n")
        let names = (try? FileManager.default.contentsOfDirectory(atPath: d.path)) ?? []
        #expect(names == ["a.txt"])
    }

    @Test func ifMatchGuardsWriteAndEdit() async {
        let (k, d) = await bridge()
        let w = await seed(k, "v.txt", "one\n")
        let stale = String(repeating: "0", count: 16)
        var r = await callBody(
            k, ["type": "write", "path": "v.txt", "if_match": .string(stale)], Data("two\n".utf8))
        #expect(r["reason"] == "version_mismatch" && r["actual"] == w["version"], "\(r.serialize())")
        #expect(text(d, "v.txt") == "one\n")
        r = await callBody(
            k, ["type": "write", "path": "v.txt", "if_match": w["version"]], Data("two\n".utf8))
        #expect(r["version"] != w["version"], "\(r.serialize())")
        r = await patch(k, "v.txt", block("two", "three"), ifMatch: stale)
        #expect(r["reason"] == "version_mismatch" && r["path"] == "v.txt", "\(r.serialize())")
    }

    @Test func editDryRunWritesNothing() async {
        let (k, d) = await bridge()
        _ = await seed(k, "d.txt", "a = 1\n")
        let r = await patch(k, "d.txt", block("a = 1", "a = 2"), dryRun: true)
        #expect(r["dry_run"] == true && r["edits"][0]["hunks"] == 1, "\(r.serialize())")
        #expect(text(d, "d.txt") == "a = 1\n")
    }

    // ─── the TRANSACTION (multi-file edits in ONE call) ────────

    @Test func multifileEditAppliesBothAtomically() async {
        let (k, d) = await bridge()
        _ = await seed(k, "a.py", "x = 1\n")
        _ = await seed(k, "b.py", "y = 2\nz = 3\n")
        let blob =
            block("x = 1", "x = 10") + "\n" + block("y = 2", "y = 20") + "\n"
            + block("z = 3", "z = 30")
        let r = await callBody(
            k,
            ["type": "edit", "edits": [["path": "a.py", "hunks": 1], ["path": "b.py", "hunks": 2]]],
            Data(blob.utf8))
        #expect(r["edited"] == true, "\(r.serialize())")
        #expect(r["edits"].asArray?.map { [$0["path"], $0["hunks"]] } == [["a.py", 1], ["b.py", 2]])
        #expect(text(d, "a.py") == "x = 10\n")
        #expect(text(d, "b.py") == "y = 20\nz = 30\n")
    }

    @Test func multifileDoomedSecondEditWritesNothing() async {
        let (k, d) = await bridge()
        _ = await seed(k, "a.py", "x = 1\n")
        _ = await seed(k, "b.py", "y = 2\n")
        let blob = block("x = 1", "x = 10") + "\n" + block("missing", "nope")
        let r = await callBody(
            k,
            ["type": "edit", "edits": [["path": "a.py", "hunks": 1], ["path": "b.py", "hunks": 1]]],
            Data(blob.utf8))
        #expect(r["edited"] == nil && r["reason"] == "no_match", "\(r.serialize())")
        #expect(r["failed"][0]["path"] == "b.py")
        #expect(text(d, "a.py") == "x = 1\n")
        #expect(text(d, "b.py") == "y = 2\n")
    }

    @Test func hunkCountsMustSumToTheBlob() async {
        let (k, d) = await bridge()
        _ = await seed(k, "a.py", "x = 1\n")
        let r = await callBody(
            k, ["type": "edit", "edits": [["path": "a.py", "hunks": 2]]],
            Data(block("x = 1", "x = 2").utf8))
        #expect(r["reason"] == "bad_arg" && r["error"].asString?.contains("sum") == true)
        #expect(text(d, "a.py") == "x = 1\n")
    }

    @Test func perEditIfMatchGuardsTheWholeTransaction() async {
        let (k, d) = await bridge()
        _ = await seed(k, "a.py", "one\n")
        _ = await seed(k, "b.py", "two\n")
        let blob = block("one", "ONE") + "\n" + block("two", "TWO")
        let r = await callBody(
            k,
            [
                "type": "edit",
                "edits": [
                    ["path": "a.py", "hunks": 1],
                    ["path": "b.py", "hunks": 1, "if_match": .string(String(repeating: "0", count: 16))],
                ],
            ], Data(blob.utf8))
        #expect(r["reason"] == "version_mismatch" && r["path"] == "b.py", "\(r.serialize())")
        #expect(text(d, "a.py") == "one\n")
    }

    /// Text courtesy: a spawned program's ctrl-fd wire is JSON (no bytes type) —
    /// a str document lands verbatim utf-8.
    @Test func writeAcceptsStrAndUtf8Encodes() async {
        let (k, d) = await bridge()
        let r = await call(k, ["type": "write", "path": "t.txt", "bytes": "h\u{e9}llo\n"])
        #expect(r["size"] == .integer(Int64("h\u{e9}llo\n".utf8.count)), "\(r.serialize())")
        #expect(text(d, "t.txt") == "h\u{e9}llo\n")
    }

    // ─── per-document serialization + payload hardening ────────

    @Test func editRefusesTheSamePathTwice() async {
        let (k, d) = await bridge()
        _ = await seed(k, "a.md", "one\ntwo\n")
        let r = await callBody(
            k,
            ["type": "edit", "edits": [["path": "a.md", "hunks": 1], ["path": "a.md", "hunks": 1]]],
            Data((block("one", "1") + "\n" + block("two", "2")).utf8))
        #expect(r["reason"] == "bad_arg" && r["arg"] == "edits", "\(r.serialize())")
        #expect(r["error"].asString?.contains("at most once") == true)
        #expect(text(d, "a.md") == "one\ntwo\n")
    }

    @Test func concurrentEditsOnOneFileBothLand() async {
        let (k, d) = await bridge()
        _ = await seed(k, "f.py", "a = 1\nb = 2\n")
        async let r1 = patch(k, "f.py", block("a = 1", "a = 10"))
        async let r2 = patch(k, "f.py", block("b = 2", "b = 20"))
        let (a, b) = await (r1, r2)
        #expect(a["edited"] == true && b["edited"] == true, "\(a.serialize()) \(b.serialize())")
        #expect(text(d, "f.py") == "a = 10\nb = 20\n")
        let final = v16("a = 10\nb = 20\n")
        let versions: Set<String> = [
            a["edits"][0]["version"].asString ?? "", b["edits"][0]["version"].asString ?? "",
        ]
        #expect(versions.contains(final), "\(versions)")
        let intermediates: Set<String> = [v16("a = 10\nb = 2\n"), v16("a = 1\nb = 20\n")]
        #expect(versions.subtracting([final]).isSubset(of: intermediates), "\(versions)")
    }

    @Test func textChannelBytesGuardIsMissingArg() async {
        let (k, _) = await bridge()
        for verb in ["write", "append"] {
            let r = await call(k, ["type": .string(verb), "path": "x"])
            #expect(
                r["reason"] == "missing_arg" && r["arg"].asString?.hasPrefix("bytes") == true,
                "\(r.serialize())")
        }
        let r = await call(k, ["type": "edit", "edits": [["path": "x", "hunks": 1]]])
        #expect(
            r["reason"] == "missing_arg" && r["arg"].asString?.hasPrefix("bytes") == true,
            "\(r.serialize())")
    }

    @Test func unknownTypeCarriesVerb() async {
        let (k, _) = await bridge()
        let r = await call(k, ["type": "frobnicate"])
        #expect(r["reason"] == "unhandled" && r["verb"] == "frobnicate", "\(r.serialize())")
    }

    @Test func readWithMaxLimitDoesNotTrap() async {
        let (k, _) = await bridge()
        _ = await seed(k, "r.txt", "l1\nl2\n")
        let r = await call(
            k, ["type": "read", "path": "r.txt", "offset": 1, "limit": .integer(Int64.max)])
        #expect(r["eof"] == true && r["lines"].asArray?.map { $0["text"] } == ["l2"])
    }

    /// A present-but-wrong number is a typo, answered as one; an explicit null is
    /// ABSENT.
    @Test func nonIntArgsAreBadArgRefusals() async {
        let (k, _) = await bridge()
        _ = await seed(k, "n.txt", (0..<10).map { "L\($0)\n" }.joined())
        let offsets: [JSON] = ["ten", .double(1.5), true, -1, [0]]
        for bad in offsets {
            let r = await call(k, ["type": "read", "path": "n.txt", "offset": bad])
            #expect(r["reason"] == "bad_arg" && r["arg"] == "offset", "\(bad.serialize()) \(r.serialize())")
        }
        let limits: [JSON] = ["200", .double(2.0), true, 0, -1]
        for bad in limits {
            let r = await call(k, ["type": "read", "path": "n.txt", "limit": bad])
            #expect(r["reason"] == "bad_arg" && r["arg"] == "limit", "\(bad.serialize()) \(r.serialize())")
        }
        let r = await call(k, ["type": "read", "path": "n.txt"])
        #expect(r["lines"].asArray?.map { $0["text"].asString ?? "" } == (0..<10).map { "L\($0)" })
        #expect(r["lines"][0]["n"] == 1 && r["eof"] == true)
    }

    @Test func editHunkCountMustBeAnInt() async {
        let (k, _) = await bridge()
        _ = await seed(k, "m.py", "a = 1\n")
        let blob = Data(block("a = 1", "a = 2").utf8)
        let counts: [JSON] = ["1", .double(1.5), true, .null, 0, -1]
        for bad in counts {
            let r = await callBody(
                k, ["type": "edit", "edits": [["path": "m.py", "hunks": bad]]], blob)
            #expect(r["reason"] == "bad_arg" && r["arg"] == "edits", "\(bad.serialize()) \(r.serialize())")
        }
        let r = await call(k, ["type": "read", "path": "m.py"])
        #expect(r["lines"].asArray?.map { $0["text"] } == ["a = 1"])
    }

    // ─── undo: ONE slot per path, in RAM ───────────────────────

    @Test func undoRestoresWhatTheLastWriteReplacedAndRedoes() async {
        let (k, d) = await bridge()
        _ = await seed(k, "u.txt", "one\n")
        _ = await seed(k, "u.txt", "two\n")
        let r = await call(k, ["type": "undo", "path": "u.txt"])
        #expect(r["undone"] == true, "\(r.serialize())")
        #expect(text(d, "u.txt") == "one\n")
        // a second undo is REDO: the slot now holds what the undo replaced
        _ = await call(k, ["type": "undo", "path": "./u.txt"])
        #expect(text(d, "u.txt") == "two\n")
    }

    @Test func undoOfACreatedFileDeletesIt() async {
        let (k, d) = await bridge()
        _ = await seed(k, "new.txt", "x\n")
        let r = await call(k, ["type": "undo", "path": "new.txt"])
        #expect(r == ["undone": true, "path": "new.txt", "deleted": true], "\(r.serialize())")
        #expect(text(d, "new.txt") == nil)
    }

    @Test func undoRevertsAnEditAndAnAppend() async {
        let (k, d) = await bridge()
        _ = await seed(k, "e.py", "a = 1\n")
        let r = await patch(k, "e.py", block("a = 1", "a = 2"))
        #expect(r["edited"] == true, "\(r.serialize())")
        _ = await call(k, ["type": "undo", "path": "e.py"])
        #expect(text(d, "e.py") == "a = 1\n")
        _ = await callBody(k, ["type": "append", "path": "e.py"], Data("b = 2\n".utf8))
        _ = await call(k, ["type": "undo", "path": "e.py"])
        #expect(text(d, "e.py") == "a = 1\n")
    }

    @Test func undoWithNoSlotIsNotFound() async {
        let (k, d) = await bridge()
        put(d, "z.txt", "z\n")
        let r = await call(k, ["type": "undo", "path": "z.txt"])
        #expect(
            r["reason"] == "not_found" && r["error"].asString?.contains("nothing to undo") == true,
            "\(r.serialize())")
    }

    @Test func undoIsRefusedOnAReadonlyLeg() async {
        let (k, _) = await bridge(["readonly": true])
        let r = await call(k, ["type": "undo", "path": "z.txt"])
        #expect(r["reason"] == "unauthorized", "\(r.serialize())")
    }

    @Test func textVerbsSeeOnlyExposedPaths() async {
        let (k, d) = await bridge(["allow": ["pub"]])
        try? FileManager.default.createDirectory(
            at: d.appendingPathComponent("pub"), withIntermediateDirectories: true)
        put(d, ".env", "SECRET=1\n")
        for verb in ["read", "write", "append", "undo"] {
            let r = await callBody(k, ["type": .string(verb), "path": ".env"], Data("x".utf8))
            #expect(r["reason"] == "not_found", "\(verb) \(r.serialize())")
        }
        #expect(text(d, ".env") == "SECRET=1\n")
        let r = await callBody(k, ["type": "write", "path": "pub/a.txt"], Data("ok".utf8))
        #expect(r["size"] == 2, "\(r.serialize())")
    }

    /// The bridge is going away — its undo slots go with it.
    @Test func deletingTheBridgeDropsItsUndoSlots() async {
        let (k, _) = await bridge()
        _ = await seed(k, "s.txt", "one\n")
        _ = await k.send([Item("kernel_state", ["type": "delete_agent", "id": "fs"])])
        let r2 = await k.send([
            Item(
                "kernel_state",
                [
                    "type": "create_agent", "parent_id": "kernel_state", "id": "fs",
                    "handler_module": "file_bridge.tools", "allow": ["."], "root": "sd",
                    "ingress_rule": "allow_all_ingress",
                ])
        ])[0].result
        #expect(r2["id"] == "fs", "\(r2.serialize())")
        let r = await call(k, ["type": "undo", "path": "s.txt"])
        #expect(r["reason"] == "not_found", "\(r.serialize())")
    }

    /// a17: the reflect carries the edit grammar and the compose rule verbatim.
    @Test func reflectCarriesTheEditGrammarAndCompose() async {
        let (k, _) = await bridge()
        let r = await call(k, ["type": "reflect"])
        #expect(r["edit_grammar"].asString?.hasPrefix("one or more blocks:") == true)
        #expect(r["compose"].asString?.contains("`bytes` is VERBATIM") == true)
        #expect(r["hidden_count"].isNull)
        #expect(r.asObject?["verbs"] == nil)
        let verbs = FileBundle().verbDocTable.asObject.map { Array($0.keys) } ?? []
        for v in ["read", "write", "edit", "append", "undo"] { #expect(verbs.contains(v), "\(v)") }
    }
}
