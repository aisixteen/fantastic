// The MODEL VIEW of the conversation — the two trimmers that shape what the
// prompt carries, and nothing else. Ported from python `ai_core/core.py:708-831`,
// which is canonical, and byte-identical to Rust's `history_view.rs`.
//
// THE STORE IS NEVER TOUCHED. Both functions are pure: they take the durable
// turns and return a VIEW. The durable JSONL sidecar keeps every turn verbatim,
// so the trace stays complete and `recall` can page anything back.
//
// Why trim at all. Measured 2026-08-20 (`live_plan`): reflect replies were 42% of
// the whole conversation — 73k of 174k chars — and 9 of 16 were a repeat of a
// target already reflected. That bulk is what fills the window; a full window is
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

/// A reflect the model has ALREADY ANSWERED is replaced by this stub. The model
/// was told to carry the facts it needs into that answer (by `REFLECT_NUDGE`,
/// which rides on the reflect reply itself), so the distillation IT wrote is what
/// survives — written by the reader, at read time, with the task in mind, which no
/// post-hoc summarizer can match. The stub NAMES the agent so "I have already
/// looked at this" stays true, and re-reflecting is always available if the
/// distillation turns out to be thin.
func reflectDropped(agent: String) -> String {
    "[reflect of \(agent) — full text dropped; your distillation follows]"
}

/// The distillation instruction, appended to every delivered `reflect` reply body
/// by `Routing`. Python `_REFLECT_NUDGE`, verbatim.
///
/// It rides ON the reply, not in the system block. Measured 2026-08-20
/// (`live_plan`, the passing run): with the instruction stated ONCE as system-block
/// policy, the model wrote a `SEEN` line for 0 of 10 reflects — and re-reflected the
/// SAME agent (`kernel_state`) 9 times in a 17-turn run. Policy sits thousands of
/// tokens from the data and applies to a turn that has not happened yet; this
/// arrives attached to the body it talks about, every time, at the moment it
/// applies. It is deliberately the LAST thing in the message: it is what the model
/// reads immediately before writing its answer.
let REFLECT_NUDGE =
    "\n[this body is TRANSIENT — after your next message it is replaced by a stub. "
    + "Open that message with the facts you will need, one compact line per agent:\n"
    + "SEEN <id> — <what it is for>; verbs: <the ones you intend to use>. "
    + "One reflect per agent is complete — reflecting it again yields nothing new; "
    + "the next useful call is an action.]"

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
    // the tag's own closer, never one carried in from the body. It was `\S+` until
    // 2026-09-04, which on `[reply to x · notes reflect]\n{"verbs":["read"]}` took
    // `reflect]` and let `[^\]]*\]` close on a bracket inside the BODY — the verb then
    // read `reflect]`, matched no vocabulary, and both trimmers silently did nothing for
    // every reply containing a bracket (which is every reflect: bodies are JSON).
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

/// Split a delivered turn into its reply BLOCKS. Python `_reply_blocks` /
/// `_BLOCK_SPLIT` (`\n\n(?=\[reply to \S+ · )`).
///
/// A delivered turn carries ONE OR MORE blocks — a batch arrives whole, one block
/// per call, joined by a blank line. The trimmers below work on the BLOCK, the unit
/// they were written for; treating the whole turn as one reply either deleted
/// siblings (`dropAnsweredReflects`) or trimmed nothing (a count guard that skipped
/// every multi-reply turn — tried 2026-08-25, superseded same day).
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

/// Python's `\w`, for the `\b` boundary either side of `SEEN`.
private func isWordChar(_ c: Character) -> Bool {
    c.isLetter || c.isNumber || c == "_"
}

/// What counts as "the model recorded it": the `SEEN <agent> — …` line the reflect
/// nudge asks for. Python `_DISTILLED` (`\bSEEN\b`, case-insensitive) — matched
/// leniently, because the point is whether facts were carried forward, not whether
/// a format was obeyed.
///
/// FAILS CLOSED: no match means the reflect body is KEPT.
func distilled(_ text: String) -> Bool {
    let chars = Array(text)
    guard chars.count >= 4 else { return false }
    for i in 0...(chars.count - 4) {
        guard String(chars[i..<(i + 4)]).lowercased() == "seen" else { continue }
        let beforeOK = i == 0 || !isWordChar(chars[i - 1])
        let afterOK = i + 4 >= chars.count || !isWordChar(chars[i + 4])
        if beforeOK && afterOK { return true }
    }
    return false
}

/// The turn's content, or "".
private func contentOf(_ turn: JSON) -> String {
    turn["content"].asString ?? ""
}

/// Replace the body of every reflect reply the model has ALREADY responded to.
/// Python `drop_answered_reflects`.
///
/// The IN-FLIGHT reflect (the last turn, not yet answered) is left intact — the
/// model is reading it right now. Everything older is a reflect it has already had
/// its chance to distil.
///
/// FAIL CLOSED. The body drops only if the model actually WROTE the distillation in
/// the turn that answered it. Measured 2026-08-20 (`live_plan`): the model replied
/// to several reflects with a bare tool_call and no prose, the body was dropped
/// anyway, and it had to re-fetch what it had lost — "The index was dropped from my
/// view. Let me send it again and, THIS TIME, actually record the facts." Burning
/// turns re-reading is the exact loop this trim exists to prevent, so a reflect the
/// model did not distil is KEPT. Non-compliance then costs context, never
/// comprehension.
func dropAnsweredReflects(_ history: [JSON]) -> [JSON] {
    var out: [JSON] = []
    for (i, turn) in history.enumerated() {
        let tagged = turn["role"].asString == "user" && parseReplyTag(contentOf(turn)) != nil
        let answer = history[(i + 1)...].first { $0["role"].asString == "assistant" }
        let answered = answer.map { distilled(contentOf($0)) } ?? false
        if !(tagged && answered) {
            out.append(turn)
            continue
        }
        // Per BLOCK: only the reflect bodies drop; sibling replies that share the
        // turn (a batch arrives whole) stay verbatim.
        let blocks = replyBlocks(contentOf(turn))
        let trimmed = blocks.map { b -> String in
            if let tag = parseReplyTag(b), tag.verb == "reflect" {
                return reflectDropped(agent: tag.agent)
            }
            return b
        }
        if trimmed == blocks {
            out.append(turn)
        } else {
            out.append(
                .object([
                    "role": .string("user"),
                    "content": .string(trimmed.joined(separator: "\n\n")),
                ]))
        }
    }
    return out
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
                .object([
                    "role": .string("user"),
                    "content": .string(keptRev.reversed().joined(separator: "\n\n")),
                ]))
        }
    }
    return keepRev.reversed()
}
