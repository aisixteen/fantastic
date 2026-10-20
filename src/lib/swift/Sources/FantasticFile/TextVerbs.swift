// The text verbs — the editing organ of `file_bridge`. Python
// `file_bridge/tools.py` (`_read`/`_write`/`_edit`/`_append`/`_undo`) is canonical.
//
// A text document is edited HERE, with the caller's OWN grants on this bridge.
// Every byte goes through `Fs` under this bridge's clamp, `allow` list and
// `readonly`; a commit is `fsWriteBytes` (a tmp sibling + atomic rename), the tmp
// judged by the TARGET path's exposure.

import CryptoKit
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// Lines per `read` page when the caller names no `limit`.
let READ_LIMIT: Int64 = 200

private let hStart = "<<<<<<< SEARCH"
private let hDiv = "======="
private let hEnd = ">>>>>>> REPLACE"

/// ONE undo slot per (kernel, bridge, path): the bytes the last `write`/`edit`/
/// `append` replaced (nil = the file did not exist). `undo` swaps file ↔ slot, so a
/// second `undo` is redo. RAM only — nothing on disk, lost on restart. Held by the
/// bundle instance and keyed by the kernel too: two kernels in one process may
/// both have a bridge `fs`.
/// TODO: memory leak possible — one slot per edited path, unbounded paths × size.
final class UndoSlots: @unchecked Sendable {
    private let lock = NSLock()
    private var slots: [String: Data?] = [:]

    /// `.some(nil)` = a slot saying "the file did not exist"; `nil` = no slot.
    func get(_ key: String) -> Data?? {
        lock.lock()
        defer { lock.unlock() }
        return slots[key]
    }

    func set(_ key: String, _ value: Data?) {
        lock.lock()
        slots[key] = .some(value)
        lock.unlock()
    }

    /// The bridge is going away — its slots go with it.
    func clear(_ kernel: Kernel, bridge: String) {
        let prefix = "\(kernel.serial)\u{0}\(bridge)\u{0}"
        lock.lock()
        slots = slots.filter { !$0.key.hasPrefix(prefix) }
        lock.unlock()
    }
}

func undoKey(_ kernel: Kernel, _ bridge: String, _ path: String) -> String {
    "\(kernel.serial)\u{0}\(bridge)\u{0}\(allowNorm(path) ?? path)"
}

// ─── per-document serialization ─────────────────────────────────
// A mutate is read → verify → commit; two concurrent mutates on ONE document
// interleaved there would both derive from the same pre-image and the second
// commit would silently discard the first. One lock per (bridge, path), held
// from the first read through the commit; `if_match` is checked UNDER it. A
// multi-file edit takes its locks in sorted order so overlapping transactions
// cannot deadlock. Entries are dropped when nobody holds or waits.
actor DocumentLocks {
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
            next.resume()  // ownership passes straight to the waiter
        } else {
            held.remove(key)
        }
    }
}

/// Run `body` with every `(kernel, bridge, path)` lock held, taken in sorted key
/// order. Python `_held`.
private func held(
    _ locks: DocumentLocks, _ kernel: Kernel, _ bridge: String, _ paths: [String],
    _ body: () async -> JSON
) async -> JSON {
    let keys = Array(Set(paths.map { undoKey(kernel, bridge, $0) })).sorted()
    for k in keys { await locks.acquire(k) }
    let out = await body()
    for k in keys.reversed() { await locks.release(k) }
    return out
}

/// The on-demand version: a content hash, computed, never stored.
func textVersion(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined().prefix(16).lowercased()
}

/// python `str.count(sub)`: non-overlapping hits, and an EMPTY needle hits once
/// per position — `len(s) + 1` CODE POINTS, not grapheme clusters.
private func countOccurrences(of needle: String, in haystack: String) -> Int {
    if needle.isEmpty { return haystack.unicodeScalars.count + 1 }
    var count = 0
    var lower = haystack.startIndex
    while let r = haystack.range(of: needle, options: [.literal], range: lower..<haystack.endIndex)
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

private struct HunkParseError: Error { let message: String }

/// SEARCH/REPLACE blocks → (search, replace) pairs, taken VERBATIM (the marker
/// lines excluded). Throws on a block that opens but does not close. Python
/// `_parse_hunks`.
private func parseHunks(_ text: String) throws(HunkParseError) -> [(String, String)] {
    let lines = text.components(separatedBy: "\n")
    var hunks: [(String, String)] = []
    var i = 0
    let n = lines.count
    while i < n {
        if lines[i] != hStart {
            i += 1
            continue
        }
        var j = i + 1
        var search: [String] = []
        while j < n && lines[j] != hDiv {
            search.append(lines[j])
            j += 1
        }
        if j >= n { throw HunkParseError(message: "a SEARCH block has no `=======` divider") }
        j += 1
        var replace: [String] = []
        while j < n && lines[j] != hEnd {
            replace.append(lines[j])
            j += 1
        }
        if j >= n { throw HunkParseError(message: "a block has no closing `>>>>>>> REPLACE`") }
        hunks.append((search.joined(separator: "\n"), replace.joined(separator: "\n")))
        i = j + 1
    }
    return hunks
}

extension FileBundle {
    /// The verbs whose document may ride the BINARY channel.
    static let textBodyVerbs: Set<String> = ["write", "edit", "append"]

    /// python `_doc` — the document: the binary body when the call carried one,
    /// else a `str` on the text channel, utf-8-encoded (a spawned program's
    /// ctrl-fd wire is JSON and has no bytes type).
    private func docArg(_ verb: String, _ payload: JSON, _ blob: Data?, _ what: String)
        -> Result<Data, Refused>
    {
        if let blob { return .success(blob) }
        if case .string(let s) = payload["bytes"] { return .success(Data(s.utf8)) }
        return .failure(
            Refused(json: Refusal.missing(verb, "bytes (\(what) — str is utf-8-encoded)")))
    }

    /// The document's bytes, or nil when no file is there. Python `_current`.
    private func current(_ root: String, _ path: String, base: String) throws(FsError) -> Data? {
        try fsIsFile(root, path, base: base) ? try fsReadBytes(root, path, base: base) : nil
    }

    /// The on-demand optimistic guard: a refusal when `if_match` names another
    /// version (an absent document has no version and never matches). Python
    /// `_mismatch`.
    private func mismatch(_ path: String, _ want: JSON, _ have: Data?) -> JSON? {
        let haveVer = have.map { textVersion($0) }
        if let haveVer, want == .string(haveVer) { return nil }
        return .object([
            "error": .string("version mismatch — the document changed since you read it"),
            "reason": .string("version_mismatch"),
            "path": .string(path),
            "expected": want,
            "actual": haveVer.map { .string($0) } ?? .null,
        ])
    }

    /// `root` or a refusal: readonly for a mutate, a real path, the clamp and the
    /// allow list on both ends. Python `_text_gate`.
    private func textGate(_ verb: String, _ agent: Agent, _ path: String, mutate: Bool, base: String)
        -> JSON?
    {
        if mutate, pyTruthy(agent.metaValue(forKey: "readonly") ?? .null) {
            return readonlyRefusal()
        }
        if path.isEmpty { return Refusal.missing(verb, "path") }
        return gatePath(verb, root: rootStr(agent), allow: allowList(agent), path: path, base: base)
    }

    /// args: path, offset=0 (line), limit=200 — a line-numbered page.
    func textRead(_ agent: Agent, _ payload: JSON, base: String) -> JSON {
        let path: String
        let off: Int64
        let limit: Int64
        do {
            path = try pathArg("read", payload, "path")
            off = try intArg("read", payload, "offset", default: 0, minimum: 0)
            limit = try intArg("read", payload, "limit", default: READ_LIMIT, minimum: 1)
        } catch { return error.json }
        if let refused = textGate("read", agent, path, mutate: false, base: base) { return refused }
        let data: Data?
        do { data = try current(rootStr(agent), path, base: base) } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        guard let data else { return notFound("file \(pyRepr(path)) not found", path: path) }
        if data.contains(0) { return Refusal.bad("read", "path", "a text file (this one is binary)") }
        if pyUtf8DecodeError(data) != nil {
            return Refusal.bad("read", "path", "valid utf-8 (this file is not)")
        }
        var all = String(decoding: data, as: UTF8.self).components(separatedBy: "\n")
        // A trailing newline yields one phantom empty tail element — dropped so
        // `total` counts real lines.
        if all.last == "" { all.removeLast() }
        // Saturating: a caller may pass `limit` up to Int64.max (python has no
        // overflow; swift would trap on `off + limit`).
        let total = Int64(all.count)
        let lo = Int(min(off, total))
        let hi = limit >= total - Int64(lo) ? all.count : lo + Int(limit)
        let page = Array(all[lo..<hi])
        let ver = textVersion(data)
        var lines: [JSON] = []
        for (i, t) in page.enumerated() {
            lines.append(.object(["n": .integer(off + Int64(i) + 1), "text": .string(t)]))
        }
        var out: OrderedDictionary<String, JSON> = [:]
        out["path"] = .string(path)
        out["lines"] = .array(lines)
        out["total"] = .integer(Int64(all.count))
        out["eof"] = .bool(off >= total || limit >= total - off)
        // whether the file ends in a newline: a whole-document save keeps it as it was
        out["newline"] = .bool(data.last == 0x0A)
        out["version"] = .string(ver)
        // HOW TO GET THIS BODY AGAIN — one line; a reader that keeps no body keeps this.
        out["origin"] = .string(
            "\(agent.id.value) \(path) lines \(UInt64(off) + 1)-\(off + Int64(page.count)) version \(ver) — "
                + "read {path: \(path), offset: \(off), limit: \(limit)}; a different version "
                + "means the file changed since")
        return .object(out)
    }

    /// args: path, bytes (the whole document), if_match? — atomic whole write.
    func textWrite(_ agent: Agent, _ payload: JSON, blob: Data?, base: String, kernel: Kernel) async -> JSON {
        let path: String
        do { path = try pathArg("write", payload, "path") } catch { return error.json }
        if let refused = textGate("write", agent, path, mutate: true, base: base) { return refused }
        let data: Data
        switch docArg("write", payload, blob, "the whole document") {
        case .success(let d): data = d
        case .failure(let r): return r.json
        }
        let root = rootStr(agent)
        let id = agent.id.value
        return await held(docLocks, kernel, id, [path]) {
            let before: Data?
            do throws(FsError) {
                before = try current(root, path, base: base)
                let want = payload["if_match"]
                if want != .null, let err = mismatch(path, want, before) { return err }
                try fsWriteBytes(root, path, data, base: base)
            } catch {
                return Refusal.make(Refusal.transportError, error.message)
            }
            undoSlots.set(undoKey(kernel, id, path), before)
            return .object([
                "path": .string(path), "size": .integer(Int64(data.count)),
                "version": .string(textVersion(data)),
            ])
        }
    }

    /// args: path, bytes (one chunk) — appends (creates if absent).
    func textAppend(_ agent: Agent, _ payload: JSON, blob: Data?, base: String, kernel: Kernel) async -> JSON {
        let path: String
        do { path = try pathArg("append", payload, "path") } catch { return error.json }
        if let refused = textGate("append", agent, path, mutate: true, base: base) {
            return refused
        }
        let data: Data
        switch docArg("append", payload, blob, "one chunk") {
        case .success(let d): data = d
        case .failure(let r): return r.json
        }
        let root = rootStr(agent)
        let id = agent.id.value
        return await held(docLocks, kernel, id, [path]) {
            let before: Data?
            let size: Int64
            let whole: Data
            do throws(FsError) {
                before = try current(root, path, base: base)
                (_, size) = try fsWriteChunk(
                    root, path, data, offset: nil, truncate: false, base: base)
                whole = try fsReadBytes(root, path, base: base)
            } catch {
                return Refusal.make(Refusal.transportError, error.message)
            }
            undoSlots.set(undoKey(kernel, id, path), before)
            return .object([
                "path": .string(path), "size": .integer(size),
                "version": .string(textVersion(whole)),
            ])
        }
    }

    private struct PlannedEdit {
        let path: String
        let hunks: [(String, String)]
        let ifMatch: JSON
        var before = Data()
        var new = Data()
    }

    /// args: edits:[{path, hunks, if_match?}], bytes (ALL hunks), dry_run? — a
    /// TRANSACTION over every listed document.
    func textEdit(_ agent: Agent, _ payload: JSON, blob: Data?, base: String, kernel: Kernel) async -> JSON {
        if pyTruthy(agent.metaValue(forKey: "readonly") ?? .null) { return readonlyRefusal() }
        guard let edits = payload["edits"].asArray, !edits.isEmpty else {
            return Refusal.missing("edit", "edits (a list of {path, hunks, if_match?})")
        }
        let data: Data
        switch docArg("edit", payload, blob, "the patch — all hunks, in edits order") {
        case .success(let d): data = d
        case .failure(let r): return r.json
        }
        if data.isEmpty {
            return Refusal.missing("edit", "bytes (the patch — all hunks, in edits order)")
        }
        if pyUtf8DecodeError(data) != nil {
            return Refusal.bad("edit", "bytes", "valid utf-8 (the patch is text)")
        }
        let hunks: [(String, String)]
        do { hunks = try parseHunks(String(decoding: data, as: UTF8.self)) } catch {
            return Refusal.bad("edit", "bytes", "a well-formed patch — \(error.message)")
        }
        if hunks.isEmpty {
            return Refusal.bad(
                "edit", "bytes",
                "at least one `<<<<<<< SEARCH` / `=======` / `>>>>>>> REPLACE` block")
        }
        let root = rootStr(agent)
        let allow = allowList(agent)
        var plan: [PlannedEdit] = []
        var cursor = 0
        for (i, e) in edits.enumerated() {
            guard case .object(let o) = e, case .string(let p)? = o["path"], !p.isEmpty else {
                return Refusal.bad("edit", "edits", "edit \(i): an object with a `path`")
            }
            guard case .integer(let n)? = o["hunks"], n >= 1 else {
                return Refusal.bad("edit", "edits", "edit \(i) (\(pyRepr(p))): `hunks` (int >= 1)")
            }
            if plan.contains(where: { allowNorm($0.path) == allowNorm(p) }) {
                return Refusal.bad(
                    "edit", "edits",
                    "each path at most once — put all of a file's hunks in one edit")
            }
            if let gate = gatePath("edit", root: root, allow: allow, path: p, base: base) {
                return gate
            }
            let end = min(cursor + Int(n), hunks.count)
            plan.append(
                PlannedEdit(
                    path: p, hunks: cursor < end ? Array(hunks[cursor..<end]) : [],
                    ifMatch: o["if_match"] ?? .null))
            cursor += Int(n)
        }
        if cursor != hunks.count {
            return Refusal.bad(
                "edit", "edits",
                "hunk counts that sum to the blob's blocks — edits claim \(cursor), "
                    + "the patch holds \(hunks.count)")
        }
        let id = agent.id.value
        let dryRun = pyTruthy(payload["dry_run"])
        let planned = plan
        return await held(docLocks, kernel, id, planned.map { $0.path }) {
            editTxn(id, root, planned, dryRun: dryRun, base: base, kernel: kernel)
        }
    }

    /// The locked half of `edit`: verify every hunk against the CURRENT document
    /// (`if_match` checked here, under the lock), then commit all; a mid-commit
    /// failure restores every document already written from its pre-image.
    private func editTxn(
        _ id: String, _ root: String, _ planIn: [PlannedEdit], dryRun: Bool, base: String,
        kernel: Kernel
    )
        -> JSON
    {
        var plan = planIn
        do {
            for k in plan.indices {
                let e = plan[k]
                guard let raw = try current(root, e.path, base: base) else {
                    return notFound("file \(pyRepr(e.path)) not found", path: e.path)
                }
                if e.ifMatch != .null, let err = mismatch(e.path, e.ifMatch, raw) { return err }
                if pyUtf8DecodeError(raw) != nil {
                    return Refusal.bad("edit", "edits", "\(pyRepr(e.path)): a utf-8 text file")
                }
                var working = String(decoding: raw, as: UTF8.self)
                for (idx, (search, replace)) in e.hunks.enumerated() {
                    let hits = countOccurrences(of: search, in: working)
                    if hits != 1 {
                        var failed: OrderedDictionary<String, JSON> = [:]
                        failed["path"] = .string(e.path)
                        failed["index"] = .integer(Int64(idx))
                        failed["reason"] = .string(
                            hits == 0 ? "search text not found" : "search text matches \(hits) places")
                        return .object([
                            "error": .string("a hunk did not apply — nothing written"),
                            "reason": .string(hits == 0 ? "no_match" : "ambiguous"),
                            "failed": .array([.object(failed)]),
                        ])
                    }
                    working = replaceFirst(search, with: replace, in: working)
                }
                plan[k].before = raw
                plan[k].new = Data(working.utf8)
            }
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        if dryRun {
            return .object([
                "dry_run": .bool(true),
                "edits": .array(
                    plan.map {
                        .object(["path": .string($0.path), "hunks": .integer(Int64($0.hunks.count))])
                    }),
            ])
        }
        var written: [PlannedEdit] = []
        for e in plan {
            do {
                try fsWriteBytes(root, e.path, e.new, base: base)
            } catch {
                // ROLL BACK what already landed — every pre-image is in hand.
                for w in written { try? fsWriteBytes(root, w.path, w.before, base: base) }
                guard case .object(var r) = Refusal.make(Refusal.transportError, error.message)
                else { return .null }
                r["restored"] = .array(written.map { .string($0.path) })
                r["failed_path"] = .string(e.path)
                return .object(r)
            }
            written.append(e)
        }
        for e in plan { undoSlots.set(undoKey(kernel, id, e.path), e.before) }
        return .object([
            "edited": .bool(true),
            "edits": .array(
                plan.map {
                    .object([
                        "path": .string($0.path), "hunks": .integer(Int64($0.hunks.count)),
                        "size": .integer(Int64($0.new.count)),
                        "version": .string(textVersion($0.new)),
                    ])
                }),
        ])
    }

    /// args: path — swap the document with what the last write/edit/append
    /// replaced (a second undo is redo).
    func textUndo(_ agent: Agent, _ payload: JSON, base: String, kernel: Kernel) async -> JSON {
        let path: String
        do { path = try pathArg("undo", payload, "path") } catch { return error.json }
        if let refused = textGate("undo", agent, path, mutate: true, base: base) { return refused }
        let root = rootStr(agent)
        let id = agent.id.value
        let key = undoKey(kernel, id, path)
        return await held(docLocks, kernel, id, [path]) {
            guard let slot = undoSlots.get(key) else {
                return Refusal.make(
                    Refusal.notFound, "undo: nothing to undo for \(pyRepr(path))",
                    hint: "the slot holds what the LAST write/edit/append on this path "
                        + "replaced; it lives in RAM, one per path, and is gone after a restart",
                    extra: [("path", .string(path))])
            }
            let now: Data?
            do throws(FsError) {
                now = try current(root, path, base: base)
                if let slot {
                    try fsWriteBytes(root, path, slot, base: base)
                } else if now != nil {
                    try fsRemove(root, path, recursive: false, base: base)
                }
            } catch {
                return Refusal.make(Refusal.transportError, error.message)
            }
            undoSlots.set(key, now)
            if let slot {
                return .object([
                    "undone": .bool(true), "path": .string(path),
                    "version": .string(textVersion(slot)),
                ])
            }
            return .object(["undone": .bool(true), "path": .string(path), "deleted": .bool(true)])
        }
    }
}
