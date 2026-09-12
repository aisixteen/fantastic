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
// Checkpoint ids ARE the dispatch's kernel task pid (`<agent>:<hex8>`).

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

    private func versionMismatch(want: String, have: String) -> JSON {
        .object([
            "error": .string("version mismatch — the document changed since you read it"),
            "reason": .string("version_mismatch"),
            "expected": .string(want),
            "actual": .string(have),
        ])
    }

    // ─── stream I/O (every byte through the gated leg) ──────────

    private func readAll(_ kernel: Kernel, fb: String, path: String) async
        -> Result<Data, JSON>
    {
        var raw = Data()
        var offset = 0
        while true {
            let (r, body) = await kernel.sendWithBinary(
                AgentId(fb),
                .object([
                    "type": .string("read_stream"), "path": .string(path),
                    "offset": .integer(Int64(offset)), "length": .integer(Int64(CHUNK)),
                ]), Data())
            guard case .object = r else {
                return .failure(.object(["error": .string("\(fb): no reply")]))
            }
            if r["error"].asString != nil { return .failure(r) }
            if !body.isEmpty { raw.append(body) }
            if r["eof"].asBool ?? true { break }
            if body.isEmpty { break }
            offset = Int(r["next_offset"].asInt ?? Int64(offset + body.count))
        }
        return .success(raw)
    }

    private func writeChunks(_ kernel: Kernel, fb: String, path: String, data: Data) async
        -> JSON?
    {
        var off = 0
        var first = true
        while first || off < data.count {
            let end = min(off + CHUNK, data.count)
            let chunk = data.subdata(in: off..<end)
            let (r, _) = await kernel.sendWithBinary(
                AgentId(fb),
                .object([
                    "type": .string("write_stream"), "path": .string(path),
                    "offset": .integer(Int64(off)), "truncate": .bool(first),
                ]), chunk)
            guard case .object = r else {
                return .object(["error": .string("\(fb): no reply")])
            }
            if r["error"].asString != nil { return r }
            off += chunk.count
            first = false
        }
        return nil
    }

    private func legCall(_ kernel: Kernel, fb: String, _ payload: JSON) async -> JSON {
        await kernel.send([Item(AgentId(fb), payload)])[0].result
    }

    private func writeAtomic(_ kernel: Kernel, fb: String, path: String, data: Data) async
        -> JSON?
    {
        let tmp = "\(path).tmp.\(pid())"
        if let err = await writeChunks(kernel, fb: fb, path: tmp, data: data) { return err }
        let r = await legCall(
            kernel, fb: fb,
            .object([
                "type": .string("rename"), "old_path": .string(tmp), "new_path": .string(path),
            ]))
        guard case .object = r else { return .object(["error": .string("\(fb): no reply")]) }
        if r["error"].asString != nil { return r }
        return nil
    }

    /// A checkpoint id from the payload, whatever the notation typed it as.
    ///
    /// A ckpt id IS a pid, and a pid is a NUMBER — so a model writing
    /// `<ckpt_id>29</ckpt_id>` hands over an integer, and reading it as a string
    /// only refuses the model its own checkpoint back ("restore: ckpt_id
    /// required", measured 2026-09-10 on `undo:rust`). Python accepts anything
    /// truthy here for the same reason.
    private func ckptArg(_ payload: JSON) -> String? {
        switch payload["ckpt_id"] {
        case .string(let s): return s.isEmpty ? nil : s
        case .integer(let n): return String(n)
        case .double(let d): return String(d)
        default: return nil
        }
    }

    /// A ckpt id on the wire: python emits the minted pid as an INT and a
    /// `.ckpt` dir entry as `int(name) if name.isdigit() else name`.
    private func ckptJSON(_ id: String) -> JSON {
        Int64(id).map(JSON.integer) ?? .string(id)
    }

    private func ckptPath(_ path: String, _ ckptId: String) -> String {
        "\(CKPT_DIR)/\(path.replacingOccurrences(of: "/", with: "__"))/\(ckptId)"
    }

    private func ifMatchGuard(_ kernel: Kernel, fb: String, path: String, payload: JSON)
        async -> JSON?
    {
        guard let want = payload["if_match"].asString else { return nil }
        switch await readAll(kernel, fb: fb, path: path) {
        case .failure(let e): return e
        case .success(let data):
            let have = version(data)
            // NOT a ternary: JSON is ExpressibleByNilLiteral, so `? nil :` would
            // coerce to `.some(.null)` and a MATCH would read as a failure.
            if have == want { return nil }
            return versionMismatch(want: want, have: have)
        }
    }

    /// Commit the pre-image BEFORE any mutate touches the target. A file that
    /// does not exist yet checkpoints as ABSENT (restore = delete).
    private func takeCheckpoint(_ kernel: Kernel, fb: String, path: String) async
        -> Result<String, JSON>
    {
        let ckptId = String(pid())
        var data: Data?
        switch await readAll(kernel, fb: fb, path: path) {
        case .success(let d): data = d
        case .failure(let e):
            if e["reason"].asString == "not_found" { data = nil } else { return .failure(e) }
        }
        let dest = ckptPath(path, ckptId)
        if let err = await writeChunks(kernel, fb: fb, path: dest, data: data ?? Data()) {
            return .failure(err)
        }
        if data == nil {
            if let err = await writeChunks(kernel, fb: fb, path: dest + ".absent", data: Data())
            {
                return .failure(err)
            }
        }
        return .success(ckptId)
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

    private func countOccurrences(of needle: String, in haystack: String) -> Int {
        if needle.isEmpty { return haystack.count + 1 }
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

    private func replaceFirst(_ needle: String, with replacement: String, in haystack: String)
        -> String
    {
        guard let r = haystack.range(of: needle, options: [.literal]) else { return haystack }
        return haystack.replacingCharacters(in: r, with: replacement)
    }

    // ─── verbs ──────────────────────────────────────────────────

    private func pathOf(_ payload: JSON, _ verb: String) -> Result<String, JSON> {
        guard let p = payload["path"].asString, !p.isEmpty else {
            return .failure(Refusal.missing(verb, "path"))
        }
        return .success(p)
    }

    private func reflect(_ agent: Agent) -> JSON {
        let sentence =
            agent.metaValue(forKey: "description")?.asString
            ?? "Text documents on a file surface — any format, the caller's choice: "
                + "line-read, atomic write, SEARCH/REPLACE patch, append; versioning "
                + "and checkpoints ON DEMAND."
        let targetId = agent.metaValue(forKey: "target_id")?.asString
        return .object([
            "id": .string(agent.id.value),
            "sentence": .string(sentence),
            "target_id": targetId.map { JSON.string($0) } ?? .null,
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
        let path: String
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
        if data.contains(0) { return Refusal.bad("read", "path", "a text file (this one is binary)") }
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
            "path": .string(path),
            "lines": .array(page),
            "total": .integer(Int64(allLines.count)),
            "eof": .bool(overflow || span >= allLines.count),
            "version": .string(version(data)),
        ])
    }

    private func writeReply(_ kernel: Kernel, fb: String, payload: JSON, blob: Data) async
        -> JSON
    {
        let path: String
        switch pathOf(payload, "write") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        if blob.isEmpty && payload["bytes"].isNull {
            return Refusal.missing("write", "bytes (the document — str is utf-8-encoded)")
        }
        return await locked(fb, [path]) {
            await writeLocked(kernel, fb: fb, path: path, payload: payload, blob: blob)
        }
    }

    /// The locked half of `write`: the `if_match` check happens here, under
    /// the document lock, and stays valid through the rename.
    private func writeLocked(
        _ kernel: Kernel, fb: String, path: String, payload: JSON, blob: Data
    ) async -> JSON {
        if let err = await ifMatchGuard(kernel, fb: fb, path: path, payload: payload) {
            return err
        }
        var ckptId: String?
        if payload["checkpoint"].asBool == true {
            switch await takeCheckpoint(kernel, fb: fb, path: path) {
            case .failure(let e): return e
            case .success(let id): ckptId = id
            }
        }
        if let err = await writeAtomic(kernel, fb: fb, path: path, data: blob) {
            return err
        }
        var out: OrderedDictionary<String, JSON> = [
            "path": .string(path),
            "size": .integer(Int64(blob.count)),
            "version": .string(version(blob)),
        ]
        if let ckptId { out["ckpt_id"] = ckptJSON(ckptId) }
        return .object(out)
    }

    private func appendReply(_ kernel: Kernel, fb: String, payload: JSON, blob: Data) async
        -> JSON
    {
        let path: String
        switch pathOf(payload, "append") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        if blob.isEmpty && payload["bytes"].isNull {
            return Refusal.missing("append", "bytes (one chunk — str is utf-8-encoded)")
        }
        return await locked(fb, [path]) {
            // absent offset = append at end (creates the file)
            let (r, _) = await kernel.sendWithBinary(
                AgentId(fb),
                .object(["type": .string("write_stream"), "path": .string(path)]), blob)
            guard case .object = r else { return .object(["error": .string("\(fb): no reply")]) }
            if r["error"].asString != nil { return r }
            let whole: Data
            switch await readAll(kernel, fb: fb, path: path) {
            case .failure(let e): return e
            case .success(let d): whole = d
            }
            return .object([
                "path": .string(path),
                "size": r["size"],
                "version": .string(version(whole)),
            ])
        }
    }

    private struct PlannedEdit {
        let path: String
        let hunks: [(String, String)]
        let ifMatch: String?
        var newBytes: Data = Data()
    }

    private func patchReply(_ kernel: Kernel, fb: String, payload: JSON, blob: Data) async
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
        if blob.isEmpty {
            return Refusal.missing("patch", "bytes (the utf-8 patch — all hunks, in edits order)")
        }
        guard let patch = String(data: blob, encoding: .utf8) else {
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
        var cursor = 0
        for (i, e) in edits.enumerated() {
            guard let path = e["path"].asString, !path.isEmpty else {
                return Refusal.bad("patch", "edits", "edit \(i): an object with a `path`")
            }
            // `.integer` ONLY — `asInt` promotes a whole double, so `hunks: 1.0`
            // would pass here and be refused by python/rust. Same typing rule as
            // `intArg`: a present-but-wrong count is a typo, not a coercion.
            guard case .integer(let n) = e["hunks"], n >= 1 else {
                return Refusal.bad("patch", "edits", "edit \(i) (\"\(path)\"): `hunks` (int >= 1)")
            }
            let n2 = Int(n)
            if plan.contains(where: { $0.path == path }) {
                return Refusal.bad(
                    "patch", "edits",
                    "each path at most once — put all of a file's hunks in one edit")
            }
            if cursor + n2 > hunks.count {
                cursor += n2
                break
            }
            plan.append(
                PlannedEdit(
                    path: path, hunks: Array(hunks[cursor..<cursor + n2]),
                    ifMatch: e["if_match"].asString))
            cursor += n2
        }
        if cursor != hunks.count || plan.count != edits.count {
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
            switch await readAll(kernel, fb: fb, path: plan[i].path) {
            case .failure(let e): return e
            case .success(let d): raw = d
            }
            if let want = plan[i].ifMatch {
                let have = version(raw)
                if have != want {
                    var out = versionMismatch(want: want, have: have)
                    out["path"] = .string(plan[i].path)
                    return out
                }
            }
            guard var working = String(data: raw, encoding: .utf8) else {
                return Refusal.bad("patch", "edits", "\"\(plan[i].path)\": a utf-8 text file")
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
        if payload["dry_run"].asBool == true {
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
        var ckpts: [(String, String)] = []
        for e in plan {
            switch await takeCheckpoint(kernel, fb: fb, path: e.path) {
            case .failure(let err): return err
            case .success(let id): ckpts.append((e.path, id))
            }
        }
        var written: [(String, String)] = []
        for (e, ck) in zip(plan, ckpts) {
            if let err = await writeAtomic(kernel, fb: fb, path: e.path, data: e.newBytes) {
                for (path, id) in written {
                    await restoreFrom(kernel, fb: fb, path: path, ckptId: id)
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
        if payload["checkpoint"].asBool == true {
            out["ckpt_ids"] = .array(ckpts.map {
                .object(["path": .string($0.0), "ckpt_id": ckptJSON($0.1)])
            })
        } else {
            for (path, id) in ckpts {
                await dropQuiet(kernel, fb: fb, path: path, ckptId: id)
            }
        }
        return .object(out)
    }

    /// Rollback arm of the patch transaction: put the pre-image back (or
    /// delete a file that did not exist). Best-effort — it runs INSIDE a
    /// failure path that is already being reported.
    private func restoreFrom(_ kernel: Kernel, fb: String, path: String, ckptId: String) async {
        let src = ckptPath(path, ckptId)
        let (absent, _) = await kernel.sendWithBinary(
            AgentId(fb),
            .object([
                "type": .string("read_stream"), "path": .string(src + ".absent"),
                "offset": .integer(0), "length": .integer(1),
            ]), Data())
        if case .object = absent, absent["error"].asString == nil {
            _ = await legCall(kernel, fb: fb, .object(["type": .string("delete"), "path": .string(path)]))
            return
        }
        if case .success(let data) = await readAll(kernel, fb: fb, path: src) {
            _ = await writeAtomic(kernel, fb: fb, path: path, data: data)
        }
    }

    /// Self-cleaning arm: a COMMITTED transaction discards its pre-images.
    private func dropQuiet(_ kernel: Kernel, fb: String, path: String, ckptId: String) async {
        let src = ckptPath(path, ckptId)
        _ = await legCall(kernel, fb: fb, .object(["type": .string("delete"), "path": .string(src)]))
        _ = await legCall(
            kernel, fb: fb, .object(["type": .string("delete"), "path": .string(src + ".absent")]))
    }

    private func checkpointReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: String
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
                "ckpt_id": ckptJSON(id), "path": .string(path),
                "version": .string(version(data)),
            ])
        }
    }

    private func checkpointsReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: String
        switch pathOf(payload, "checkpoints") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        let dir = "\(CKPT_DIR)/\(path.replacingOccurrences(of: "/", with: "__"))"
        let r = await legCall(
            kernel, fb: fb, .object(["type": .string("list"), "path": .string(dir)]))
        guard case .object = r, r["error"].asString == nil else {
            // no dir yet = no checkpoints, honestly empty
            return .object(["path": .string(path), "ckpts": .array([])])
        }
        var ckpts: [JSON] = []
        for f in r["files"].asArray ?? [] {
            guard f["type"].asString == "file",
                let name = f["name"].asString, !name.hasSuffix(".absent")
            else { continue }
            ckpts.append(.object(["id": ckptJSON(name), "size": f["size"]]))
        }
        return .object(["path": .string(path), "ckpts": .array(ckpts)])
    }

    private func restoreReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: String
        switch pathOf(payload, "restore") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        guard let ckptId = ckptArg(payload) else {
            return Refusal.missing("restore", "ckpt_id")
        }
        return await locked(fb, [path]) {
            await restoreLocked(kernel, fb: fb, path: path, ckptId: ckptId, payload: payload)
        }
    }

    /// The locked half of `restore`: `if_match` checked under the document
    /// lock, then the atomic put-back (or delete).
    private func restoreLocked(
        _ kernel: Kernel, fb: String, path: String, ckptId: String, payload: JSON
    ) async -> JSON {
        if let err = await ifMatchGuard(kernel, fb: fb, path: path, payload: payload) {
            return err
        }
        let src = ckptPath(path, ckptId)
        // An `.absent` twin means the pre-image was NO FILE — restore deletes.
        let (absent, _) = await kernel.sendWithBinary(
            AgentId(fb),
            .object([
                "type": .string("read_stream"), "path": .string(src + ".absent"),
                "offset": .integer(0), "length": .integer(1),
            ]), Data())
        if case .object = absent, absent["error"].asString == nil {
            let r = await legCall(
                kernel, fb: fb, .object(["type": .string("delete"), "path": .string(path)]))
            guard case .object = r, r["error"].asString == nil else { return r }
            return .object([
                "restored": .bool(true), "path": .string(path), "deleted": .bool(true),
            ])
        }
        let data: Data
        switch await readAll(kernel, fb: fb, path: src) {
        case .failure(let e):
            if e["reason"].asString == "not_found" {
                return .object([
                    "error": .string("restore: no checkpoint \"\(ckptId)\" for \"\(path)\""),
                    "reason": .string("not_found"),
                ])
            }
            return e
        case .success(let d): data = d
        }
        if let err = await writeAtomic(kernel, fb: fb, path: path, data: data) { return err }
        return .object([
            "restored": .bool(true), "path": .string(path), "version": .string(version(data)),
        ])
    }

    private func dropReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: String
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
        guard case .object = r, r["error"].asString == nil else { return r }
        // best-effort: the `.absent` twin, if any (its absence is not an error)
        _ = await legCall(
            kernel, fb: fb,
            .object(["type": .string("delete"), "path": .string(src + ".absent")]))
        return .object([
            // python echoes the caller's `ckpt_id` AS GIVEN
            "dropped": .bool(true), "ckpt_id": payload["ckpt_id"], "path": .string(path),
        ])
    }

    /// One dispatch used by BOTH channels — the byte verbs take the blob, the
    /// rest ignore it.
    private func dispatch(agent: Agent, payload: JSON, blob: Data, kernel: Kernel) async
        -> JSON
    {
        let verb = payload["type"].asString ?? ""
        if verb == "reflect" { return reflect(agent) }
        if verb == "boot" || verb == "shutdown" { return .null }
        guard let fb = target(agent, payload) else {
            switch verb {
            case "read", "write", "patch", "append", "checkpoint", "checkpoints",
                "restore", "drop":
                return unwired(verb)
            default:
                return unknownType(verb)
            }
        }
        switch verb {
        case "read": return await readReply(kernel, fb: fb, payload: payload)
        case "write": return await writeReply(kernel, fb: fb, payload: payload, blob: blob)
        case "patch": return await patchReply(kernel, fb: fb, payload: payload, blob: blob)
        case "append": return await appendReply(kernel, fb: fb, payload: payload, blob: blob)
        case "checkpoint": return await checkpointReply(kernel, fb: fb, payload: payload)
        case "checkpoints": return await checkpointsReply(kernel, fb: fb, payload: payload)
        case "restore": return await restoreReply(kernel, fb: fb, payload: payload)
        case "drop": return await dropReply(kernel, fb: fb, payload: payload)
        default:
            return unknownType(verb)
        }
    }

    private func unknownType(_ verb: String) -> JSON {
        Refusal.make(
            Refusal.unhandled, "text_file: unknown type \"\(verb)\"",
            extra: [("verb", .string(verb))])
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        let verb = payload["type"].asString ?? ""
        // The byte verbs carry RAW BYTES — binary channel, OR the text courtesy
        // (py-canonical, 2026-09-01): a str `bytes` in the payload IS the
        // document, utf-8. ai_core encodes for the MODEL; a spawned program's
        // ctrl-fd wire is JSON (no bytes type), so the str form is its one path.
        if verb == "write" || verb == "patch" || verb == "append" {
            if let text = payload["bytes"].asString {
                return await dispatch(
                    agent: agent, payload: payload, blob: Data(text.utf8), kernel: kernel)
            }
            return Refusal.missing(verb, "bytes (the document — str is utf-8-encoded)")
        }
        return await dispatch(agent: agent, payload: payload, blob: Data(), kernel: kernel)
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
        clamp and the ingress gate apply unchanged. Memory here is whatever you write.

        Name the surface per call with `on: <file_bridge id>`, or set a default
        `target_id` on this agent's record. The operator wires BOTH ends.

        Verbs: `read` (line-numbered, paged) · `write` (atomic tmp+rename) · `patch`
        (SEARCH/REPLACE blocks, all-or-nothing, fails loud) · `append` · ON DEMAND:
        `checkpoint` / `checkpoints` / `restore` / `drop` (pre-images as plain files
        under `.ckpt/`), a `version` content-hash on every reply, an `if_match` guard.
        """
}
