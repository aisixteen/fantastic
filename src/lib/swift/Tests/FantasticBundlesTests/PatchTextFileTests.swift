// patch_text_file — SEARCH/REPLACE whole-file edit over the binary channel.
//
// Mirror of py `file_bridge/tests/test_patch.py`. The patch rides as raw utf-8
// BYTES on the binary channel (sendWithBinary), so a hunk full of quotes,
// backslashes, and newlines applies VERBATIM — never JSON-escaped. Each SEARCH
// must match the current content EXACTLY ONCE; not-found / ambiguous FAILS LOUD
// and writes nothing (all-or-nothing, no fuzzy apply).

import FantasticFile
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections
import Testing

@Suite("patch_text_file (SEARCH/REPLACE)")
struct PatchTextFileTests {

    /// A file_bridge rooted at a fresh cwd-relative dir (the running-dir law: root
    /// must lie inside the process cwd, so we cannot use the temp dir). Returns
    /// (kernel, fs-agent-id, dir). Caller removes `dir`.
    private func openBridge(
        readonly: Bool = false, sealed: Bool = false
    ) async -> (Kernel, String, URL) {
        let kernel = makeKernelWithAll()
        let fm = FileManager.default
        let rel = "fantastic-patch-\(UUID().uuidString)"
        let dir = URL(fileURLWithPath: fm.currentDirectoryPath).appendingPathComponent(rel)
        try? fm.createDirectory(at: dir, withIntermediateDirectories: true)
        var meta: OrderedDictionary<String, JSON> = [
            "type": "create_agent", "parent_id": "core",
            "handler_module": "file_bridge.tools", "id": "fs", "root": .string(rel),
        ]
        if !sealed { meta["ingress_rule"] = "allow_all_ingress" }
        if readonly { meta["readonly"] = .bool(true) }
        _ = await kernel.send([Item("core", .object(meta))])[0].result
        return (kernel, "fs", dir)
    }

    /// Seed a file THROUGH the bridge (write_stream, binary channel).
    private func seed(_ kernel: Kernel, _ fid: String, _ path: String, _ text: String) async {
        _ = await kernel.sendWithBinary(
            AgentId(fid),
            .object([
                "type": .string("write_stream"), "path": .string(path),
                "truncate": .bool(true),
            ]), Data(text.utf8))
    }

    /// Apply a patch THROUGH the bridge (patch_text_file, binary channel).
    private func patch(
        _ kernel: Kernel, _ fid: String, _ path: String, _ patch: String?
    ) async -> JSON {
        let (m, _) = await kernel.sendWithBinary(
            AgentId(fid),
            .object(["type": .string("patch_text_file"), "path": .string(path)]),
            patch.map { Data($0.utf8) } ?? Data())
        return m
    }

    private func block(_ old: String, _ new: String) -> String {
        "<<<<<<< SEARCH\n\(old)\n=======\n\(new)\n>>>>>>> REPLACE"
    }

    private func read(_ dir: URL, _ path: String) -> String {
        (try? String(contentsOf: dir.appendingPathComponent(path), encoding: .utf8)) ?? "<none>"
    }

    // MARK: - happy paths

    @Test func singleHunkAppliesAndWrites() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\nb = 2\nc = 3\n")
        let r = await patch(k, fid, "m.py", block("b = 2", "b = 20"))
        #expect(r["patched"].asBool == true)
        #expect(r["hunks"].asInt == 1)
        #expect(read(dir, "m.py") == "a = 1\nb = 20\nc = 3\n")
    }

    @Test func multipleHunksApplyInOrder() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "x = 1\ny = 2\nz = 3\n")
        let p = block("x = 1", "x = 11") + "\n" + block("z = 3", "z = 33")
        let r = await patch(k, fid, "m.py", p)
        #expect(r["patched"].asBool == true && r["hunks"].asInt == 2)
        #expect(read(dir, "m.py") == "x = 11\ny = 2\nz = 33\n")
    }

    @Test func hunkMatchesContentAPriorHunkCreated() async {
        // Hunks apply against the EVOLVING content: hunk 2's SEARCH exists only
        // after hunk 1.
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.txt", "one\n")
        let p = block("one", "two") + "\n" + block("two", "three")
        let r = await patch(k, fid, "m.txt", p)
        #expect(r["patched"].asBool == true && r["hunks"].asInt == 2)
        #expect(read(dir, "m.txt") == "three\n")
    }

    @Test func hunkWithQuotesBackslashesNewlinesIsVerbatim() async {
        // The whole point of bytes-on-the-wire: no escaping. A quote-heavy program
        // applies exactly as written.
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let original = "print(\"hi\")\npath = \"c:\\\\tmp\"\n"
        await seed(k, fid, "p.py", original)
        let expected = "print(\"bye\", end=\"\")\npath = \"c:\\\\tmp\"\n"
        let r = await patch(
            k, fid, "p.py", block("print(\"hi\")", "print(\"bye\", end=\"\")"))
        #expect(r["patched"].asBool == true)
        #expect(read(dir, "p.py") == expected)
    }

    @Test func unicodeContentAndHunk() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "u.txt", "café — over\nnext\n")
        let r = await patch(k, fid, "u.txt", block("café — over", "tea — done"))
        #expect(r["patched"].asBool == true)
        #expect(read(dir, "u.txt") == "tea — done\nnext\n")
    }

    @Test func multilineHunk() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "def f():\n    return 1\n\ndef g():\n    return 2\n")
        let r = await patch(
            k, fid, "m.py",
            block("def f():\n    return 1", "def f():\n    return 99"))
        #expect(r["patched"].asBool == true)
        #expect(read(dir, "m.py") == "def f():\n    return 99\n\ndef g():\n    return 2\n")
    }

    // MARK: - fail-loud, all-or-nothing

    @Test func noMatchFailsAndWritesNothing() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\n")
        let r = await patch(k, fid, "m.py", block("nope = 0", "x = 0"))
        #expect(r["patched"].asBool == nil)
        #expect(r["reason"].asString == "no_match")
        #expect(r["failed"][0]["index"].asInt == 0)
        #expect(read(dir, "m.py") == "a = 1\n")  // untouched
    }

    @Test func ambiguousMatchFailsAndWritesNothing() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "v = 1\nv = 1\n")
        let r = await patch(k, fid, "m.py", block("v = 1", "v = 2"))
        #expect(r["patched"].asBool == nil)
        #expect(r["reason"].asString == "ambiguous")
        #expect(r["failed"][0]["reason"].asString?.contains("2 places") == true)
        #expect(read(dir, "m.py") == "v = 1\nv = 1\n")  // untouched
    }

    @Test func secondHunkFailureRevertsTheWholePatch() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\nb = 2\n")
        let p = block("a = 1", "a = 10") + "\n" + block("missing", "x")
        let r = await patch(k, fid, "m.py", p)
        #expect(r["patched"].asBool == nil && r["failed"][0]["index"].asInt == 1)
        // first hunk WAS valid but nothing is written — all-or-nothing.
        #expect(read(dir, "m.py") == "a = 1\nb = 2\n")
    }

    // MARK: - bad input

    @Test func malformedPatchUnterminatedBlock() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\n")
        let badPatch = "<<<<<<< SEARCH\na = 1\n=======\na = 2\n"  // no >>>>>>> REPLACE
        let r = await patch(k, fid, "m.py", badPatch)
        #expect(r["patched"].asBool == nil && r["reason"].asString == "bad_arg")
        #expect(read(dir, "m.py") == "a = 1\n")
    }

    @Test func emptyPatchNoBlocks() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\n")
        let r = await patch(k, fid, "m.py", "just some prose, no markers")
        #expect(r["patched"].asBool == nil && r["reason"].asString == "bad_arg")
    }

    @Test func missingBytes() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\n")
        let r = await patch(k, fid, "m.py", nil)
        #expect(r["patched"].asBool == nil)
        #expect(r["error"].asString?.contains("bytes") == true)
    }

    @Test func fileNotFound() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        let r = await patch(k, fid, "nope.py", block("a", "b"))
        #expect(r["patched"].asBool == nil && r["reason"].asString == "not_found")
    }

    @Test func missingPath() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        // Header carries the patch but NO `path`.
        let (r, _) = await k.sendWithBinary(
            AgentId(fid),
            .object(["type": .string("patch_text_file")]),
            Data(block("a", "b").utf8))
        #expect(r["patched"].asBool == nil)
        #expect(r["error"].asString?.contains("path") == true)
    }

    @Test func badUtf8Patch() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\n")
        // Not valid utf-8 — the patch is text.
        let (r, _) = await k.sendWithBinary(
            AgentId(fid),
            .object(["type": .string("patch_text_file"), "path": .string("m.py")]),
            Data([0xFF, 0xFE, 0xFF]))
        #expect(r["patched"].asBool == nil && r["reason"].asString == "bad_arg")
        #expect(read(dir, "m.py") == "a = 1\n")
    }

    @Test func malformedMissingDivider() async {
        let (k, fid, dir) = await openBridge()
        defer { try? FileManager.default.removeItem(at: dir) }
        await seed(k, fid, "m.py", "a = 1\n")
        // a SEARCH block that never reaches `=======` before it ends
        let badPatch = "<<<<<<< SEARCH\na = 1\n>>>>>>> REPLACE"
        let r = await patch(k, fid, "m.py", badPatch)
        #expect(r["patched"].asBool == nil && r["reason"].asString == "bad_arg")
        #expect(read(dir, "m.py") == "a = 1\n")
    }

    // MARK: - gate + posture

    @Test func readonlyRefuses() async {
        let (k, fid, dir) = await openBridge(readonly: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        // Readonly can't seed through the bridge — write the file directly.
        try? "a = 1\n".write(
            to: dir.appendingPathComponent("m.py"), atomically: true, encoding: .utf8)
        let r = await patch(k, fid, "m.py", block("a = 1", "a = 2"))
        #expect(r["reason"].asString == "unauthorized")
        #expect(read(dir, "m.py") == "a = 1\n")
    }

    @Test func sealedBridgeDeniesPatch() async {
        // No ingress_rule ⇒ sealed by default; every verb but reflect denies.
        let (k, fid, dir) = await openBridge(sealed: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        try? "a = 1\n".write(
            to: dir.appendingPathComponent("m.py"), atomically: true, encoding: .utf8)
        let r = await patch(k, fid, "m.py", block("a = 1", "a = 2"))
        #expect(r["reason"].asString == "unauthorized")
        #expect(read(dir, "m.py") == "a = 1\n")
    }
}
