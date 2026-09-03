//! The MODEL VIEW of the conversation — the two trimmers that shape what the
//! prompt carries, and nothing else. Ported from python `ai_core/core.py:708-831`,
//! which is canonical.
//!
//! THE STORE IS NEVER TOUCHED. Both functions are pure: they take the durable
//! turns and return a VIEW. The durable JSONL sidecar keeps every turn verbatim,
//! so the trace stays complete and `recall` can page anything back.
//!
//! Why trim at all. Measured 2026-08-20 (`live_plan`): reflect replies were 42% of
//! the whole conversation — 73k of 174k chars — and 9 of 16 were a repeat of a
//! target already reflected. That bulk is what fills the window; a full window is
//! what forces compaction; compaction is what put the run's goal at risk.
//! Trimming it attacks the cause, not the symptom.
//!
//! The rule is narrow on purpose. A reply may be dropped only when a LATER reply
//! makes it strictly redundant, which requires the verb to return WHOLE state
//! rather than an answer to a question:
//!
//!   `reflect` · `list_agents` · `ps` · `keys` → whole-state. The newest reply IS
//!       the truth; older copies describe a state that no longer exists and can
//!       only mislead. Safe to drop.
//!   `read` · `list` · `status` · `get` → PARAMETERISED. Two `read`s on the same
//!       agent are different files; the tag carries no args, so deduping them
//!       would silently delete content. NEVER dropped.

use serde_json::{json, Value};

/// The verbs whose reply is WHOLE STATE, so a later reply strictly supersedes an
/// earlier one. Python `_WHOLE_STATE_READS`.
pub const WHOLE_STATE_READS: [&str; 4] = ["reflect", "list_agents", "ps", "keys"];

/// A reflect the model has ALREADY ANSWERED is replaced by this stub, `{agent}`
/// filled in. The model was told to carry the facts it needs into that answer (by
/// [`REFLECT_NUDGE`], which rides on the reflect reply itself), so the
/// distillation IT wrote is what survives — written by the reader, at read time,
/// with the task in mind, which no post-hoc summarizer can match. The stub NAMES
/// the agent so "I have already looked at this" stays true, and re-reflecting is
/// always available if the distillation turns out to be thin.
pub fn reflect_dropped(agent: &str) -> String {
    format!("[reflect of {agent} — full text dropped; your distillation follows]")
}

/// The distillation instruction, appended to every delivered `reflect` reply body
/// by [`crate::route`]. Python `_REFLECT_NUDGE`, verbatim.
///
/// It rides ON the reply, not in the system block. Measured 2026-08-20
/// (`live_plan`, the passing run): with the instruction stated ONCE as
/// system-block policy, the model wrote a `SEEN` line for 0 of 10 reflects — and
/// re-reflected the SAME agent (`kernel_state`) 9 times in a 17-turn run. Policy
/// sits thousands of tokens from the data and applies to a turn that has not
/// happened yet; this arrives attached to the body it talks about, every time, at
/// the moment it applies. It is deliberately the LAST thing in the message: it is
/// what the model reads immediately before writing its answer.
pub const REFLECT_NUDGE: &str =
    "\n[this body is TRANSIENT — after your next message it is replaced by a stub. \
Open that message with the facts you will need, one compact line per agent:\n\
SEEN <id> — <what it is for>; verbs: <the ones you intend to use>. \
One reflect per agent is complete — reflecting it again yields nothing new; \
the next useful call is an action.]";

/// Python's `\S` — a char that is not whitespace.
fn is_non_space(c: char) -> bool {
    !c.is_whitespace()
}

/// The leading `\S+` run of `s` and the rest, or `None` when the run is empty.
fn take_run(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !is_non_space(c)).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    Some((&s[..end], &s[end..]))
}

/// Read a `[reply to <pid> · <agent> <verb>…]` tag off the FRONT of `s` and
/// answer `(agent, verb)`. A hand-scan of python's
/// `^\[reply to \S+ · (\S+) (\S+)[^\]]*\]` — including the trailing `[^\]]*`,
/// which is what absorbs the optional ` · N of M in batch <token>` suffix.
///
/// No regex dependency: this crate has none, and one matcher used by two trimmers
/// does not earn one.
pub fn parse_reply_tag(s: &str) -> Option<(&str, &str)> {
    let rest = s.strip_prefix("[reply to ")?;
    let (_pid, rest) = take_run(rest)?;
    let rest = rest.strip_prefix(" · ")?;
    let (agent, rest) = take_run(rest)?;
    let rest = rest.strip_prefix(' ')?;
    let (verb_run, tail) = take_run(rest)?;
    // `[^\]]*\]` matches whatever follows as long as a `]` exists ahead, so the
    // greedy `\S+` keeps the whole run whenever the tail carries the bracket.
    if tail.contains(']') {
        return Some((agent, verb_run));
    }
    // Otherwise the regex backtracks INTO the run, to the last `]` inside it.
    let i = verb_run.rfind(']')?;
    if i == 0 {
        return None; // `\S+` must match at least one char
    }
    Some((agent, &verb_run[..i]))
}

/// True when `s` OPENS with a reply tag's head (`[reply to \S+ · `) — the
/// lookahead half of python's `_BLOCK_SPLIT`.
fn starts_a_block(s: &str) -> bool {
    let Some(rest) = s.strip_prefix("[reply to ") else {
        return false;
    };
    take_run(rest).is_some_and(|(_, r)| r.starts_with(" · "))
}

/// Split a delivered turn into its reply BLOCKS. Python `_reply_blocks` /
/// `_BLOCK_SPLIT` (`\n\n(?=\[reply to \S+ · )`).
///
/// A delivered turn carries ONE OR MORE blocks — a batch arrives whole, one block
/// per call, joined by a blank line. The trimmers below work on the BLOCK, the
/// unit they were written for; treating the whole turn as one reply either deleted
/// siblings (`drop_answered_reflects`) or trimmed nothing (a count guard that
/// skipped every multi-reply turn — tried 2026-08-25, superseded same day).
pub fn reply_blocks(content: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while let Some(rel) = content[i..].find("\n\n") {
        let at = i + rel;
        if starts_a_block(&content[at + 2..]) {
            out.push(&content[start..at]);
            start = at + 2;
            i = start;
        } else {
            // The engine advances ONE char and retries — `\n\n\n[reply to …`
            // splits at the SECOND newline pair, not the first.
            i = at + 1;
        }
    }
    out.push(&content[start..]);
    out
}

/// What counts as "the model recorded it": the `SEEN <agent> — …` line the reflect
/// nudge asks for. Python `_DISTILLED` (`\bSEEN\b`, case-insensitive) — matched
/// leniently, because the point is whether facts were carried forward, not whether
/// a format was obeyed.
///
/// FAILS CLOSED: no match means the reflect body is KEPT.
pub fn distilled(text: &str) -> bool {
    let b = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(rel) = lower[from..].find("seen") {
        let at = from + rel;
        let before_ok = at == 0 || !is_word_byte(b[at - 1]);
        let after = at + 4;
        let after_ok = after >= b.len() || !is_word_byte(b[after]);
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Python's `\w` for the `\b` boundary either side of `SEEN`.
fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The turn's content, or `""`.
fn content_of(turn: &Value) -> &str {
    turn.get("content").and_then(Value::as_str).unwrap_or("")
}

/// Replace the body of every reflect reply the model has ALREADY responded to.
/// Python `drop_answered_reflects`.
///
/// The IN-FLIGHT reflect (the last turn, not yet answered) is left intact — the
/// model is reading it right now. Everything older is a reflect it has already had
/// its chance to distil.
///
/// FAIL CLOSED. The body drops only if the model actually WROTE the distillation
/// in the turn that answered it. Measured 2026-08-20 (`live_plan`): the model
/// replied to several reflects with a bare tool_call and no prose, the body was
/// dropped anyway, and it had to re-fetch what it had lost — "The index was dropped
/// from my view. Let me send it again and, THIS TIME, actually record the facts."
/// Burning turns re-reading is the exact loop this trim exists to prevent, so a
/// reflect the model did not distil is KEPT. Non-compliance then costs context,
/// never comprehension.
pub fn drop_answered_reflects(history: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(history.len());
    for (i, turn) in history.iter().enumerate() {
        let tagged = turn.get("role").and_then(Value::as_str) == Some("user")
            && parse_reply_tag(content_of(turn)).is_some();
        let answer = history[i + 1..]
            .iter()
            .find(|t| t.get("role").and_then(Value::as_str) == Some("assistant"));
        let answered = answer.is_some_and(|a| distilled(content_of(a)));
        if !(tagged && answered) {
            out.push(turn.clone());
            continue;
        }
        // Per BLOCK: only the reflect bodies drop; sibling replies that share the
        // turn (a batch arrives whole) stay verbatim.
        let content = content_of(turn);
        let blocks = reply_blocks(content);
        let trimmed: Vec<String> = blocks
            .iter()
            .map(|b| match parse_reply_tag(b) {
                Some((agent, "reflect")) => reflect_dropped(agent),
                _ => (*b).to_string(),
            })
            .collect();
        if trimmed
            .iter()
            .zip(blocks.iter())
            .all(|(t, b)| t.as_str() == *b)
        {
            out.push(turn.clone());
        } else {
            out.push(json!({"role": "user", "content": trimmed.join("\n\n")}));
        }
    }
    out
}

/// Drop whole-state read replies that a later reply to the same `(agent, verb)`
/// supersedes. Newest wins; order is otherwise preserved. Python
/// `dedupe_stale_reads`.
///
/// This is dedup, NOT stripping: the newest reply stays verbatim, so the model
/// keeps the facts (verbs, ids, tree) and has no reason to re-reflect — re-fetching
/// what it lost is exactly the loop we are trying to avoid.
pub fn dedupe_stale_reads(history: &[Value]) -> Vec<Value> {
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let mut keep_rev: Vec<Value> = Vec::with_capacity(history.len());
    for turn in history.iter().rev() {
        let content = content_of(turn);
        if turn.get("role").and_then(Value::as_str) != Some("user")
            || parse_reply_tag(content).is_none()
        {
            keep_rev.push(turn.clone());
            continue;
        }
        // Per BLOCK — a turn may carry a whole batch. A superseded whole-state
        // read drops alone; its siblings stay. A turn whose every block is
        // superseded drops entirely.
        let mut kept_rev: Vec<&str> = Vec::new();
        let mut changed = false;
        for b in reply_blocks(content).into_iter().rev() {
            if let Some((agent, verb)) = parse_reply_tag(b) {
                if WHOLE_STATE_READS.contains(&verb) {
                    let key = (agent.to_string(), verb.to_string());
                    if seen.contains(&key) {
                        changed = true;
                        continue; // a later reply already carries this state
                    }
                    seen.insert(key);
                }
            }
            kept_rev.push(b);
        }
        if kept_rev.is_empty() {
            continue;
        }
        if !changed {
            keep_rev.push(turn.clone());
        } else {
            kept_rev.reverse();
            keep_rev.push(json!({"role": "user", "content": kept_rev.join("\n\n")}));
        }
    }
    keep_rev.reverse();
    keep_rev
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    fn text(t: &Value) -> &str {
        t.get("content").and_then(Value::as_str).unwrap_or("")
    }

    #[test]
    fn stale_whole_state_reads_dedupe_but_parameterised_reads_never_do() {
        let body = [
            turn("user", "[reply to ks:a1 · kernel_state reflect]\nOLD TREE"),
            turn("assistant", "noted"),
            turn("user", "[reply to fb:b1 · readings read]\nreading_1 = 41"),
            turn("user", "[reply to fb:b2 · readings read]\nreading_2 = 42"),
            turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
        ];
        let out = dedupe_stale_reads(&body);
        let all = out.iter().map(text).collect::<Vec<_>>().join("\n");
        // whole-state: the newest reflect supersedes the older one
        assert!(!all.contains("OLD TREE"), "{all}");
        assert!(all.contains("NEW TREE"), "{all}");
        // parameterised: two `read`s are DIFFERENT FILES — dropping either loses
        // data, and the reply tag carries no args to tell them apart.
        assert!(all.contains("reading_1 = 41"), "{all}");
        assert!(all.contains("reading_2 = 42"), "{all}");
    }

    #[test]
    fn answered_reflects_are_stubbed_but_the_in_flight_one_survives() {
        let body = [
            turn(
                "user",
                &format!(
                    "[reply to notes:aaa · notes reflect]\n{}",
                    "TREE".repeat(200)
                ),
            ),
            turn(
                "assistant",
                "SEEN notes — key/value store; verbs read/set/keys.",
            ),
            turn(
                "user",
                &format!(
                    "[reply to assets:bbb · assets reflect]\n{}",
                    "TREE".repeat(200)
                ),
            ),
        ];
        let out = drop_answered_reflects(&body);
        // answered → stubbed, and the stub still NAMES the agent, so "I already
        // looked at this" stays true and the model does not re-reflect out of doubt
        assert!(!text(&out[0]).contains("TREE") && text(&out[0]).contains("notes"));
        // the model's own distillation is untouched — it is the record now
        assert_eq!(out[1], body[1]);
        // the IN-FLIGHT reflect is verbatim: the model is reading it this very turn
        assert_eq!(out[2], body[2]);
    }

    /// FAIL CLOSED. Measured 2026-08-20 (`live_plan`): the model answered several
    /// reflects with a bare tool_call and no prose. The body was dropped anyway, so
    /// the facts were simply gone — "The index was dropped from my view. Let me send
    /// it again and, THIS TIME, actually record the facts." Re-fetching what it lost
    /// is the exact loop this trim exists to prevent, so a reflect the model did not
    /// distil must survive: non-compliance costs context, never comprehension.
    #[test]
    fn a_reflect_the_model_did_not_distil_is_kept() {
        let body = [
            turn(
                "user",
                &format!(
                    "[reply to notes:aaa · notes reflect]\n{}",
                    "TREE".repeat(100)
                ),
            ),
            turn("assistant", r#"<tool_call>{"name":"send",...}</tool_call>"#),
            turn(
                "user",
                &format!(
                    "[reply to assets:bbb · assets reflect]\n{}",
                    "FACTS".repeat(100)
                ),
            ),
            turn(
                "assistant",
                "SEEN assets — public web assets; verbs read/list.",
            ),
            turn("user", "next"),
        ];
        let out = drop_answered_reflects(&body);
        assert!(
            text(&out[0]).contains("TREE"),
            "undistilled reflect must be KEPT"
        );
        assert!(
            !text(&out[2]).contains("FACTS"),
            "distilled reflect is dropped"
        );
        assert!(
            text(&out[2]).contains("assets"),
            "the stub still names the agent"
        );
    }

    /// A batch arrives as ONE turn holding several `[reply to …]` blocks. Both
    /// trimmers work per BLOCK: a distilled reflect block drops alone, its sibling
    /// replies stay; a superseded whole-state block dedupes alone. Treating the turn
    /// as one reply either deleted siblings (`drop_answered_reflects` replaced the
    /// WHOLE content) or trimmed nothing (a count guard skipped every multi-reply
    /// turn — tried 2026-08-25, superseded same day).
    #[test]
    fn a_combined_turn_trims_per_block_never_whole() {
        let combined = format!(
            "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch ab12]\n{}\
             \n\n[reply to tot:b1 · totals read · 2 of 2 in batch ab12]\nreading_1 = 41",
            "TREE".repeat(100)
        );
        let body = [
            turn("user", &combined),
            turn(
                "assistant",
                "SEEN kernel_state — the root; verbs create/list.",
            ),
            turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
        ];
        let out = drop_answered_reflects(&body);
        assert!(
            !text(&out[0]).contains(&"TREE".repeat(5)),
            "distilled reflect block drops"
        );
        assert!(
            text(&out[0]).contains("reading_1 = 41"),
            "sibling block must SURVIVE"
        );

        let out2 = dedupe_stale_reads(&body);
        let first = text(&out2[0]);
        assert!(
            !first.contains(&"TREE".repeat(5)),
            "older whole-state block is superseded"
        );
        assert!(
            first.contains("reading_1 = 41"),
            "parameterised sibling must survive"
        );
        assert!(out2.iter().any(|m| text(m).contains("NEW TREE")));
    }

    #[test]
    fn a_turn_whose_every_block_is_superseded_drops_entirely() {
        let body = [
            turn(
                "user",
                "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch cd34]\nOLD\
                 \n\n[reply to k:a2 · kernel ps · 2 of 2 in batch cd34]\nOLD ROWS",
            ),
            turn("assistant", "noted"),
            turn("user", "[reply to ks:a3 · kernel_state reflect]\nNEW"),
            turn("user", "[reply to k:a4 · kernel ps]\nNEW ROWS"),
        ];
        let out = dedupe_stale_reads(&body);
        assert!(!out.iter().any(|m| text(m).contains("OLD")));
        assert_eq!(out.len(), 3, "the emptied turn leaves no husk behind");
    }

    /// The tag matcher is python's regex, hand-scanned — including the `[^\]]*`
    /// that absorbs the batch suffix, and the fail-closed `SEEN` boundary.
    #[test]
    fn the_tag_matcher_and_the_seen_boundary_are_pythons() {
        assert_eq!(
            parse_reply_tag("[reply to a:1 · notes reflect]\nbody"),
            Some(("notes", "reflect"))
        );
        assert_eq!(
            parse_reply_tag("[reply to a:1 · notes reflect · 2 of 3 in batch ab12]\nbody"),
            Some(("notes", "reflect"))
        );
        assert_eq!(parse_reply_tag("no tag here"), None);
        assert_eq!(parse_reply_tag("[reply to a:1 notes reflect]"), None);
        // `\bSEEN\b`, any case, anywhere — and NOT inside a longer word.
        assert!(distilled("seen notes — a store"));
        assert!(distilled("I have SEEN it."));
        assert!(!distilled("unforeseen consequences"));
        assert!(!distilled("SEENING"));
        assert!(!distilled("a bare tool call and no prose"));
    }
}
