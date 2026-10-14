// The MODEL VIEW of the conversation — the two trimmers that shape what the
// prompt carries, and nothing else. Ported from python `ai_core/core.py:708-831`,
// which is canonical, and byte-identical to Rust's `history_view.rs`.
//
// THE STORE IS NEVER TOUCHED. Both functions are pure: they take the durable
// turns and return a VIEW. The durable JSONL sidecar keeps every turn verbatim,
// so the trace stays complete and `recall` can page anything back.
//
// Why trim at all. Repeated whole-state replies (reflect above all) are a large
// share of a conversation. That bulk is what fills the window; a full window is
// what forces compaction; compaction is what put the run's goal at risk. Trimming
// it attacks the cause, not the symptom.
//
// The rule is narrow on purpose. A reply may be dropped only when a LATER reply
// makes it strictly redundant, which requires the verb to return WHOLE state
// rather than an answer to a question:
//
//   reflect · list_agents · ps · keys → whole-state. The newest reply IS the
//       truth; older copies describe a state that no longer exists and can only
//       mislead. Safe to drop.
//   read · list · status · get → PARAMETERISED. Two `read`s on the same agent are
//       different files; the tag carries no args, so deduping them would silently
//       delete content. NEVER dropped.

import FantasticJSON
import Foundation

/// The verbs whose reply is WHOLE STATE, so a later reply strictly supersedes an
/// earlier one. Python `_WHOLE_STATE_READS`.
let WHOLE_STATE_READS: Set<String> = ["reflect", "list_agents", "ps", "keys"]

/// The leading `\S+` run of `s` and the rest, or nil when the run is empty.
private func takeRun(_ s: Substring) -> (Substring, Substring)? {
    let end = s.firstIndex(where: { $0.isWhitespace }) ?? s.endIndex
    if end == s.startIndex { return nil }
    return (s[s.startIndex..<end], s[end...])
}

/// Read a `[reply to <pid> · <agent> <verb>…]` tag off the FRONT of `s` and answer
/// `(agent, verb)`. A hand-scan of python's
/// `^\[reply to \S+ · (\S+) (\S+)[^\]]*\]` — including the trailing `[^\]]*`, which
/// is what absorbs the optional ` · N of M in batch <token>` suffix.
///
/// No regex: one matcher used by two trimmers does not earn a dependency, and a
/// hand-scan is what Rust does for the same reason.
func parseReplyTag(_ s: String) -> (agent: String, verb: String)? {
    let full = Substring(s)
    guard full.hasPrefix("[reply to ") else { return nil }
    var rest = full.dropFirst("[reply to ".count)
    guard let (_, afterPid) = takeRun(rest) else { return nil }
    rest = afterPid
    guard rest.hasPrefix(" · ") else { return nil }
    rest = rest.dropFirst(3)
    guard let (agent, afterAgent) = takeRun(rest) else { return nil }
    rest = afterAgent
    guard rest.hasPrefix(" ") else { return nil }
    rest = rest.dropFirst()
    guard let (verbRun, tail) = takeRun(rest) else { return nil }
    // The verb group is `[^\]\s]+`, so a `]` ENDS it: the first bracket in the run is
    // the tag's own closer, never one carried in from the body. With `\S+`,
    // `[reply to x · notes reflect]\n{"verbs":["read"]}` would take `reflect]` and let
    // `[^\]]*\]` close on a bracket inside the BODY — the verb would match no
    // vocabulary, and both trimmers would skip every reply containing a bracket
    // (every reflect: bodies are JSON).
    if let i = verbRun.firstIndex(of: "]") {
        if i == verbRun.startIndex { return nil }
        return (String(agent), String(verbRun[verbRun.startIndex..<i]))
    }
    // No bracket in the run: the tag can still close later (the ` · N of M in batch`
    // suffix), but there must BE a `]` ahead or the regex would not have matched.
    if !tail.contains("]") { return nil }
    return (String(agent), String(verbRun))
}

/// True when `s` OPENS with a reply tag's head (`[reply to \S+ · `) — the
/// lookahead half of python's `_BLOCK_SPLIT`.
private func startsABlock(_ s: Substring) -> Bool {
    guard s.hasPrefix("[reply to ") else { return false }
    guard let (_, rest) = takeRun(s.dropFirst("[reply to ".count)) else { return false }
    return rest.hasPrefix(" · ")
}

/// The pid off a block's first line — what `grep` and `strip` address a body by.
/// Python `_PID_TAG` (`^\[reply to (\S+) · `), as a hand scan like the rest of
/// this file. The token comes back AS THE TAG WRITES IT (`42`, `-`, a foreign
/// `notes:a1`); the store keys an int pid by its decimal text, so the string is
/// the one key both sides agree on.
func pidOf(_ block: String) -> String? {
    let full = Substring(block)
    guard full.hasPrefix("[reply to ") else { return nil }
    guard let (pid, rest) = takeRun(full.dropFirst("[reply to ".count)) else { return nil }
    guard rest.hasPrefix(" · ") else { return nil }
    return String(pid)
}

/// Python's `len()` of a string — code points, not grapheme clusters and not
/// bytes. A stub says how much left the view in the units python counts.
func pyLen(_ s: Substring) -> Int { s.unicodeScalars.count }
func pyLen(_ s: String) -> Int { s.unicodeScalars.count }

/// A reply block reduced to its stub: the tag line, then what it cost.
/// Python `strategies/strip._stub` — ONE OWNER, shared by `strip` and
/// `applyRes`, so a stub renders the same bytes wherever it is produced:
///     [reply to pid · notes read · "survey chunk 2"] (stripped: 6800 bytes — recall by pid)
func stub(_ block: String) -> String {
    // `head, _, body = block.partition("\n")`: no newline ⇒ the whole block is the
    // head and the body is empty.
    if let nl = block.firstIndex(of: "\n") {
        let head = block[block.startIndex..<nl]
        let body = block[block.index(after: nl)...]
        return "\(head) (stripped: \(pyLen(body)) bytes — recall by pid)"
    }
    return "\(block) (stripped: 0 bytes — recall by pid)"
}

/// Split a delivered turn into its reply BLOCKS. Python `_reply_blocks` /
/// `_BLOCK_SPLIT` (`\n\n(?=\[reply to \S+ · )`).
///
/// A delivered turn carries ONE OR MORE blocks — a batch arrives whole, one block
/// per call, joined by a blank line. The trimmers below work on the BLOCK, the unit
/// they were written for; treating the whole turn as one reply either deletes
/// siblings (the reflect cut) or trims nothing (a count guard skips
/// every multi-reply turn).
func replyBlocks(_ content: String) -> [String] {
    var out: [String] = []
    var start = content.startIndex
    var from = content.startIndex
    while let r = content.range(of: "\n\n", range: from..<content.endIndex) {
        if startsABlock(content[r.upperBound...]) {
            out.append(String(content[start..<r.lowerBound]))
            start = r.upperBound
            from = r.upperBound
        } else {
            // The engine advances ONE char and retries — `\n\n\n[reply to …`
            // splits at the SECOND newline pair, not the first.
            from = content.index(after: r.lowerBound)
        }
    }
    out.append(String(content[start...]))
    return out
}

/// The turn's content, or "".
private func contentOf(_ turn: JSON) -> String {
    turn["content"].asString ?? ""
}

/// A `docs` reply the model has ANSWERED is cut to this stub.
/// The body itself stays HELD this run under its pid: the model
/// keeps the lines it needs with its own `grep`, and those small replies stay in its
/// view. Python `_DOCS_DROPPED`.
func docsDropped(of: String, tier: String, pid: String, verb: String) -> String {
    "[docs of=\(of) tier=\(tier) — dropped from your view; held this run: send yourself "
        + "<type>grep</type> <ids><item>\(pid)</item></ids> <query>\(verb)</query>]"
}

/// A REAL verb of this docs body for the grep example: the first that is not
/// `reflect`/`boot`, else the first. A placeholder example is copied verbatim:
/// `<query>verb</query>` sent as written matches only the `<verbs>` lines.
/// `nil` when the body lists no verbs (nothing to keep, so nothing is held).
/// Python `example_verb`.
func exampleVerb(_ body: String) -> String? {
    let all = body.components(separatedBy: "\n")
    guard let start = all.firstIndex(of: "<verbs>") else { return nil }
    var names: [String] = []
    var i = start + 1
    while i < all.count, !all[i].hasPrefix("</verbs>") {
        let line = all[i]
        i += 1
        // A top-level entry `<name>…</name>` starts a line; its doc may run on over
        // more lines, up to its own closer.
        guard line.hasPrefix("<"), let close = line.firstIndex(of: ">") else { continue }
        let name = String(line[line.index(after: line.startIndex)..<close])
        guard let first = name.unicodeScalars.first, first == "_" || (first.isASCII && CharacterSet.letters.contains(first)),
            name.unicodeScalars.allSatisfy({ $0 == "_" || ($0.isASCII && CharacterSet.alphanumerics.contains($0)) })
        else { continue }
        let closer = "</\(name)>"
        if !line[line.index(after: close)...].contains(closer) {
            while i < all.count, !all[i].contains(closer) { i += 1 }
            guard i < all.count else { break }
            i += 1
        }
        names.append(name)
    }
    return names.first { $0 != "reflect" && $0 != "boot" } ?? names.first
}

/// The FIRST line of a docs body: it is erased after the next message, and how to
/// keep what the model will need. Python `_DOCS_HEAD`.
func docsHead(pid: Int64, verb: String) -> String {
    "[erased from your view after your next message — decide now which verbs you need "
        + "and keep them in this same turn: send yourself <type>grep</type> "
        + "<ids><item>\(pid)</item></ids> <query>\(verb)</query> <width>1000</width>]\n"
}

/// The `<of>`/`<tier>` lines of a rendered docs body. Python `_DOCS_FIELD`
/// (`^<(of|tier)>([^<\n]*)</\1>$`, multiline).
private func docsFields(_ block: String) -> [String: String] {
    var out: [String: String] = [:]
    for line in block.split(separator: "\n", omittingEmptySubsequences: false) {
        for key in ["of", "tier"] {
            let open = "<\(key)>", close = "</\(key)>"
            guard line.hasPrefix(open), line.hasSuffix(close),
                line.count >= open.count + close.count
            else { continue }
            let inner = line.dropFirst(open.count).dropLast(close.count)
            if !inner.contains("<"), out[key] == nil { out[key] = String(inner) }
        }
    }
    return out
}

/// Replace every `docs` reply body the model has answered with its stub. The
/// reply in the last turn (not answered yet) stays whole; a refusal (no `of` in
/// the body) stays whole. Python `drop_read_docs`.
func dropReadDocs(_ history: [JSON]) -> [JSON] {
    var out: [JSON] = []
    for (i, turn) in history.enumerated() {
        let content = contentOf(turn)
        let answered = history[(i + 1)...].contains { $0["role"].asString == "assistant" }
        if turn["role"].asString != "user" || !answered || parseReplyTag(content) == nil {
            out.append(turn)
            continue
        }
        let blocks = replyBlocks(content)
        let trimmed = blocks.map { b -> String in
            guard let tag = parseReplyTag(b), tag.verb == "docs" else { return b }
            let fields = docsFields(b)
            guard let of = fields["of"], let verb = exampleVerb(b) else { return b }
            let head = b.split(separator: "\n", maxSplits: 1, omittingEmptySubsequences: false).first
                .map(String.init) ?? b
            let pid = head.dropFirst("[reply to ".count).split(separator: " ").first.map(String.init) ?? "-"
            return head + "\n" + docsDropped(of: of, tier: fields["tier"] ?? "", pid: pid, verb: verb)
        }
        out.append(trimmed == blocks ? turn : withField(turn, "content", .string(trimmed.joined(separator: "\n\n"))))
    }
    return out
}

/// A refusal of the model's own call — a verb its target does not answer
/// (`unhandled`, naming that verb), or an argument it did not send (`missing_arg`
/// naming the `arg`) — names where the fix is, `docs`, when it carries no hint of
/// its own. Decided on the machine `reason`, never on prose. Python
/// `point_to_docs`.
func pointToDocs(target: String, verb: String?, result: JSON) -> JSON {
    guard case .object(var o) = result, o["hint"] == nil else { return result }
    let reason = o["reason"]?.asString
    if reason == "unhandled", let verb, o["verb"]?.asString == verb {
        o["hint"] = .string("the verbs \(target) answers: send kernel {type: docs, of: '\(target)'}")
        return .object(o)  // a NEW key lands last, as python's `{**result, "hint": …}`
    }
    if reason == "missing_arg", let verb, let arg = o["arg"], pyTruthy(arg) {
        o["hint"] = .string(
            "how to call it: send kernel {type: docs, of: '\(target)', verb: '\(verb)', tier: detailed}")
        return .object(o)  // a NEW key lands last, as python's `{**result, "hint": …}`
    }
    return result
}

/// Drop whole-state read replies that a later reply to the same `(agent, verb)`
/// supersedes. Newest wins; order is otherwise preserved. Python
/// `dedupe_stale_reads`.
///
/// This is dedup, NOT stripping: the newest reply stays verbatim, so the model
/// keeps the facts (verbs, ids, tree) and has no reason to re-reflect — re-fetching
/// what it lost is exactly the loop we are trying to avoid.
func dedupeStaleReads(_ history: [JSON]) -> [JSON] {
    var seen: Set<String> = []
    var keepRev: [JSON] = []
    for turn in history.reversed() {
        let content = contentOf(turn)
        if turn["role"].asString != "user" || parseReplyTag(content) == nil {
            keepRev.append(turn)
            continue
        }
        // Per BLOCK — a turn may carry a whole batch. A superseded whole-state read
        // drops alone; its siblings stay. A turn whose every block is superseded
        // drops entirely.
        var keptRev: [String] = []
        var changed = false
        for b in replyBlocks(content).reversed() {
            if let tag = parseReplyTag(b), WHOLE_STATE_READS.contains(tag.verb) {
                let key = "\(tag.agent)\u{1F}\(tag.verb)"
                if seen.contains(key) {
                    changed = true
                    continue  // a later reply already carries this state
                }
                seen.insert(key)
            }
            keptRev.append(b)
        }
        if keptRev.isEmpty { continue }
        if !changed {
            keepRev.append(turn)
        } else {
            keepRev.append(
                withField(turn, "content", .string(keptRev.reversed().joined(separator: "\n\n"))))
        }
    }
    return keepRev.reversed()
}

/// A reply whose BODY is byte-identical to an EARLIER one (the same file read
/// again, the same listing) is shown as a one-line stub citing the first — the
/// first copy stays whole. Only bodies worth the saving are stubbed, and a
/// digested reply (its origin line) is already one line. The view restarts at a
/// clear, so a read after a clear is new again. Python `dedupe_repeat_bodies`.
func dedupeRepeatBodies(_ history: [JSON]) -> [JSON] {
    var seen: [String: JSON] = [:]
    var out: [JSON] = []
    for turn in history {
        let content = pyTruthy(turn["content"]) ? (turn["content"].asString ?? "") : ""
        guard turn["role"].asString == "user", parseReplyTag(content) != nil else {
            out.append(turn)
            continue
        }
        var changed = false
        var kept: [String] = []
        for b in replyBlocks(content) {
            // `head, _, body = b.partition("\n")`
            let head: String
            let body: String
            if let nl = b.firstIndex(of: "\n") {
                head = String(b[b.startIndex..<nl])
                body = String(b[b.index(after: nl)...])
            } else {
                head = b
                body = ""
            }
            if pyLen(body) < 400 || body.hasPrefix("(settled") {
                kept.append(b)
                continue
            }
            if let first = seen[body] {
                kept.append(
                    "\(head)\n[identical to the reply in row \(pyStr(first)) — the same "
                        + "\(pyLen(body)) bytes, not repeated]")
                changed = true
            } else {
                seen[body] = turn["id"]
                kept.append(b)
            }
        }
        out.append(changed ? withField(turn, "content", .string(kept.joined(separator: "\n\n"))) : turn)
    }
    return out
}
