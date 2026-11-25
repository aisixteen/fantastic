//! The MODEL VIEW of the conversation — the trimmers and the autofold that shape
//! what the prompt carries, and nothing else. Ported from python
//! `ai_core/core.py:708-831`, which is canonical.
//!
//! THE STORE IS NEVER TOUCHED. Every function is pure: they take the durable
//! turns and return a VIEW. The durable JSONL sidecar keeps every turn verbatim,
//! so the trace stays complete and `recall` can page anything back.
//!
//! Why trim at all. Reflect replies — many of them repeats of a target already
//! reflected — can be a large share of the whole conversation. That bulk is what
//! fills the window; a full window is what forces compaction; compaction is what
//! puts the run's goal at risk. Trimming it attacks the cause, not the symptom.
//!
//! The rule is narrow on purpose. A reply may be dropped only when a LATER reply
//! makes it strictly redundant, which requires the verb to return WHOLE state
//! rather than an answer to a question:
//!
//!   `reflect` · `ps` · `keys` → whole-state. The newest reply IS
//!       the truth; older copies describe a state that no longer exists and can
//!       only mislead. Safe to drop.
//!   `read` · `list` · `status` · `get` → PARAMETERISED. Two `read`s on the same
//!       agent are different files; the tag carries no args, so deduping them
//!       would silently delete content. NEVER dropped.
//!
//! AUTOFOLD ([`autofold`], #556) then shows a thought, a cut answer and a large or
//! recalled reply as a short anchor, at assembly time and never stored.

use crate::constants::{AUTOFOLD_AT, CHARS_PER_TOKEN, KEEP_SUMMARY_MAX};
use serde_json::{json, Value};

/// The verbs whose reply is WHOLE STATE, so a later reply strictly supersedes an
/// earlier one. Python `_WHOLE_STATE_READS`.
pub const WHOLE_STATE_READS: [&str; 3] = ["reflect", "ps", "keys"];

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
    // The verb group is `[^\]\s]+`, so a `]` ENDS it: the first bracket in the run is
    // the tag's own closer, never one carried in from the body. With `\S+`, on
    // `[reply to x · notes reflect]\n{"verbs":["read"]}` the group takes
    // `reflect]` and `[^\]]*\]` closes on a bracket inside the BODY — the verb then
    // reads `reflect]`, matches no vocabulary, and both trimmers silently do nothing
    // for every reply containing a bracket (which is every reflect: bodies are JSON).
    if let Some(i) = verb_run.find(']') {
        if i == 0 {
            return None; // `[^\]\s]+` must match at least one char
        }
        return Some((agent, &verb_run[..i]));
    }
    // No bracket in the run: the tag can still close later (the ` · N of M in batch`
    // suffix), but there must BE a `]` ahead or the regex would not have matched.
    if !tail.contains(']') {
        return None;
    }
    Some((agent, verb_run))
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
/// unit they were written for; treating the whole turn as one reply would either
/// delete siblings (the reflect cut) or trim nothing (a count guard that skips
/// every multi-reply turn).
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

/// The turn's content, or `""`.
fn content_of(turn: &Value) -> &str {
    turn.get("content").and_then(Value::as_str).unwrap_or("")
}

/// A refusal of the model's own call — a verb its target does not answer
/// (`unhandled`, naming that verb), or an argument it did not send (`missing_arg`
/// naming the `arg`) — names where the fix is, the target's `reflect` with a `tier`,
/// when it carries no hint of its own. Decided on the machine `reason`, never on
/// prose. Python `point_to_docs`.
#[must_use]
pub fn point_to_docs(target: &str, verb: &str, result: &Value) -> Option<Value> {
    let o = result.as_object()?;
    if o.contains_key("hint") {
        return None;
    }
    let reason = o.get("reason").and_then(Value::as_str);
    let hint = if reason == Some("unhandled") && o.get("verb").and_then(Value::as_str) == Some(verb)
    {
        format!("the verbs {target} answers: send {target} {{type: reflect, tier: oneliners}}")
    } else if reason == Some("missing_arg") && o.get("arg").is_some_and(Value::is_string) {
        format!(
            "how to call it: send {target} {{type: reflect, verb: '{verb}', tier: detailed}}"
        )
    } else {
        return None;
    };
    let mut m = o.clone();
    m.insert("hint".to_string(), Value::String(hint));
    Some(Value::Object(m))
}

/// The row with a NEW `content` and every other key intact — python's
/// `{**turn, "content": …}`.
pub fn with_content(turn: &Value, content: String) -> Value {
    let mut row = match turn {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    row.insert("content".to_string(), json!(content));
    Value::Object(row)
}

/// The tokens of `n` characters of text: `ceil(n / CHARS_PER_TOKEN)`.
fn body_tokens(body: &str) -> usize {
    body.chars().count().div_ceil(CHARS_PER_TOKEN)
}

/// The text of a block after its first line (the tag line).
fn body_of(block: &str) -> &str {
    block.split_once('\n').map_or("", |(_, b)| b)
}

/// `s` capped at 120 characters.
fn cap120(s: &str) -> String {
    s.chars().take(120).collect()
}

/// The `thinking` size the row records, in tokens (`thinking` plus `thinking_cut`),
/// when it has thinking.
fn thinking_tokens(row: &Value) -> Option<usize> {
    let t = row.get("thinking").and_then(Value::as_str)?;
    if t.is_empty() {
        return None;
    }
    let cut = row
        .get("thinking_cut")
        .and_then(Value::as_u64)
        .map_or(0, |n| n as usize);
    Some((t.chars().count() + cut).div_ceil(CHARS_PER_TOKEN))
}

/// What a recall reply recalled, as the anchor says it (` · id N`, or
/// ` · query "q"`): read off the model's own recall call that the block answers —
/// the block's `↑N` names the row, and the `nth` recall call to `agent` in it is the
/// one. Empty when any step fails.
fn recalled_what(store: &[Value], tag_line: &str, agent: &str, nth: usize) -> String {
    let find = || -> Option<String> {
        let n = crate::agent_loop::incoming_parent(tag_line)?;
        let row = store.iter().find(|r| {
            r.get("id").and_then(Value::as_i64) == Some(n)
                && r.get("role").and_then(Value::as_str) == Some("assistant")
        })?;
        let calls = crate::tool_parse::terminated_calls(content_of(row));
        let call = calls
            .iter()
            .map(|(c, _)| c)
            .filter(|c| {
                c.name == "send"
                    && c.arguments.get("target_id").and_then(Value::as_str) == Some(agent)
                    && c.arguments
                        .get("payload")
                        .and_then(|p| p.get("type"))
                        .and_then(Value::as_str)
                        == Some("recall")
            })
            .nth(nth)?;
        let payload = call.arguments.get("payload")?;
        if let Some(id) = payload
            .get("id")
            .and_then(|v| crate::tool_parse::typed::integer("id", v).ok())
        {
            return Some(format!(" · id {id}"));
        }
        let q = payload.get("query").and_then(Value::as_str)?;
        (!q.is_empty()).then(|| format!(" · query \"{q}\""))
    };
    find().unwrap_or_default()
}

/// Whether a user turn's content will fold once it is answered: it opens with a
/// reply tag and carries a recall reply or a body over [`AUTOFOLD_AT`] tokens (a
/// `reflect` never folds).
#[must_use]
pub fn will_fold(content: &str) -> bool {
    parse_reply_tag(content).is_some() && reply_blocks(content).into_iter().any(block_folds)
}

/// Whether one reply block folds once answered: a recall reply, or a body over
/// [`AUTOFOLD_AT`] tokens that is not a `reflect`.
fn block_folds(block: &str) -> bool {
    let verb = parse_reply_tag(block).map_or("", |(_, v)| v);
    verb == "recall" || (verb != "reflect" && body_tokens(body_of(block)) > AUTOFOLD_AT)
}

/// The pid a reply block names (`[reply to <pid> · …]`), when it is a number.
#[must_use]
pub fn reply_pid(block: &str) -> Option<i64> {
    let (pid, _) = take_run(block.strip_prefix("[reply to ")?)?;
    pid.parse().ok()
}

/// KEEP (#641): `content`, the turn that just arrived, with the `keep` call template
/// — the block's pid filled in — closing each reply block that will fold. The call's
/// presence is the cue; the model's view of the turn only, never stored.
#[must_use]
pub fn mark_keep(content: &str) -> String {
    if parse_reply_tag(content).is_none() {
        return content.to_string();
    }
    reply_blocks(content)
        .into_iter()
        .map(|b| match reply_pid(b).filter(|_| block_folds(b)) {
            Some(pid) => {
                format!("{b}\n<call name=\"keep\"><of>{pid}</of><summary>\u{2026}</summary></call>")
            }
            None => b.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The summary the model kept for the block `pid` of a row (`attributes.keep`), when it
/// is a real one and within [`KEEP_SUMMARY_MAX`] tokens: the placeholder copied back, an
/// empty text or an over-long one is no summary, and the block folds to its plain anchor.
fn kept_summary(row: &Value, pid: Option<i64>) -> Option<&str> {
    let text = row
        .get("attributes")?
        .get("keep")?
        .get(pid?.to_string())?
        .as_str()?
        .trim();
    let real = !text
        .trim_matches(|c: char| c == '.' || c == '\u{2026}' || c.is_whitespace())
        .is_empty();
    (real && body_tokens(text) <= KEEP_SUMMARY_MAX).then_some(text)
}

/// The summaries `kept` (by block pid) onto the rows that carry those blocks, as
/// `attributes.keep` (pid to summary): what [`autofold`] reads.
pub fn attach_kept(rows: &mut [Value], kept: &std::collections::HashMap<i64, Value>) {
    for row in rows
        .iter_mut()
        .filter(|r| r.get("role").and_then(Value::as_str) == Some("user"))
    {
        let mine: serde_json::Map<String, Value> = reply_blocks(content_of(row))
            .into_iter()
            .filter_map(reply_pid)
            .filter_map(|pid| kept.get(&pid).map(|s| (pid.to_string(), s.clone())))
            .collect();
        if !mine.is_empty() {
            row["attributes"]["keep"] = Value::Object(mine);
        }
    }
}

/// `anchor`, followed by the model's kept summary when there is one.
fn with_kept(anchor: String, summary: Option<&str>) -> String {
    match summary {
        Some(s) => format!("{anchor}\n{s}"),
        None => anchor,
    }
}

/// AUTOFOLD (#556): the row `store[idx]`, whose current message is `msg`, as the
/// model sees it — an anchor in place of a thought, an answered large body or any
/// answered recall reply. Pure and never stored: the store keeps every row whole,
/// and `recall` with the row's id reads it back. A row with no int `id` comes back
/// unchanged. Also which form the row took: `whole`, `thought_anchor` (the anchor
/// stands for the whole answer), `thought` (the anchor sits on the answer),
/// `recalled` (a recall reply) or `anchor` (a large body).
#[must_use]
pub fn autofold(store: &[Value], idx: usize, msg: &Value) -> (Value, &'static str) {
    let (Some(row), Some(content)) = (store.get(idx), msg.get("content").and_then(Value::as_str))
    else {
        return (msg.clone(), "whole");
    };
    let Some(id) = row.get("id").and_then(Value::as_i64) else {
        return (msg.clone(), "whole");
    };
    match row.get("role").and_then(Value::as_str) {
        Some("assistant") => match thinking_tokens(row) {
            Some(x) => {
                let anchor = format!("[thought ~{x} tokens · id {id}]");
                let (shown, form) = if content.is_empty() {
                    (anchor, "thought_anchor")
                } else {
                    (format!("{anchor}\n{content}"), "thought")
                };
                (with_content(msg, shown), form)
            }
            None => (msg.clone(), "whole"),
        },
        Some("user") => {
            let answered = store[idx + 1..]
                .iter()
                .any(|t| t.get("role").and_then(Value::as_str) == Some("assistant"));
            if !answered || parse_reply_tag(content).is_none() {
                return (msg.clone(), "whole");
            }
            let mut changed = false;
            let mut recalled = false;
            let mut recalls = 0usize;
            let shown: Vec<String> = reply_blocks(content)
                .into_iter()
                .map(|b| {
                    let body = body_of(b);
                    let x = body_tokens(body);
                    let (agent, verb) = parse_reply_tag(b).unwrap_or(("", ""));
                    if verb == "recall" {
                        let tag_line = b.split('\n').next().unwrap_or(b);
                        let what = recalled_what(store, tag_line, agent, recalls);
                        recalls += 1;
                        changed = true;
                        recalled = true;
                        with_kept(
                            format!(
                                "[folded · recall reply from {agent}{what} · ~{x} tokens · get: {id}]"
                            ),
                            kept_summary(row, reply_pid(b)),
                        )
                    } else if verb != "reflect" && x > AUTOFOLD_AT {
                        changed = true;
                        let head = cap120(body.split('\n').next().unwrap_or("").trim());
                        let summary = kept_summary(row, reply_pid(b));
                        if summary.is_some() {
                            // the summary stands where the head line would
                            with_kept(
                                format!(
                                    "[folded · {verb} reply from {agent} · ~{x} tokens · get: {id}]"
                                ),
                                summary,
                            )
                        } else {
                            format!(
                                "[folded · {verb} reply from {agent} · ~{x} tokens · head: \"{head}\" · get: {id}]"
                            )
                        }
                    } else {
                        b.to_string()
                    }
                })
                .collect();
            if changed {
                (
                    with_content(msg, shown.join("\n\n")),
                    if recalled { "recalled" } else { "anchor" },
                )
            } else {
                (msg.clone(), "whole")
            }
        }
        _ => (msg.clone(), "whole"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The verb group must exclude `]`: with `\S+`, on a reply whose BODY carries
    /// a bracket the run keeps `reflect]` and the regex closes on the body's
    /// bracket. The verb then reads `reflect]`, matches no vocabulary, and BOTH
    /// trimmers silently do nothing — on every reflect, since reflect bodies are
    /// JSON and always contain one. Only a fixture with a bracketed body catches it.
    #[test]
    fn the_verb_stops_at_the_tags_own_bracket_not_the_bodys() {
        let real = "[reply to notes:aa \u{b7} notes reflect]\nverbs=[\"read\"]  root=.";
        assert_eq!(parse_reply_tag(real), Some(("notes", "reflect")));
        let stamped =
            "[reply to notes:aa \u{b7} notes reflect \u{b7} 2 of 3 in batch ab12]\nv=[\"x\"]";
        assert_eq!(parse_reply_tag(stamped), Some(("notes", "reflect")));
        assert_eq!(parse_reply_tag("[reply to a:1 notes reflect]"), None);
    }

    /// The tag matcher is python's regex, hand-scanned — including the `[^\]]*`
    /// that absorbs the batch suffix.
    #[test]
    fn the_tag_matcher_is_pythons() {
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
    }
    // ─── autofold (#556) ────────────────────────────────────────────────────

    fn row(id: i64, role: &str, content: &str) -> Value {
        json!({"role": role, "id": id, "content": content})
    }

    /// The view message of `store[idx]` after autofold.
    fn folded(store: &[Value], idx: usize) -> String {
        let (out, _) = autofold(store, idx, &store[idx]);
        out["content"].as_str().unwrap().to_string()
    }

    /// A send to `ai`, as the model writes it.
    fn send_call(payload: Value) -> String {
        let mut args = serde_json::Map::new();
        args.insert("target_id".into(), json!("ai"));
        args.insert("payload".into(), payload);
        crate::tool_parse::render_call("send", &args)
    }

    fn big_body(chars: usize) -> String {
        format!("first line\n{}", "x".repeat(chars - 11))
    }

    #[test]
    fn a_row_with_thinking_shows_its_thought_anchor() {
        let thinking = "t".repeat(400);
        let mut a = row(5, "assistant", "answer");
        a["thinking"] = json!(thinking);
        let store = [a];
        let shown = folded(&store, 0);
        assert_eq!(shown, "[thought ~100 tokens · id 5]\nanswer");
        assert!(!shown.contains("tttt"));
        // no thinking, no anchor
        let plain = [row(6, "assistant", "answer")];
        assert_eq!(folded(&plain, 0), "answer");
        // an anchor alone when the answer is empty
        let mut e = row(7, "assistant", "");
        e["thinking"] = json!("t".repeat(8));
        assert_eq!(folded(&[e], 0), "[thought ~2 tokens · id 7]");
    }

    #[test]
    fn the_thought_anchor_counts_the_cut_thinking_too() {
        let mut a = row(5, "assistant", "answer");
        a["thinking"] = json!("t".repeat(400));
        a["thinking_cut"] = json!(400);
        assert_eq!(folded(&[a], 0), "[thought ~200 tokens · id 5]\nanswer");
    }

    #[test]
    fn a_reply_body_over_autofold_at_folds_once_answered() {
        let tag = "[reply to 3 · notes read]\n";
        let over = format!("{tag}{}", big_body(AUTOFOLD_AT * 4 + 4));
        let store = [row(8, "user", &over), row(9, "assistant", "ok")];
        assert_eq!(
            folded(&store, 0),
            "[folded · read reply from notes · ~16001 tokens · head: \"first line\" · get: 8]"
        );
        let at = format!("{tag}{}", big_body(AUTOFOLD_AT * 4));
        let store = [row(8, "user", &at), row(9, "assistant", "ok")];
        assert_eq!(folded(&store, 0), at);
        // unanswered: whole
        let store = [row(8, "user", &over)];
        assert_eq!(folded(&store, 0), over);
    }

    #[test]
    fn a_batch_folds_only_its_large_block() {
        let small = "[reply to 3 · notes list · 1 of 2 in batch b]\nsmall body";
        let large = format!(
            "[reply to 3 · notes read · 2 of 2 in batch b]\n{}",
            big_body(AUTOFOLD_AT * 4 + 8)
        );
        let content = format!("{small}\n\n{large}");
        let store = [row(8, "user", &content), row(9, "assistant", "ok")];
        let shown = folded(&store, 0);
        let (first, second) = shown.split_once("\n\n").expect("two blocks");
        assert_eq!(first, small);
        assert!(
            second.starts_with("[folded · read reply from notes · ~"),
            "{shown}"
        );
        assert!(second.ends_with(" · get: 8]"), "{shown}");
    }

    #[test]
    fn an_answered_recall_reply_folds_whatever_its_size_and_names_what_it_recalled() {
        let by_id = send_call(json!({"type": "recall", "id": 12}));
        let by_query = send_call(json!({"type": "recall", "query": "codename"}));
        let store = [
            row(20, "assistant", &by_id),
            row(21, "user", "[reply to 7 · ai recall · ↑20]\ntiny"),
            row(22, "assistant", "ok"),
            row(23, "assistant", &by_query),
            row(24, "user", "[reply to 7 · ai recall · ↑23]\ntiny"),
            row(25, "assistant", "ok"),
            row(26, "user", "[reply to 7 · ai recall]\ntiny"),
            row(27, "assistant", "ok"),
        ];
        assert_eq!(
            folded(&store, 1),
            "[folded · recall reply from ai · id 12 · ~1 tokens · get: 21]"
        );
        assert_eq!(
            folded(&store, 4),
            "[folded · recall reply from ai · query \"codename\" · ~1 tokens · get: 24]"
        );
        assert_eq!(
            folded(&store, 6),
            "[folded · recall reply from ai · ~1 tokens · get: 26]"
        );
        // unanswered: whole
        assert_eq!(
            folded(&store[..2], 1),
            "[reply to 7 · ai recall · ↑20]\ntiny"
        );
    }

    /// H12: an old conversation with a stored correction block. The block shows
    /// whole as the reply block it is, and the row before it shows whole: its
    /// thought anchor only, no cut anchor.
    #[test]
    fn old_correction_is_a_plain_block() {
        let before = "Sending.\n<function=send>";
        let mut a = row(30, "assistant", before);
        a["thinking"] = json!("t".repeat(28));
        let malformed =
            "[reply to - · - - · ↑30]\n[malformed call — nothing ran] line 2, col 1:\n  \
                         <function=send>\n  ^~~~~~~~~~~~~~~ not a call tag\n";
        let unsupported =
            "[reply to - · - - · ↑30]\n[unsupported tag — not read] line 1, col 1:\n  \
                           <b>\n  ^~~ `<b>` is not part of the call grammar — delete it\n";
        let ordinary = "[reply to 5 · notes read]\nbody";
        let not_run = "[reply to - · - -]\ncall NOT dispatched: a send names its target";
        for next in [malformed, unsupported, ordinary, not_run, "plain user text"] {
            let store = [a.clone(), row(31, "user", next)];
            let (out, form) = autofold(&store, 0, &store[0]);
            assert_eq!(form, "thought", "{next}");
            assert_eq!(
                out["content"],
                json!(format!("[thought ~7 tokens · id 30]\n{before}")),
                "{next}"
            );
            // the block itself, unanswered and answered, is the stored text whole
            assert_eq!(autofold(&store, 1, &store[1]), (store[1].clone(), "whole"));
            let answered = [a.clone(), row(31, "user", next), row(32, "assistant", "ok")];
            assert_eq!(
                autofold(&answered, 1, &answered[1]),
                (answered[1].clone(), "whole"),
                "{next}"
            );
        }
        // the last row of the store, with no row after it, shows whole too
        let store = [a.clone()];
        assert_eq!(autofold(&store, 0, &store[0]).1, "thought");
    }

    #[test]
    fn a_reflect_reply_never_folds_at_any_size() {
        let big = format!(
            "[reply to 3 \u{b7} ai reflect]\nhead line\n{}",
            "x".repeat(AUTOFOLD_AT * 8)
        );
        assert!(!will_fold(&big));
        let kept = [row(7, "user", &big), row(8, "assistant", "ok")];
        assert_eq!(autofold(&kept, 0, &kept[0]).1, "whole");
    }

    #[test]
    fn will_fold_is_true_for_a_recall_or_a_large_block_only() {
        assert!(will_fold("[reply to 3 · ai recall]\nx"));
        assert!(will_fold(&format!(
            "[reply to 3 · notes read]\n{}",
            "x".repeat(AUTOFOLD_AT * 4 + 4)
        )));
        assert!(!will_fold(&format!(
            "[reply to 3 · notes read]\n{}",
            "x".repeat(AUTOFOLD_AT * 4)
        )));
        assert!(!will_fold("[reply to 3 · notes read]\nsmall"));
        assert!(!will_fold(&"x".repeat(AUTOFOLD_AT * 8)));
    }

    fn kept_row(content: &str, keep: Option<&str>) -> Vec<Value> {
        let mut user = json!({"role": "user", "id": 5, "content": content});
        if let Some(k) = keep {
            user["attributes"] = json!({"keep": {"3": k}});
        }
        vec![user, json!({"role": "assistant", "id": 6, "content": "ok"})]
    }

    #[test]
    fn keep_marks_the_blocks_that_will_fold_only() {
        let big = format!(
            "[reply to 3 · ai read]\nhead\n{}",
            "x".repeat(AUTOFOLD_AT * 4 + 4)
        );
        let small = "[reply to 4 · ai read]\nsmall";
        let marked = mark_keep(&format!("{small}\n\n{big}"));
        assert_eq!(marked.matches("<call name=\"keep\">").count(), 1);
        assert!(marked.starts_with(small), "{marked}");
        assert!(
            marked.ends_with("<call name=\"keep\"><of>3</of><summary>\u{2026}</summary></call>")
        );
        assert_eq!(mark_keep(small), small);
        assert_eq!(mark_keep("plain text"), "plain text");
    }

    #[test]
    fn a_kept_summary_stands_for_the_body_when_it_folds() {
        let big = format!(
            "[reply to 3 · ai read]\nhead\n{}",
            "x".repeat(AUTOFOLD_AT * 4 + 4)
        );
        let store = kept_row(&big, Some("the part I need"));
        let (out, form) = autofold(&store, 0, &store[0]);
        assert_eq!(form, "anchor");
        assert_eq!(
            out["content"],
            json!("[folded · read reply from ai · ~16003 tokens · get: 5]\nthe part I need")
        );
    }

    #[test]
    fn no_summary_an_over_cap_one_or_the_placeholder_fold_to_the_plain_anchor() {
        let big = format!(
            "[reply to 3 · ai read]\nhead\n{}",
            "x".repeat(AUTOFOLD_AT * 4 + 4)
        );
        let plain = "[folded · read reply from ai · ~16003 tokens · head: \"head\" · get: 5]";
        let long = "y".repeat(KEEP_SUMMARY_MAX * 4 + 4);
        for keep in [
            None,
            Some(long.as_str()),
            Some("\u{2026}"),
            Some(" ... "),
            Some(""),
        ] {
            let store = kept_row(&big, keep);
            let (out, _) = autofold(&store, 0, &store[0]);
            assert_eq!(out["content"], json!(plain), "{keep:?}");
        }
        let at = "y".repeat(KEEP_SUMMARY_MAX * 4);
        let store = kept_row(&big, Some(&at));
        let (out, _) = autofold(&store, 0, &store[0]);
        assert!(out["content"].as_str().unwrap().ends_with(&at));
    }
}
