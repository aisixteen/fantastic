// text_file — a stateless text-document agent over the file stream.
//
// Swift mirror of the canonical python `text_file` bundle (canonical; on
// disagreement this file is wrong).
//
// Format-free by design: a document is whatever text the caller writes. This
// agent supplies the TEXT operations — line-read, atomic write, SEARCH/REPLACE
// patch, append — plus ON-DEMAND versioning (`version` content hash; an
// `if_match` guard that fires only when passed) and ON-DEMAND checkpoints
// (pre-images as plain files under `.ckpt/`; retention is the caller's).
//
// NO DISK CODE HERE. Every byte routes through the file_bridge AGENT named by
// `on` (per call) or the record's `target_id`, over its stream verbs — plain
// GATED sends, sender = this agent (the dispatch scope), so the operator wires
// both ends and the leg's clamp + ingress rule apply unchanged. Never
// bookkeeping context: these verbs take caller-supplied paths, which is exactly
// what the confused-deputy rule forbids escalating.
//
// Checkpoint ids ARE the dispatch's kernel task pid — an INT, one id space with
// `ps`, the trace and the chat turn that caused the mutation.

import CryptoKit
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// `Result<_, JSON>` failure carrier: the refusal JSON IS the error value.
/// File-local conformance — nothing outside this bundle throws a JSON.
extension JSON: Swift.Error {}

private let CKPT_DIR = ".ckpt"
private let CHUNK = 65536

/// ABSENT ⇒ `def`. Present, an integer, ≥ `minimum` ⇒ that value. ANYTHING ELSE
/// (a bool, a string, a float, an explicit null, a value under the bound) ⇒ a
/// `bad_arg` refusal.
///
/// A present-but-wrong number is a TYPO, and `asInt ?? 0` hid it: `read
/// {offset: "12"}` silently paged from the top and told the caller nothing.
private func intArg(
    _ payload: JSON, _ key: String, _ def: Int64, _ verb: String, _ minimum: Int64
) -> Result<Int64, JSON> {
    guard let raw = payload.asObject?[key] else { return .success(def) }
    if case .integer(let n) = raw, n >= minimum { return .success(n) }
    let want = minimum == 0 ? "a non-negative int" : "an int >= \(minimum)"
    return .failure(Refusal.bad(verb, key, want))
}

// ─── per-document serialization ─────────────────────────────────
// A mutate is read → verify → commit with awaits between; two concurrent
// mutates on ONE document interleaved there would both derive from the same
// pre-image and the second commit would silently discard the first. One lock
// per (target surface, path), held from the first read through the rename;
// `if_match` is checked UNDER it. Multi-edit patches take theirs in sorted key
// order so two transactions over overlapping sets cannot deadlock. Entries are
// pruned when nobody holds or waits, so the map never grows.
private actor DocumentLocks {
    static let shared = DocumentLocks()
    private var held: Set<String> = []
    private var waiters: [String: [CheckedContinuation<Void, Never>]] = [:]

    func acquire(_ key: String) async {
        if !held.contains(key) {
            held.insert(key)
            return
        }
        await withCheckedContinuation { (c: CheckedContinuation<Void, Never>) in
            waiters[key, default: []].append(c)
        }
    }

    func release(_ key: String) {
        if var queue = waiters[key], !queue.isEmpty {
            let next = queue.removeFirst()
            waiters[key] = queue.isEmpty ? nil : queue
            // ownership passes straight to the waiter; `held` stays set
            next.resume()
        } else {
            held.remove(key)
        }
    }
}

/// Run `body` with every `(fb, path)` lock held, taken in sorted key order.
private func locked(_ fb: String, _ paths: [String], _ body: () async -> JSON) async -> JSON {
    let keys = Array(Set(paths.map { "\(fb)\u{0}\($0)" })).sorted()
    for k in keys { await DocumentLocks.shared.acquire(k) }
    let out = await body()
    for k in keys.reversed() { await DocumentLocks.shared.release(k) }
    return out
}

public struct TextFileBundle: AgentBundle {
    public let name = "text_file"

    public init() {}

    public var readme: String? { Self.readmeText }

    /// Refuse a write this bundle cannot honour: `target_id`, when present, must
    /// be a non-empty agent-id string (the default surface this agent operates
    /// on). Mirrors py `text_file.validate_meta`.
    public func validateMeta(
        _ meta: OrderedDictionary<String, JSON>, record: OrderedDictionary<String, JSON>
    ) -> String? {
        guard let t = meta["target_id"] else { return nil }
        if case .string(let s) = t, !s.isEmpty { return nil }
        return "target_id must be a non-empty file_bridge agent id"
    }

    /// The verbs that operate on a target surface, in python's `VERBS` order.
    /// Anything outside this set (plus `reflect`/`boot`) is an unknown type —
    /// python has no `shutdown` here, so `shutdown` is refused like any typo.
    static let surfaceVerbs: [String] = [
        "read", "write", "patch", "append", "checkpoint", "checkpoints", "restore", "drop",
    ]

    // ─── the target surface ─────────────────────────────────────

    private func target(_ agent: Agent, _ payload: JSON) -> String? {
        if let on = payload["on"].asString, !on.isEmpty { return on }
        return agent.metaValue(forKey: "target_id")?.asString.flatMap { $0.isEmpty ? nil : $0 }
    }

    private func unwired(_ verb: String) -> JSON {
        .object([
            "error": .string(
                "\(verb): no target surface — pass on=<file_bridge id> or set "
                    + "`target_id` on this agent's record"),
            "reason": .string("not_found"),
            "hint": .string(
                "the operator wires both ends: this agent's can_call names the "
                    + "leg, and the leg's callable_by accepts this agent"),
        ])
    }

    private func version(_ data: Data) -> String {
        let digest = SHA256.hash(data: data)
        return digest.map { String(format: "%02x", $0) }.joined().prefix(16).lowercased()
    }

    private func pid() -> Int64 {
        // Python `_pid`: `current_task() or 0` — a number, 0 outside any task.
        KernelTaskLocals.currentTask?.value ?? 0
    }

    /// python's version-mismatch body. The per-edit form inside `patch` carries
    /// `path` BETWEEN `reason` and `expected`; the standalone `if_match` guard
    /// has no `path` at all. `expected` echoes the caller's raw value.
    private func versionMismatch(want: JSON, have: String) -> JSON {
        .object([
            "error": .string("version mismatch — the document changed since you read it"),
            "reason": .string("version_mismatch"),
            "expected": want,
            "actual": .string(have),
        ])
    }

    /// The per-edit form, `path` BETWEEN `reason` and `expected`.
    private func versionMismatchAt(path: JSON, want: JSON, have: String) -> JSON {
        .object([
            "error": .string("version mismatch — the document changed since you read it"),
            "reason": .string("version_mismatch"),
            "path": path,
            "expected": want,
            "actual": .string(have),
        ])
    }

    // ─── the leg-IO seam (three functions, one per channel shape) ─
    // EVERY byte this agent moves crosses the file_bridge leg through exactly
    // these functions, which are channel-shaped rather than verb-shaped: each
    // call site already speaks (path, offset, length) or (path, offset,
    // truncate, bytes) and reads its outcome through `legFailure`. Moving that
    // traffic onto the batch (C37) therefore replaces the two transport bodies
    // below and touches no verb.

    /// One `read_stream` chunk: the leg's reply plus the chunk itself, which
    /// rides the binary body here and `r["bytes"]` in python.
    private func legReadChunk(
        _ kernel: Kernel, fb: String, path: JSON, offset: Int, length: Int
    ) async -> (JSON, Data) {
        await kernel.sendWithBinary(
            AgentId(fb),
            .object([
                "type": .string("read_stream"), "path": path,
                "offset": .integer(Int64(offset)), "length": .integer(Int64(length)),
            ]), Data())
    }

    /// One `write_stream` chunk. An absent `offset`/`truncate` means append at
    /// the end (creating the file) — python omits both keys for `append`.
    private func legWriteChunk(
        _ kernel: Kernel, fb: String, path: JSON, offset: Int?, truncate: Bool?, bytes: Data
    ) async -> JSON {
        var p: OrderedDictionary<String, JSON> = [
            "type": .string("write_stream"), "path": path,
        ]
        if let offset { p["offset"] = .integer(Int64(offset)) }
        if let truncate { p["truncate"] = .bool(truncate) }
        return await kernel.sendWithBinary(AgentId(fb), .object(p), bytes).0
    }

    /// A leg call that moves no bytes (`rename`, `delete`, `list`).
    private func legCall(_ kernel: Kernel, fb: String, _ payload: JSON) async -> JSON {
        await kernel.send([Item(AgentId(fb), payload)])[0].result
    }

    /// python's leg-reply check, spelled once for every channel: a reply that is
    /// not an object IS the failure (`{"error": str(r)}`, so a `None` reads as
    /// `"None"`); an object fails only on a TRUTHY `error`.
    /// NOT a ternary: `JSON` is `ExpressibleByNilLiteral`, so `? r : nil` types
    /// both arms as `JSON` and a SUCCESS would come back as `.some(.null)`.
    private func legFailure(_ r: JSON) -> JSON? {
        guard case .object = r else { return .object(["error": .string(pyStr(r))]) }
        if pyTruthy(r["error"]) { return r }
        return nil
    }

    // ─── stream I/O (every byte through the gated leg) ──────────

    private func readAll(_ kernel: Kernel, fb: String, path: JSON) async -> Result<Data, JSON> {
        var raw = Data()
        var offset = 0
        while true {
            let (r, body) = await legReadChunk(
                kernel, fb: fb, path: path, offset: offset, length: CHUNK)
            if let err = legFailure(r) { return .failure(err) }
            raw.append(body)
            // python: `if r.get("eof") or not b: break` — an ABSENT `eof` is
            // falsy and keeps pulling, it does not end the file.
            if pyTruthy(r["eof"]) || body.isEmpty { break }
            offset = Int(r["next_offset"].asInt ?? Int64(offset + body.count))
        }
        return .success(raw)
    }

    private func writeChunks(_ kernel: Kernel, fb: String, path: JSON, data: Data) async -> JSON? {
        var off = 0
        var first = true
        while first || off < data.count {
            let end = min(off + CHUNK, data.count)
            let chunk = data.subdata(in: off..<end)
            let r = await legWriteChunk(
                kernel, fb: fb, path: path, offset: off, truncate: first, bytes: chunk)
            if let err = legFailure(r) { return err }
            off += chunk.count
            first = false
        }
        return nil
    }

    private func writeAtomic(_ kernel: Kernel, fb: String, path: JSON, data: Data) async -> JSON? {
        // python f-strings the path, whatever the notation typed it as.
        let tmp = "\(pyStr(path)).tmp.\(pid())"
        if let err = await writeChunks(kernel, fb: fb, path: .string(tmp), data: data) {
            return err
        }
        return legFailure(
            await legCall(
                kernel, fb: fb,
                .object([
                    "type": .string("rename"), "old_path": .string(tmp), "new_path": path,
                ])))
    }

    /// python `_ckpt_path`: `.ckpt/<path, `/` flattened>/<ckpt id>`. Both halves
    /// go through python's `f"{…}"`, so an id the notation typed as an int
    /// (`<ckpt_id>29</ckpt_id>`) renders exactly as python spells it.
    private func ckptPath(_ path: JSON, _ ckptId: JSON) -> String {
        "\(CKPT_DIR)/\(pyStr(path).replacingOccurrences(of: "/", with: "__"))/\(pyStr(ckptId))"
    }

    /// The caller's `ckpt_id`, verbatim — a ckpt id IS a pid, and a pid is a
    /// NUMBER, so a model writing `<ckpt_id>29</ckpt_id>` hands over an integer
    /// and demanding a string only refuses the model its own checkpoint back
    /// (measured 2026-09-10 on `undo:rust`). Only a FALSY value is missing,
    /// python's `if not ckpt_id` — `0` included, since 0 is never minted.
    private func ckptArg(_ payload: JSON) -> JSON? {
        let v = payload["ckpt_id"]
        // NOT a ternary — see `legFailure`.
        if pyTruthy(v) { return v }
        return nil
    }

    /// A `.ckpt` dir entry's id: python `int(name) if name.isdigit() else name`.
    /// `str.isdigit()` is per-scalar Unicode, not ASCII, so `"٣"` reads as 3;
    /// anything else stays the raw name.
    private func ckptId(fromName name: String) -> JSON {
        var value: Int64 = 0
        var any = false
        for s in name.unicodeScalars {
            guard s.properties.numericType == .decimal,
                let d = s.properties.numericValue.map({ Int64($0) })
            else { return .string(name) }
            let (m, mo) = value.multipliedReportingOverflow(by: 10)
            guard !mo else { return .string(name) }
            let (a, ao) = m.addingReportingOverflow(d)
            guard !ao else { return .string(name) }
            value = a
            any = true
        }
        return any ? .integer(value) : .string(name)
    }

    /// python `want is not None and _version(raw) != want`: the guard fires
    /// whenever `if_match` is PRESENT and non-null, and a non-string value can
    /// never equal a hex digest, so it always mismatches.
    private func matches(_ want: JSON, _ have: String) -> Bool {
        if case .string(let s) = want { return s == have }
        return false
    }

    private func ifMatchGuard(_ kernel: Kernel, fb: String, path: JSON, payload: JSON)
        async -> JSON?
    {
        let want = payload["if_match"]
        if want.isNull { return nil }
        switch await readAll(kernel, fb: fb, path: path) {
        case .failure(let e): return e
        case .success(let data):
            let have = version(data)
            // NOT a ternary: JSON is ExpressibleByNilLiteral, so `? nil :` would
            // coerce to `.some(.null)` and a MATCH would read as a failure.
            if matches(want, have) { return nil }
            return versionMismatch(want: want, have: have)
        }
    }

    /// Commit the pre-image BEFORE any mutate touches the target. A file that
    /// does not exist yet checkpoints as ABSENT (restore = delete).
    private func takeCheckpoint(_ kernel: Kernel, fb: String, path: JSON) async
        -> Result<Int64, JSON>
    {
        let id = pid()
        var data: Data?
        switch await readAll(kernel, fb: fb, path: path) {
        case .success(let d): data = d
        case .failure(let e):
            if e["reason"].asString == "not_found" { data = nil } else { return .failure(e) }
        }
        let dest = ckptPath(path, .integer(id))
        if let err = await writeChunks(
            kernel, fb: fb, path: .string(dest), data: data ?? Data())
        {
            return .failure(err)
        }
        if data == nil {
            if let err = await writeChunks(
                kernel, fb: fb, path: .string(dest + ".absent"), data: Data())
            {
                return .failure(err)
            }
        }
        return .success(id)
    }

    // ─── patch grammar (moved from file_bridge — same guarantees) ─

    private static let hStart = "<<<<<<< SEARCH"
    private static let hDiv = "======="
    private static let hEnd = ">>>>>>> REPLACE"

    private struct PatchParseError: Error { let message: String }

    private func parseHunks(_ text: String) throws -> [(String, String)] {
        let lines = text.components(separatedBy: "\n")
        var hunks: [(String, String)] = []
        var i = 0
        let n = lines.count
        while i < n {
            if lines[i] != Self.hStart {
                i += 1
                continue
            }
            var j = i + 1
            var search: [String] = []
            while j < n && lines[j] != Self.hDiv {
                search.append(lines[j])
                j += 1
            }
            if j >= n { throw PatchParseError(message: "a SEARCH block has no `=======` divider") }
            j += 1
            var replace: [String] = []
            while j < n && lines[j] != Self.hEnd {
                replace.append(lines[j])
                j += 1
            }
            if j >= n {
                throw PatchParseError(message: "a block has no closing `>>>>>>> REPLACE`")
            }
            hunks.append((search.joined(separator: "\n"), replace.joined(separator: "\n")))
            i = j + 1
        }
        return hunks
    }

    /// python `str.count(sub)`: non-overlapping hits, and an EMPTY needle hits
    /// once per position — `len(s) + 1` CODE POINTS, not grapheme clusters
    /// (`"a\r\nb"` is 4 code points and matches 5 places).
    private func countOccurrences(of needle: String, in haystack: String) -> Int {
        if needle.isEmpty { return haystack.unicodeScalars.count + 1 }
        var count = 0
        var lower = haystack.startIndex
        while let r = haystack.range(
            of: needle, options: [.literal], range: lower..<haystack.endIndex)
        {
            count += 1
            lower = r.upperBound
        }
        return count
    }

    /// python `str.replace(old, new, 1)`: an EMPTY needle inserts at the start.
    private func replaceFirst(_ needle: String, with replacement: String, in haystack: String)
        -> String
    {
        if needle.isEmpty { return replacement + haystack }
        guard let r = haystack.range(of: needle, options: [.literal]) else { return haystack }
        return haystack.replacingCharacters(in: r, with: replacement)
    }

    // ─── verbs ──────────────────────────────────────────────────

    /// python `path = payload.get("path", ""); if not path: return missing(…)`.
    /// Only a FALSY path is missing; a truthy non-string (the tag grammar types
    /// `<path>2024</path>` as an int) travels to the leg VERBATIM and earns the
    /// leg's own `bad_arg`, which is the refusal that names the real problem.
    private func pathOf(_ payload: JSON, _ verb: String) -> Result<JSON, JSON> {
        let p = payload["path"]
        guard pyTruthy(p) else { return .failure(Refusal.missing(verb, "path")) }
        return .success(p)
    }

    /// python's `bytes` argument, channel-agnostic: the binary body when the
    /// call carried one, else the JSON leg's text courtesy (a `str` IS the
    /// document, utf-8 — ai_core encodes for the MODEL, but a spawned program's
    /// ctrl-fd wire is JSON and has no bytes type). Anything else reads as
    /// ABSENT, which each byte verb refuses AFTER its path/edits check.
    private func bytesArg(_ payload: JSON, _ blob: Data?) -> Data? {
        if let blob, !blob.isEmpty { return blob }
        if case .string(let s) = payload["bytes"] { return Data(s.utf8) }
        return blob
    }

    /// The lock key for a document: python keys `_LOCKS` on the raw path value.
    private func lockKey(_ path: JSON) -> String { pyStr(path) }

    private func reflect(_ agent: Agent) -> JSON {
        // python `rec.get("description") or <default>` — an EMPTY description is
        // falsy and falls back to the default sentence.
        let described = agent.metaValue(forKey: "description") ?? .null
        let sentence: JSON =
            pyTruthy(described)
            ? described
            : .string(
                "Text documents on a file surface — any format, the caller's choice: "
                    + "line-read, atomic write, SEARCH/REPLACE patch, append; versioning "
                    + "and checkpoints ON DEMAND.")
        return .object([
            "id": .string(agent.id.value),
            "sentence": sentence,
            "target_id": agent.metaValue(forKey: "target_id") ?? .null,
            "patch_grammar": .string(
                "one or more blocks: `<<<<<<< SEARCH` / <old lines> / `=======` / "
                    + "<new lines> / `>>>>>>> REPLACE`; each SEARCH must match its "
                    + "document EXACTLY ONCE. `patch` is a "
                    + "TRANSACTION over `edits:[{path, hunks, if_match?}]` — even one file "
                    + "rides the array; the blob holds every edit's hunks concatenated in "
                    + "`edits` order, split by the per-edit `hunks` count; any failure "
                    + "anywhere writes nothing"),
            "compose": .string(
                "every content-carrying verb (`write`, `patch`, `append`) takes its "
                    + "document in `bytes`, and `bytes` is VERBATIM: write the document "
                    + "between `<bytes>` and `</bytes>` exactly as it should land — real "
                    + "newlines, real quotes, nothing escaped, no fence — then close the "
                    + "call. The document ends at the first `</bytes>`. A patch names its "
                    + "edits as tags: <edits><item><path>a.py</path><hunks>2</hunks></item></edits>"),
            "verbs": .object([
                "reflect": .string("no args — identity, verb guide, and the wired default surface."),
                "boot": .string("no args — no-op; this agent holds no state to hydrate."),
                "read": .string("args: path:str (req), on:str?, offset:int=0 (line), limit:int=200 — {path, lines:[{n,text}], total, eof, version}; refuses binary."),
                "write": .string("args: path:str (req), bytes (req — the whole document; str is utf-8-encoded), on:str?, if_match:str?, checkpoint:bool? — atomic tmp+rename; {path, size, version, ckpt_id?}."),
                "patch": .string("args: edits:[{path:str, hunks:int, if_match:str?}] (req — even for ONE file), bytes (req — ALL hunks concatenated; str is utf-8-encoded), on:str?, dry_run:bool?, checkpoint:bool? — a TRANSACTION: verify every hunk of every edit, checkpoint all, apply all, drop the pre-images; any failure writes nothing (or restores what was written); {patched:true, edits:[{path,hunks,size,version}], ckpt_ids?}."),
                "append": .string("args: path:str (req), bytes (req — one chunk; str is utf-8-encoded), on:str? — appends (creates if absent); {path, size, version}."),
                "checkpoint": .string("args: path:str (req), on:str? — commit a pre-image now, mutation-free; {ckpt_id, path, version}."),
                "checkpoints": .string("args: path:str (req), on:str? — the pre-images held for a document; {path, ckpts:[{id, size}]}."),
                "restore": .string("args: path:str (req), ckpt_id:str (req), on:str?, if_match:str? — atomic restore of the pre-image (or delete, if it was absent); {restored:true, path, version|deleted}."),
                "drop": .string("args: path:str (req), ckpt_id:str (req), on:str? — discard one pre-image (retention is the caller's); {dropped:true}."),
            ]),
        ])
    }

    private func readReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: JSON
        switch pathOf(payload, "read") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        // Type the paging args BEFORE the read — a typo'd `offset` costs no IO.
        let off: Int
        switch intArg(payload, "offset", 0, "read", 0) {
        case .failure(let e): return e
        case .success(let v): off = Int(v)
        }
        let limit: Int
        switch intArg(payload, "limit", 200, "read", 1) {
        case .failure(let e): return e
        case .success(let v): limit = Int(v)
        }
        let data: Data
        switch await readAll(kernel, fb: fb, path: path) {
        case .failure(let e): return e
        case .success(let d): data = d
        }
        if data.contains(0) {
            return Refusal.bad("read", "path", "a text file (this one is binary)")
        }
        guard let text = String(data: data, encoding: .utf8) else {
            return Refusal.bad("read", "path", "valid utf-8 (this file is not)")
        }
        var allLines = text.components(separatedBy: "\n")
        if allLines.last == "" { allLines.removeLast() }
        // `limit: Int64.max` must page to eof, not trap on `off + limit`.
        let (span, overflow) = off.addingReportingOverflow(limit)
        let end = overflow ? allLines.count : min(span, allLines.count)
        var page: [JSON] = []
        if off < end {
            for (i, t) in allLines[off..<end].enumerated() {
                page.append(.object(["n": .integer(Int64(off + i + 1)), "text": .string(t)]))
            }
        }
        return .object([
            "path": path,
            "lines": .array(page),
            "total": .integer(Int64(allLines.count)),
            "eof": .bool(overflow || span >= allLines.count),
            "version": .string(version(data)),
        ])
    }

    private func writeReply(_ kernel: Kernel, fb: String, payload: JSON, blob: Data?) async
        -> JSON
    {
        let path: JSON
        switch pathOf(payload, "write") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        guard let data = bytesArg(payload, blob) else {
            return Refusal.missing("write", "bytes (the document — str is utf-8-encoded)")
        }
        return await locked(fb, [lockKey(path)]) {
            await writeLocked(kernel, fb: fb, path: path, payload: payload, data: data)
        }
    }

    /// The locked half of `write`: the `if_match` check happens here, under
    /// the document lock, and stays valid through the rename.
    private func writeLocked(
        _ kernel: Kernel, fb: String, path: JSON, payload: JSON, data: Data
    ) async -> JSON {
        if let err = await ifMatchGuard(kernel, fb: fb, path: path, payload: payload) {
            return err
        }
        var ckptId: Int64?
        if pyTruthy(payload["checkpoint"]) {
            switch await takeCheckpoint(kernel, fb: fb, path: path) {
            case .failure(let e): return e
            case .success(let id): ckptId = id
            }
        }
        if let err = await writeAtomic(kernel, fb: fb, path: path, data: data) {
            return err
        }
        var out: OrderedDictionary<String, JSON> = [
            "path": path,
            "size": .integer(Int64(data.count)),
            "version": .string(version(data)),
        ]
        // python `if ckpt_id:` — the pid 0 an operator call mints is falsy and
        // never reaches the reply.
        if let ckptId, ckptId != 0 { out["ckpt_id"] = .integer(ckptId) }
        return .object(out)
    }

    private func appendReply(_ kernel: Kernel, fb: String, payload: JSON, blob: Data?) async
        -> JSON
    {
        let path: JSON
        switch pathOf(payload, "append") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        guard let data = bytesArg(payload, blob) else {
            return Refusal.missing("append", "bytes (one chunk — str is utf-8-encoded)")
        }
        return await locked(fb, [lockKey(path)]) {
            // absent offset = append at end (creates the file)
            let r = await legWriteChunk(
                kernel, fb: fb, path: path, offset: nil, truncate: nil, bytes: data)
            if let err = legFailure(r) { return err }
            let whole: Data
            switch await readAll(kernel, fb: fb, path: path) {
            case .failure(let e): return e
            case .success(let d): whole = d
            }
            return .object([
                "path": path,
                "size": r["size"],
                "version": .string(version(whole)),
            ])
        }
    }

    private struct PlannedEdit {
        let path: String
        let hunks: [(String, String)]
        let ifMatch: JSON
        var newBytes: Data = Data()
    }

    private func patchReply(_ kernel: Kernel, fb: String, payload: JSON, blob: Data?) async
        -> JSON
    {
        // A TRANSACTION over `edits:[{path, hunks, if_match?}]` — even one file
        // rides the array (one shape, no single/multi drift; user decision
        // 2026-09-01). ONE raw hunk blob, split by the per-edit counts; verify
        // every hunk of every edit in memory, checkpoint all, apply all, then
        // self-clean the pre-images (`checkpoint:true` keeps them). A mid-apply
        // failure restores everything already written. Mirrors python (canonical).
        guard let edits = payload["edits"].asArray, !edits.isEmpty else {
            return Refusal.missing("patch", "edits (a list of {path, hunks, if_match?})")
        }
        guard let data = bytesArg(payload, blob), !data.isEmpty else {
            return Refusal.missing("patch", "bytes (the utf-8 patch — all hunks, in edits order)")
        }
        guard let patch = String(data: data, encoding: .utf8) else {
            return Refusal.bad("patch", "bytes", "valid utf-8 (the patch is text)")
        }
        let hunks: [(String, String)]
        do {
            hunks = try parseHunks(patch)
        } catch let e as PatchParseError {
            return Refusal.bad("patch", "bytes", "a well-formed patch — \(e.message)")
        } catch {
            return Refusal.bad("patch", "bytes", "a well-formed patch")
        }
        if hunks.isEmpty {
            return Refusal.bad(
                "patch", "bytes",
                "at least one `<<<<<<< SEARCH` / `=======` / `>>>>>>> REPLACE` block")
        }
        // ── address the one hunk stream: split by per-edit counts ──
        var plan: [PlannedEdit] = []
        var cursor: Int64 = 0
        for (i, e) in edits.enumerated() {
            guard case .object = e, let path = e["path"].asString, !path.isEmpty else {
                return Refusal.bad("patch", "edits", "edit \(i): an object with a `path`")
            }
            // `.integer` ONLY — `asInt` promotes a whole double, so `hunks: 1.0`
            // would pass here and be refused by python/rust. Same typing rule as
            // `intArg`: a present-but-wrong count is a typo, not a coercion.
            guard case .integer(let n) = e["hunks"], n >= 1 else {
                return Refusal.bad(
                    "patch", "edits", "edit \(i) (\(pyRepr(path))): `hunks` (int >= 1)")
            }
            if plan.contains(where: { $0.path == path }) {
                return Refusal.bad(
                    "patch", "edits",
                    "each path at most once — put all of a file's hunks in one edit")
            }
            // python slices `hunks[cursor:cursor + n]`: a slice past the end is
            // SHORT, never an error, so EVERY edit is still validated and the
            // reported sum is the whole claim, not the prefix that fit.
            let (sum, over) = cursor.addingReportingOverflow(n)
            let lo = Int(min(cursor, Int64(hunks.count)))
            let hi = over ? hunks.count : Int(min(sum, Int64(hunks.count)))
            plan.append(
                PlannedEdit(
                    path: path, hunks: lo < hi ? Array(hunks[lo..<hi]) : [],
                    ifMatch: e["if_match"]))
            cursor = over ? Int64.max : sum
        }
        if cursor != Int64(hunks.count) {
            return Refusal.bad(
                "patch", "edits",
                "hunk counts that sum to the blob's blocks — edits claim \(cursor), "
                    + "the patch holds \(hunks.count)")
        }
        // Every edit's document is held from the first read to the last rename.
        return await locked(fb, plan.map { $0.path }) {
            await patchTxn(kernel, fb: fb, plan: plan, payload: payload)
        }
    }

    /// The locked half of `patch`: verify every hunk against the CURRENT
    /// pre-image (the `if_match` check happens here, under the lock),
    /// checkpoint all, apply all, self-clean or keep the pre-images.
    private func patchTxn(_ kernel: Kernel, fb: String, plan planIn: [PlannedEdit], payload: JSON)
        async -> JSON
    {
        var plan = planIn
        // ── VERIFY ALL BEFORE ANY: compute every file's new content in memory ──
        for i in plan.indices {
            let raw: Data
            switch await readAll(kernel, fb: fb, path: .string(plan[i].path)) {
            case .failure(let e): return e
            case .success(let d): raw = d
            }
            let want = plan[i].ifMatch
            if !want.isNull {
                let have = version(raw)
                if !matches(want, have) {
                    return versionMismatchAt(
                        path: .string(plan[i].path), want: want, have: have)
                }
            }
            guard var working = String(data: raw, encoding: .utf8) else {
                return Refusal.bad("patch", "edits", "\(pyRepr(plan[i].path)): a utf-8 text file")
            }
            for (idx, hunk) in plan[i].hunks.enumerated() {
                let (search, replace) = hunk
                let hits = countOccurrences(of: search, in: working)
                if hits != 1 {
                    let reason = hits == 0 ? "no_match" : "ambiguous"
                    let detail =
                        hits == 0 ? "search text not found" : "search text matches \(hits) places"
                    return .object([
                        "error": .string("a hunk did not apply — nothing written"),
                        "reason": .string(reason),
                        "failed": .array([
                            .object([
                                "path": .string(plan[i].path),
                                "index": .integer(Int64(idx)),
                                "reason": .string(detail),
                            ])
                        ]),
                    ])
                }
                working = replaceFirst(search, with: replace, in: working)
            }
            plan[i].newBytes = Data(working.utf8)
        }
        if pyTruthy(payload["dry_run"]) {
            return .object([
                "dry_run": .bool(true),
                "edits": .array(plan.map {
                    .object(["path": .string($0.path), "hunks": .integer(Int64($0.hunks.count))])
                }),
            ])
        }
        // ── CHECKPOINT ALL, then APPLY ALL; a mid-apply failure restores ──
        // everything already written. Pre-images are ALWAYS taken (they ARE
        // the rollback); `checkpoint:true` only KEEPS them.
        var ckpts: [(String, Int64)] = []
        for e in plan {
            switch await takeCheckpoint(kernel, fb: fb, path: .string(e.path)) {
            case .failure(let err): return err
            case .success(let id): ckpts.append((e.path, id))
            }
        }
        var written: [(String, Int64)] = []
        for (e, ck) in zip(plan, ckpts) {
            if let err = await writeAtomic(
                kernel, fb: fb, path: .string(e.path), data: e.newBytes)
            {
                for (path, id) in written {
                    await restoreFrom(kernel, fb: fb, path: .string(path), ckptId: .integer(id))
                }
                var out = err
                out["restored"] = .array(written.map { .string($0.0) })
                out["failed_path"] = .string(e.path)
                return out
            }
            written.append((e.path, ck.1))
        }
        var out: OrderedDictionary<String, JSON> = [
            "patched": .bool(true),
            "edits": .array(plan.map {
                .object([
                    "path": .string($0.path),
                    "hunks": .integer(Int64($0.hunks.count)),
                    "size": .integer(Int64($0.newBytes.count)),
                    "version": .string(version($0.newBytes)),
                ])
            }),
        ]
        if pyTruthy(payload["checkpoint"]) {
            out["ckpt_ids"] = .array(ckpts.map {
                .object(["path": .string($0.0), "ckpt_id": .integer($0.1)])
            })
        } else {
            for (path, id) in ckpts {
                await dropQuiet(kernel, fb: fb, path: .string(path), ckptId: .integer(id))
            }
        }
        return .object(out)
    }

    /// Rollback arm of the patch transaction: put the pre-image back (or
    /// delete a file that did not exist). Best-effort — it runs INSIDE a
    /// failure path that is already being reported.
    private func restoreFrom(_ kernel: Kernel, fb: String, path: JSON, ckptId: JSON) async {
        let src = ckptPath(path, ckptId)
        let (absent, _) = await legReadChunk(
            kernel, fb: fb, path: .string(src + ".absent"), offset: 0, length: 1)
        if legFailure(absent) == nil {
            _ = await legCall(
                kernel, fb: fb, .object(["type": .string("delete"), "path": path]))
            return
        }
        if case .success(let data) = await readAll(kernel, fb: fb, path: .string(src)) {
            _ = await writeAtomic(kernel, fb: fb, path: path, data: data)
        }
    }

    /// Self-cleaning arm: a COMMITTED transaction discards its pre-images.
    private func dropQuiet(_ kernel: Kernel, fb: String, path: JSON, ckptId: JSON) async {
        let src = ckptPath(path, ckptId)
        _ = await legCall(
            kernel, fb: fb, .object(["type": .string("delete"), "path": .string(src)]))
        _ = await legCall(
            kernel, fb: fb,
            .object(["type": .string("delete"), "path": .string(src + ".absent")]))
    }

    private func checkpointReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: JSON
        switch pathOf(payload, "checkpoint") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        let data: Data
        switch await readAll(kernel, fb: fb, path: path) {
        case .failure(let e): return e
        case .success(let d): data = d
        }
        switch await takeCheckpoint(kernel, fb: fb, path: path) {
        case .failure(let e): return e
        case .success(let id):
            return .object([
                "ckpt_id": .integer(id), "path": path,
                "version": .string(version(data)),
            ])
        }
    }

    private func checkpointsReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: JSON
        switch pathOf(payload, "checkpoints") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        let dir = "\(CKPT_DIR)/\(pyStr(path).replacingOccurrences(of: "/", with: "__"))"
        let r = await legCall(
            kernel, fb: fb, .object(["type": .string("list"), "path": .string(dir)]))
        if legFailure(r) != nil {
            // no dir yet = no checkpoints, honestly empty
            return .object(["path": path, "ckpts": .array([])])
        }
        var ckpts: [JSON] = []
        for f in r["files"].asArray ?? [] {
            guard f["type"].asString == "file",
                let name = f["name"].asString, !name.hasSuffix(".absent")
            else { continue }
            ckpts.append(.object(["id": ckptId(fromName: name), "size": f["size"]]))
        }
        return .object(["path": path, "ckpts": .array(ckpts)])
    }

    private func restoreReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: JSON
        switch pathOf(payload, "restore") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        guard let ckptId = ckptArg(payload) else {
            return Refusal.missing("restore", "ckpt_id")
        }
        return await locked(fb, [lockKey(path)]) {
            await restoreLocked(kernel, fb: fb, path: path, ckptId: ckptId, payload: payload)
        }
    }

    /// The locked half of `restore`: `if_match` checked under the document
    /// lock, then the atomic put-back (or delete).
    private func restoreLocked(
        _ kernel: Kernel, fb: String, path: JSON, ckptId: JSON, payload: JSON
    ) async -> JSON {
        if let err = await ifMatchGuard(kernel, fb: fb, path: path, payload: payload) {
            return err
        }
        let src = ckptPath(path, ckptId)
        // An `.absent` twin means the pre-image was NO FILE — restore deletes.
        let (absent, _) = await legReadChunk(
            kernel, fb: fb, path: .string(src + ".absent"), offset: 0, length: 1)
        if legFailure(absent) == nil {
            let r = await legCall(
                kernel, fb: fb, .object(["type": .string("delete"), "path": path]))
            if let err = legFailure(r) { return err }
            return .object([
                "restored": .bool(true), "path": path, "deleted": .bool(true),
            ])
        }
        let data: Data
        switch await readAll(kernel, fb: fb, path: .string(src)) {
        case .failure(let e):
            if e["reason"].asString == "not_found" {
                // python `f"…{ckpt_id!r} for {path!r}"` — the RAW values, so an
                // int id prints bare and a str id quoted.
                return Refusal.make(
                    Refusal.notFound,
                    "restore: no checkpoint \(pyRepr(ckptId)) for \(pyRepr(path))")
            }
            return e
        case .success(let d): data = d
        }
        if let err = await writeAtomic(kernel, fb: fb, path: path, data: data) { return err }
        return .object([
            "restored": .bool(true), "path": path, "version": .string(version(data)),
        ])
    }

    private func dropReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: JSON
        switch pathOf(payload, "drop") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        guard let ckptId = ckptArg(payload) else {
            return Refusal.missing("drop", "ckpt_id")
        }
        let src = ckptPath(path, ckptId)
        let r = await legCall(
            kernel, fb: fb, .object(["type": .string("delete"), "path": .string(src)]))
        if let err = legFailure(r) { return err }
        // best-effort: the `.absent` twin, if any (its absence is not an error)
        _ = await legCall(
            kernel, fb: fb,
            .object(["type": .string("delete"), "path": .string(src + ".absent")]))
        return .object([
            // python echoes the caller's `ckpt_id` AS GIVEN
            "dropped": .bool(true), "ckpt_id": ckptId, "path": path,
        ])
    }

    /// One dispatch used by BOTH channels. `blob` is the binary body when the
    /// call came over the binary channel and nil on the JSON channel; either
    /// way the byte verbs resolve their document with `bytesArg` AFTER the
    /// unwired and path/edits checks, which is python's order.
    private func dispatch(agent: Agent, payload: JSON, blob: Data?, kernel: Kernel) async
        -> JSON
    {
        let typed = payload["type"]
        let verb = typed.asString ?? ""
        if verb == "reflect" { return reflect(agent) }
        if verb == "boot" { return .null }
        guard Self.surfaceVerbs.contains(verb) else { return unknownType(typed) }
        guard let fb = target(agent, payload) else { return unwired(verb) }
        switch verb {
        case "read": return await readReply(kernel, fb: fb, payload: payload)
        case "write": return await writeReply(kernel, fb: fb, payload: payload, blob: blob)
        case "patch": return await patchReply(kernel, fb: fb, payload: payload, blob: blob)
        case "append": return await appendReply(kernel, fb: fb, payload: payload, blob: blob)
        case "checkpoint": return await checkpointReply(kernel, fb: fb, payload: payload)
        case "checkpoints": return await checkpointsReply(kernel, fb: fb, payload: payload)
        case "restore": return await restoreReply(kernel, fb: fb, payload: payload)
        default: return await dropReply(kernel, fb: fb, payload: payload)
        }
    }

    /// python `refusal(UNHANDLED, f"text_file: unknown type {t!r}", verb=t)` —
    /// the RAW type value, so a str is quoted and an int prints bare.
    private func unknownType(_ typed: JSON) -> JSON {
        Refusal.make(
            Refusal.unhandled, "text_file: unknown type \(pyRepr(typed))",
            extra: [("verb", typed)])
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        return await dispatch(agent: agent, payload: payload, blob: nil, kernel: kernel)
    }

    public func handleBinary(agentId: AgentId, header: JSON, blob: Data, kernel: Kernel)
        async throws -> (JSON?, Data)
    {
        guard let agent = kernel.agent(agentId) else {
            return (.object(["error": .string("no agent \(agentId)")]), Data())
        }
        return (await dispatch(agent: agent, payload: header, blob: blob, kernel: kernel), Data())
    }

    static let readmeText = """
        # text_file — text documents, any format, on any file surface

        A stateless TEXT-DOCUMENT agent. It owns no disk and no format: every byte
        rides a `send` to a `file_bridge` agent's stream verbs, so the running-dir
        clamp and the ingress gate apply unchanged, and what a document *contains* is
        entirely the caller's business — notes, markdown, JSON, source code. Memory
        here is whatever you write.

        Name the surface per call with `on: <file_bridge id>`, or set a default
        `target_id` on this agent's record. The operator wires BOTH ends: this agent's
        `can_call` names the leg, the leg's `callable_by` accepts this agent.

        Verbs: `read` (line-numbered, paged, rejects binary) · `write` (atomic:
        tmp + rename on the target) · `patch` (a TRANSACTION over
        `edits:[{path, hunks, if_match?}]` — even one file rides the array; verify
        every hunk of every edit, checkpoint all, apply all, self-clean; any failure
        writes nothing or restores what landed; fails loud on `no_match`/`ambiguous`) · `append` · and ON DEMAND:
        `checkpoint` / `checkpoints` / `restore` / `drop` (pre-images as plain files
        under `.ckpt/` on the same surface — visible, caller-retained) plus a
        `version` content-hash on every reply and an `if_match` guard on every mutate.

        Atomicity, honestly: this agent owns the TRANSACTION protocol (tmp→rename
        commit, checkpoint-before-mutate, verify-all-before-any-hunk); the file leg
        owns per-op atomicity (one rename). A crash mid-sequence leaves visible
        tmp/checkpoint debris — never lost data, never a half-written target.

        """
}
