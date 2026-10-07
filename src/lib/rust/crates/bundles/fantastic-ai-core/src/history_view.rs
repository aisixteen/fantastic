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
    // the tag's own closer, never one carried in from the body. It was `\S+` until
    // 2026-09-04, which on `[reply to x · notes reflect]\n{"verbs":["read"]}` took
    // `reflect]` and let `[^\]]*\]` close on a bracket inside the BODY — the verb then
    // read `reflect]`, matched no vocabulary, and both trimmers silently did nothing for
    // every reply containing a bracket (which is every reflect: bodies are JSON).
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
/// unit they were written for; treating the whole turn as one reply either deleted
/// siblings (the reflect cut) or trimmed nothing (a count guard that
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

/// The turn's content, or `""`.
fn content_of(turn: &Value) -> &str {
    turn.get("content").and_then(Value::as_str).unwrap_or("")
}

/// A `docs` reply the model has ANSWERED is cut to this stub (owner,
/// 2026-10-05). The body itself stays HELD this run under its pid (owner,
/// 2026-10-06): the model keeps the lines it needs with its own `grep`, and those
/// small replies stay in its view. Python `_DOCS_DROPPED`.
fn docs_dropped(of: &str, tier: &str, pid: &str, verb: &str) -> String {
    format!(
        "[docs of={of} tier={tier} — dropped from your view; held this run: send yourself \
         <type>grep</type> <ids><item>{pid}</item></ids> <query>{verb}</query>]"
    )
}

/// A REAL verb of this docs body for the grep example: the first that is not
/// `reflect`/`boot`, else the first. A placeholder example is copied verbatim —
/// measured 2026-10-06 (canvas E55/E58): `<query>verb</query>` was sent as written,
/// matched only the `<verbs>` lines, and the model concluded the held body was gone.
/// `None` when the body lists no verbs (nothing to keep, so nothing is held).
/// Python `example_verb`.
#[must_use]
pub fn example_verb(body: &str) -> Option<String> {
    let mut lines = body.lines().skip_while(|l| *l != "<verbs>").skip(1);
    let mut names: Vec<String> = Vec::new();
    while let Some(line) = lines.next() {
        if line.starts_with("</verbs>") {
            break;
        }
        // A top-level entry `<name>…</name>` starts a line; its doc may run on
        // over more lines, up to its own closer.
        let Some(name) = line
            .strip_prefix('<')
            .and_then(|r| r.split_once('>'))
            .map(|(n, _)| n)
            .filter(|n| {
                n.starts_with(|c: char| c == '_' || c.is_ascii_alphabetic())
                    && n.chars().all(|c| c == '_' || c.is_ascii_alphanumeric())
            })
        else {
            continue;
        };
        let closer = format!("</{name}>");
        if !line[name.len() + 2..].contains(&closer) && !lines.any(|l| l.contains(&closer)) {
            break;
        }
        names.push(name.to_string());
    }
    names
        .iter()
        .find(|n| !matches!(n.as_str(), "reflect" | "boot"))
        .or_else(|| names.first())
        .cloned()
}

/// The FIRST line of a docs body: it is erased after the next message, and how to
/// keep what the model will need. Python `_DOCS_HEAD`.
#[must_use]
pub fn docs_head(pid: i64, verb: &str) -> String {
    format!(
        "[erased from your view after your next message — decide now which verbs you need \
         and keep them in this same turn: send yourself <type>grep</type> \
         <ids><item>{pid}</item></ids> <query>{verb}</query> <width>1000</width>]\n"
    )
}

/// The `<of>`/`<tier>` lines of a rendered docs body. Python `_DOCS_FIELD`
/// (`^<(of|tier)>([^<\n]*)</\1>$`, multiline).
fn docs_field(block: &str, key: &str) -> Option<String> {
    let open = format!("<{key}>");
    let close = format!("</{key}>");
    block.split('\n').find_map(|line| {
        let inner = line
            .strip_prefix(open.as_str())?
            .strip_suffix(close.as_str())?;
        (!inner.contains('<')).then(|| inner.to_string())
    })
}

/// Replace every `docs` reply body the model has answered with its stub. The
/// reply in the last turn (not answered yet) stays whole; a refusal (no `of` in
/// the body) stays whole. Python `drop_read_docs`.
pub fn drop_read_docs(history: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(history.len());
    for (i, turn) in history.iter().enumerate() {
        let content = content_of(turn);
        let answered = history[i + 1..]
            .iter()
            .any(|t| t.get("role").and_then(Value::as_str) == Some("assistant"));
        if turn.get("role").and_then(Value::as_str) != Some("user")
            || !answered
            || parse_reply_tag(content).is_none()
        {
            out.push(turn.clone());
            continue;
        }
        let blocks = reply_blocks(content);
        let trimmed: Vec<String> = blocks
            .iter()
            .map(|b| {
                let docs = matches!(parse_reply_tag(b), Some((_, "docs")));
                match (docs, docs_field(b, "of"), example_verb(b)) {
                    (true, Some(of), Some(verb)) => {
                        let head = b.split('\n').next().unwrap_or(b);
                        let tier = docs_field(b, "tier").unwrap_or_default();
                        let pid = head
                            .strip_prefix("[reply to ")
                            .and_then(|r| r.split(' ').next())
                            .unwrap_or("-");
                        format!("{head}\n{}", docs_dropped(&of, &tier, pid, &verb))
                    }
                    _ => (*b).to_string(),
                }
            })
            .collect();
        if trimmed
            .iter()
            .zip(blocks.iter())
            .all(|(t, b)| t.as_str() == *b)
        {
            out.push(turn.clone());
        } else {
            out.push(with_content(turn, trimmed.join("\n\n")));
        }
    }
    out
}

/// A refusal of the model's own call — a verb its target does not answer
/// (`unhandled`, naming that verb), or an argument it did not send (`missing_arg`
/// naming the `arg`) — names where the fix is, `docs`, when it carries no hint of
/// its own. Decided on the machine `reason`, never on prose. Python
/// `point_to_docs`.
#[must_use]
pub fn point_to_docs(target: &str, verb: &str, result: &Value) -> Option<Value> {
    let o = result.as_object()?;
    if o.contains_key("hint") {
        return None;
    }
    let reason = o.get("reason").and_then(Value::as_str);
    let hint = if reason == Some("unhandled") && o.get("verb").and_then(Value::as_str) == Some(verb)
    {
        format!("the verbs {target} answers: send kernel {{type: docs, of: '{target}'}}")
    } else if reason == Some("missing_arg") && o.get("arg").is_some_and(fantastic_bundle::py_truthy)
    {
        format!(
            "how to call it: send kernel {{type: docs, of: '{target}', verb: '{verb}', tier: detailed}}"
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
fn with_content(turn: &Value, content: String) -> Value {
    let mut row = match turn {
        Value::Object(m) => m.clone(),
        _ => serde_json::Map::new(),
    };
    row.insert("content".to_string(), json!(content));
    Value::Object(row)
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
            keep_rev.push(with_content(turn, kept_rev.join("\n\n")));
        }
    }
    keep_rev.reverse();
    keep_rev
}

/// A reply whose BODY is byte-identical to an EARLIER one (the same file read
/// again, the same listing) is shown as a one-line stub citing the first — the
/// first copy stays whole. Only bodies worth the saving are stubbed (400 chars
/// and up), and a digested reply (its origin line, `(settled …`) is already one
/// line. The view restarts at a clear, so a read after a clear is new again.
/// Python `dedupe_repeat_bodies`.
#[must_use]
pub fn dedupe_repeat_bodies(history: &[Value]) -> Vec<Value> {
    let mut seen: std::collections::HashMap<String, Value> = std::collections::HashMap::new();
    let mut out: Vec<Value> = Vec::with_capacity(history.len());
    for turn in history {
        let content = turn.get("content").and_then(Value::as_str).unwrap_or("");
        let is_user = turn.get("role").and_then(Value::as_str) == Some("user");
        if !is_user || parse_reply_tag(content).is_none() {
            out.push(turn.clone());
            continue;
        }
        let mut changed = false;
        let mut kept: Vec<String> = Vec::new();
        for b in reply_blocks(content) {
            let (head, body) = b.split_once('\n').unwrap_or((b, ""));
            if body.chars().count() < 400 || body.starts_with("(settled") {
                kept.push(b.to_string());
                continue;
            }
            match seen.get(body) {
                Some(first) => {
                    kept.push(format!(
                        "{head}\n[identical to the reply in row {} — the same {} bytes, not repeated]",
                        fantastic_bundle::py_str(first),
                        body.chars().count()
                    ));
                    changed = true;
                }
                None => {
                    seen.insert(
                        body.to_string(),
                        turn.get("id").cloned().unwrap_or(Value::Null),
                    );
                    kept.push(b.to_string());
                }
            }
        }
        if changed {
            out.push(with_content(turn, kept.join("\n\n")));
        } else {
            out.push(turn.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// python `{**turn, "content": …}`: a trimmed row keeps `id`, `parent`,
    /// `agent` and `ts`. Rebuilding a bare `{role, content}` made `next_span`
    /// skip the row, `save_view` drop it from the array, and the estimate lose
    /// its bytes (rust folds carried 7 refs where python carried 8).
    #[test]
    fn the_view_trimmers_keep_row_keys() {
        let row = |id: i64, content: String| {
            json!({"role": "user", "id": id, "parent": id - 1, "agent": "ai",
                   "ts": 1.0, "content": content})
        };
        let keys = |out: &[Value]| {
            for r in out {
                if r["role"] == json!("assistant") {
                    continue;
                }
                assert!(r.get("id").is_some(), "the row lost its id: {r:#?}");
                assert!(r.get("parent").is_some(), "the row lost its parent: {r:#?}");
                assert!(r.get("agent").is_some(), "the row lost its agent: {r:#?}");
                assert!(r.get("ts").is_some(), "the row lost its ts: {r:#?}");
            }
        };
        // `dedupe_stale_reads`: a SUPERSEDED whole-state block drops from a turn
        // whose sibling block stays.
        let batch = "[reply to 11 \u{b7} notes reflect]\nold state\n\n\
                     [reply to 12 \u{b7} notes read]\nfile body"
            .to_string();
        let stale = vec![
            row(11, batch.clone()),
            row(
                12,
                "[reply to 13 \u{b7} notes reflect]\nnew state".to_string(),
            ),
        ];
        let out = dedupe_stale_reads(&stale);
        assert_ne!(out[0]["content"], json!(batch), "the stale block must drop");
        keys(&out);
    }

    fn turn(role: &str, content: &str) -> Value {
        json!({"role": role, "content": content})
    }

    fn text(t: &Value) -> &str {
        t.get("content").and_then(Value::as_str).unwrap_or("")
    }

    /// MEASURED 2026-09-04, in all three runtimes at once. The verb group was `\S+`,
    /// which does not exclude `]`, so on a reply whose BODY carries a bracket the run
    /// kept `reflect]` and the regex closed on the body's bracket. The verb then read
    /// `reflect]`, matched no vocabulary, and BOTH trimmers silently did nothing —
    /// on every reflect, since reflect bodies are JSON and always contain one.
    ///
    /// It survived because every fixture here, in python and in swift, used a
    /// bracket-free body. The three runtimes agreed with each other and with nothing.
    #[test]
    fn the_verb_stops_at_the_tags_own_bracket_not_the_bodys() {
        let real = "[reply to notes:aa \u{b7} notes reflect]\nverbs=[\"read\"]  root=.";
        assert_eq!(parse_reply_tag(real), Some(("notes", "reflect")));
        let stamped =
            "[reply to notes:aa \u{b7} notes reflect \u{b7} 2 of 3 in batch ab12]\nv=[\"x\"]";
        assert_eq!(parse_reply_tag(stamped), Some(("notes", "reflect")));
        assert_eq!(parse_reply_tag("[reply to a:1 notes reflect]"), None);
    }

    #[test]
    fn a_stale_reflect_whose_body_has_a_bracket_still_dedupes() {
        let body = "verbs=[\"read\", \"write\"]  root=.";
        let hist = [
            turn("user", &format!("[reply to n:aa · notes reflect]\n{body}")),
            turn("assistant", "SEEN notes"),
            turn("user", &format!("[reply to n:bb · notes reflect]\n{body}")),
        ];
        let out = dedupe_stale_reads(&hist);
        let kept = out.iter().filter(|t| text(t).contains(body)).count();
        assert_eq!(kept, 1, "stale whole-state reflect not dropped: {out:?}");
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

    /// A batch arrives as ONE turn holding several `[reply to …]` blocks. The
    /// trimmer works per BLOCK: a superseded whole-state block dedupes alone, its
    /// sibling replies stay. Treating the turn as one reply either deleted siblings
    /// or trimmed nothing (a count guard skipped every multi-reply turn — tried
    /// 2026-08-25, superseded same day).
    #[test]
    fn a_combined_turn_trims_per_block_never_whole() {
        let combined = format!(
            "[reply to ks:a1 · kernel_state reflect · 1 of 2 in batch ab12]\n{}\
             \n\n[reply to tot:b1 · totals read · 2 of 2 in batch ab12]\nreading_1 = 41",
            "TREE".repeat(100)
        );
        let body = [
            turn("user", &combined),
            turn("assistant", "noted"),
            turn("user", "[reply to ks:a2 · kernel_state reflect]\nNEW TREE"),
        ];

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
}
