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

    private func pid() -> String {
        KernelTaskLocals.currentTask?.value ?? "op:00000000"
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
        let tmp = "\(path).tmp.\(pid().replacingOccurrences(of: ":", with: "_"))"
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
        let ckptId = pid()
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
                    + "<new lines> / `>>>>>>> REPLACE`; each SEARCH must match the current "
                    + "document EXACTLY ONCE; the patch rides as RAW bytes (real newlines, "
                    + "nothing escaped — use send_binary with `bytes` LAST)"),
            "verbs": .object([
                "reflect": .string("no args — identity, verb guide, and the wired default surface."),
                "boot": .string("no args — no-op; this agent holds no state to hydrate."),
                "read": .string(
                    "args: path:str (req), on:str?, offset:int=0 (line), limit:int=200 — {path, lines:[{n,text}], total, eof, version}; refuses binary."),
                "write": .string(
                    "args: path:str (req), bytes:bytes (req — the whole document, raw), on:str?, if_match:str?, checkpoint:bool? — atomic tmp+rename; {path, size, version, ckpt_id?}."),
                "patch": .string(
                    "args: path:str (req), bytes:bytes (req — the utf-8 patch, raw), on:str?, if_match:str?, dry_run:bool?, checkpoint:bool? — SEARCH/REPLACE blocks, each matching EXACTLY ONCE; all-or-nothing; {path, patched:true, hunks, size, version, ckpt_id?}."),
                "append": .string(
                    "args: path:str (req), bytes:bytes (req — one raw chunk), on:str? — appends (creates if absent); {path, size, version}."),
                "checkpoint": .string(
                    "args: path:str (req), on:str? — commit a pre-image now, mutation-free; {ckpt_id, path, version}."),
                "checkpoints": .string(
                    "args: path:str (req), on:str? — the pre-images held for a document; {path, ckpts:[{id, size}]}."),
                "restore": .string(
                    "args: path:str (req), ckpt_id:str (req), on:str?, if_match:str? — atomic restore of the pre-image (or delete, if it was absent); {restored:true, path, version|deleted}."),
                "drop": .string(
                    "args: path:str (req), ckpt_id:str (req), on:str? — discard one pre-image (retention is the caller's); {dropped:true}."),
            ]),
        ])
    }

    private func readReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: String
        switch pathOf(payload, "read") {
        case .failure(let e): return e
        case .success(let p): path = p
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
        let off = max(0, Int(payload["offset"].asInt ?? 0))
        let limit = max(1, Int(payload["limit"].asInt ?? 200))
        let end = min(off + limit, allLines.count)
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
            "eof": .bool(off + limit >= allLines.count),
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
            return Refusal.missing("write", "bytes (the document, raw bytes)")
        }
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
        if let ckptId { out["ckpt_id"] = .string(ckptId) }
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
            return Refusal.missing("append", "bytes (one raw chunk)")
        }
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

    private func patchReply(_ kernel: Kernel, fb: String, payload: JSON, blob: Data) async
        -> JSON
    {
        let path: String
        switch pathOf(payload, "patch") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        if blob.isEmpty { return Refusal.missing("patch", "bytes (the utf-8 patch)") }
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
        let raw: Data
        switch await readAll(kernel, fb: fb, path: path) {
        case .failure(let e): return e
        case .success(let d): raw = d
        }
        if let want = payload["if_match"].asString {
            let have = version(raw)
            if have != want { return versionMismatch(want: want, have: have) }
        }
        guard var working = String(data: raw, encoding: .utf8) else {
            return Refusal.bad("patch", "path", "a utf-8 text file")
        }
        // Apply against the EVOLVING content; each SEARCH must be unique at its
        // turn. Stop at the first failure and write NOTHING (all-or-nothing).
        for (idx, hunk) in hunks.enumerated() {
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
                            "index": .integer(Int64(idx)), "reason": .string(detail),
                        ])
                    ]),
                ])
            }
            working = replaceFirst(search, with: replace, in: working)
        }
        let new = Data(working.utf8)
        if payload["dry_run"].asBool == true {
            return .object([
                "path": .string(path), "dry_run": .bool(true),
                "hunks": .integer(Int64(hunks.count)), "version": .string(version(raw)),
            ])
        }
        var ckptId: String?
        if payload["checkpoint"].asBool == true {
            switch await takeCheckpoint(kernel, fb: fb, path: path) {
            case .failure(let e): return e
            case .success(let id): ckptId = id
            }
        }
        if let err = await writeAtomic(kernel, fb: fb, path: path, data: new) { return err }
        var out: OrderedDictionary<String, JSON> = [
            "path": .string(path),
            "patched": .bool(true),
            "hunks": .integer(Int64(hunks.count)),
            "size": .integer(Int64(new.count)),
            "version": .string(version(new)),
        ]
        if let ckptId { out["ckpt_id"] = .string(ckptId) }
        return .object(out)
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
                "ckpt_id": .string(id), "path": .string(path),
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
            ckpts.append(.object(["id": .string(name), "size": f["size"]]))
        }
        return .object(["path": .string(path), "ckpts": .array(ckpts)])
    }

    private func restoreReply(_ kernel: Kernel, fb: String, payload: JSON) async -> JSON {
        let path: String
        switch pathOf(payload, "restore") {
        case .failure(let e): return e
        case .success(let p): path = p
        }
        guard let ckptId = payload["ckpt_id"].asString, !ckptId.isEmpty else {
            return Refusal.missing("restore", "ckpt_id")
        }
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
        guard let ckptId = payload["ckpt_id"].asString, !ckptId.isEmpty else {
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
            "dropped": .bool(true), "ckpt_id": .string(ckptId), "path": .string(path),
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
                return .object(["error": .string("text_file: unknown type \"\(verb)\"")])
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
            return .object(["error": .string("text_file: unknown type \"\(verb)\"")])
        }
    }

    public func handle(agentId: AgentId, payload: JSON, kernel: Kernel) async throws -> JSON? {
        guard let agent = kernel.agent(agentId) else {
            return .object(["error": .string("no agent \(agentId)")])
        }
        let verb = payload["type"].asString ?? ""
        // The byte verbs carry RAW BYTES — binary channel only (mirrors the
        // file_bridge redirect for its stream verbs).
        if verb == "write" || verb == "patch" || verb == "append" {
            return .object([
                "error": .string(
                    "\(verb) carries raw bytes — call it on the binary channel "
                        + "(send_binary), not text send")
            ])
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
