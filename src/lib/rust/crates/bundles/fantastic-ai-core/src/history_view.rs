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
//!   `reflect` · `list_agents` · `ps` · `keys` → whole-state. The newest reply IS
//!       the truth; older copies describe a state that no longer exists and can
//!       only mislead. Safe to drop.
//!   `read` · `list` · `status` · `get` → PARAMETERISED. Two `read`s on the same
//!       agent are different files; the tag carries no args, so deduping them
//!       would silently delete content. NEVER dropped.
//!
//! AUTOFOLD ([`autofold`], #556) then shows a thought, a cut answer and a large or
//! recalled reply as a short anchor, at assembly time and never stored.

use crate::constants::{AUTOFOLD_AT, CHARS_PER_TOKEN, KEEP_SUMMARY_MAX, REFERENCE_FOLD_AT};
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

/// A `docs` reply the model has ANSWERED is cut to this stub. The body itself
/// stays HELD this run under its pid: the model keeps the lines it needs with its
/// own `grep`, and those small replies stay in its view. Python `_DOCS_DROPPED`.
fn docs_dropped(of: &str, tier: &str, pid: &str, verb: &str) -> String {
    format!(
        "[docs of={of} tier={tier} — dropped from your view; held this run: send yourself \
         <type>grep</type> <ids><item>{pid}</item></ids> <query>{verb}</query>]"
    )
}

/// A REAL verb of this docs body for the grep example: the first that is not
/// `reflect`/`boot`, else the first. A placeholder example is copied verbatim —
/// `<query>verb</query>` sent as written matches only the `<verbs>` lines, and the
/// model concludes the held body is gone.
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
        "[this reply is shortened to one line after your next message; to get lines back \
         later, a grep with <ids><item>{pid}</item></ids> <query>{verb}</query> \
         <width>1000</width> returns them]\n"
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
    } else if reason == Some("missing_arg") && o.get("arg").is_some_and(Value::is_string) {
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
pub fn with_content(turn: &Value, content: String) -> Value {
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
                        crate::helpers::shown(first),
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

/// The body of the correction `content` carries, when it carries one: a block
/// tagged `- -` whose body opens with the malformed-call or unsupported-tag report.
/// A `call NOT dispatched:` body is no cut.
fn correction(content: &str) -> Option<&str> {
    reply_blocks(content).into_iter().find_map(|b| {
        if parse_reply_tag(b) != Some(("-", "-")) {
            return None;
        }
        let body = body_of(b);
        (body.starts_with("[malformed call — nothing ran]")
            || body.starts_with("[unsupported tag — not read]"))
        .then_some(body)
    })
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

/// What a cut row shows in place of its last line: the anchor, with what to do.
fn cut_content(content: &str, thought: Option<usize>, id: i64, correction: &str) -> String {
    let c = content.trim_end();
    let (prefix, last) = match c.rfind('\n') {
        Some(i) => (c[..i].trim_end(), c[i + 1..].trim()),
        None => ("", c.trim()),
    };
    let wrote = cap120(last);
    let thought = thought.map_or(String::new(), |x| format!(" after thinking ~{x} tokens"));
    let lines: Vec<&str> = correction.split('\n').collect();
    let action = match lines.iter().position(|l| *l == "write it as:") {
        Some(i) => {
            let mut form = String::new();
            for l in &lines[i + 1..] {
                form.push_str(l);
                if l.ends_with("</call>") {
                    break;
                }
            }
            format!("rewrite as {form}")
        }
        None => {
            let caret = lines.get(2).map_or("", |l| l.trim());
            let caret = caret.trim_start_matches(['^', '~']);
            caret.strip_prefix(' ').unwrap_or(caret).to_string()
        }
    };
    let anchor =
        format!("[cut · you wrote {wrote}{thought} · nothing ran · ACTION: {action} · full: {id}]");
    if prefix.is_empty() {
        anchor
    } else {
        format!("{prefix}\n{anchor}")
    }
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

/// The token count over which a block of this `verb` folds once answered:
/// `docs` replies fold sooner than any other body (#634); a `reflect` never folds.
fn fold_limit(verb: &str) -> usize {
    if matches!(verb, "reflect" | "docs") {
        REFERENCE_FOLD_AT
    } else {
        AUTOFOLD_AT
    }
}

/// Whether a user turn's content will fold once it is answered: it opens with a
/// reply tag and carries a recall reply or a body over its verb's fold limit
/// ([`AUTOFOLD_AT`] tokens; [`REFERENCE_FOLD_AT`] for `docs`; a `reflect` never folds).
#[must_use]
pub fn will_fold(content: &str) -> bool {
    parse_reply_tag(content).is_some() && reply_blocks(content).into_iter().any(|b| block_folds(b))
}

/// Whether one reply block folds once answered: a recall reply, or a body over its
/// verb's fold limit.
fn block_folds(block: &str) -> bool {
    let verb = parse_reply_tag(block).map_or("", |(_, v)| v);
    verb == "recall" || (verb != "reflect" && body_tokens(body_of(block)) > fold_limit(verb))
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
/// model sees it — an anchor in place of a thought, a cut answer's last line, an
/// answered large body or any answered recall reply. Pure and never stored: the
/// store keeps every row whole, and `recall` with the row's id reads it back.
/// `incoming` is the text of the turn being assembled, which is the correction a
/// last cut row is followed by. A row with no int `id` comes back unchanged. Also
/// which form the row took: `whole`, `cut`, `thought_anchor` (the anchor stands for
/// the whole answer), `thought` (the anchor sits on the answer), `recalled` (a recall
/// reply) or `anchor` (a large body).
#[must_use]
pub fn autofold(
    store: &[Value],
    idx: usize,
    msg: &Value,
    incoming: Option<&str>,
) -> (Value, &'static str) {
    let (Some(row), Some(content)) = (store.get(idx), msg.get("content").and_then(Value::as_str))
    else {
        return (msg.clone(), "whole");
    };
    let Some(id) = row.get("id").and_then(Value::as_i64) else {
        return (msg.clone(), "whole");
    };
    match row.get("role").and_then(Value::as_str) {
        Some("assistant") => {
            let next = match store.get(idx + 1) {
                Some(n) => Some(content_of(n)),
                None => incoming,
            };
            let thought = thinking_tokens(row);
            if let Some(body) = next.and_then(correction) {
                return (
                    with_content(msg, cut_content(content, thought, id, body)),
                    "cut",
                );
            }
            match thought {
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
            }
        }
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
                    } else if verb != "reflect" && x > fold_limit(verb) {
                        changed = true;
                        let head = cap120(body.split('\n').next().unwrap_or("").trim());
                        let summary = kept_summary(row, reply_pid(b));
                        if matches!(verb, "reflect" | "docs") {
                            // re-called, never recalled: no `get:` pointer
                            with_kept(
                                format!(
                                    "[folded · {verb} reply from {agent} · ~{x} tokens · {verb} again for the current state]"
                                ),
                                summary,
                            )
                        } else if summary.is_some() {
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
    /// sibling replies stay. Treating the turn as one reply would either delete
    /// siblings or trim nothing (a count guard skips every multi-reply turn).
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
    // ─── autofold (#556) ────────────────────────────────────────────────────

    fn row(id: i64, role: &str, content: &str) -> Value {
        json!({"role": role, "id": id, "content": content})
    }

    /// The view message of `store[idx]` after autofold.
    fn folded(store: &[Value], idx: usize, incoming: Option<&str>) -> String {
        let (out, _) = autofold(store, idx, &store[idx], incoming);
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

    /// A correction as the model is sent it.
    fn correction_for(answer: &str) -> String {
        format!(
            "[reply to - · - - · ↑9]\n{}",
            crate::call_check::check(answer, 0)[0]
        )
    }

    #[test]
    fn a_row_with_thinking_shows_its_thought_anchor() {
        let thinking = "t".repeat(400);
        let mut a = row(5, "assistant", "answer");
        a["thinking"] = json!(thinking);
        let store = [a];
        let shown = folded(&store, 0, None);
        assert_eq!(shown, "[thought ~100 tokens · id 5]\nanswer");
        assert!(!shown.contains("tttt"));
        // no thinking, no anchor
        let plain = [row(6, "assistant", "answer")];
        assert_eq!(folded(&plain, 0, None), "answer");
        // an anchor alone when the answer is empty
        let mut e = row(7, "assistant", "");
        e["thinking"] = json!("t".repeat(8));
        assert_eq!(folded(&[e], 0, None), "[thought ~2 tokens · id 7]");
    }

    #[test]
    fn the_thought_anchor_counts_the_cut_thinking_too() {
        let mut a = row(5, "assistant", "answer");
        a["thinking"] = json!("t".repeat(400));
        a["thinking_cut"] = json!(400);
        assert_eq!(
            folded(&[a], 0, None),
            "[thought ~200 tokens · id 5]\nanswer"
        );
    }

    #[test]
    fn a_reply_body_over_autofold_at_folds_once_answered() {
        let tag = "[reply to 3 · notes read]\n";
        let over = format!("{tag}{}", big_body(AUTOFOLD_AT * 4 + 4));
        let store = [row(8, "user", &over), row(9, "assistant", "ok")];
        assert_eq!(
            folded(&store, 0, None),
            "[folded · read reply from notes · ~16001 tokens · head: \"first line\" · get: 8]"
        );
        let at = format!("{tag}{}", big_body(AUTOFOLD_AT * 4));
        let store = [row(8, "user", &at), row(9, "assistant", "ok")];
        assert_eq!(folded(&store, 0, None), at);
        // unanswered: whole
        let store = [row(8, "user", &over)];
        assert_eq!(folded(&store, 0, None), over);
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
        let shown = folded(&store, 0, None);
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
            folded(&store, 1, None),
            "[folded · recall reply from ai · id 12 · ~1 tokens · get: 21]"
        );
        assert_eq!(
            folded(&store, 4, None),
            "[folded · recall reply from ai · query \"codename\" · ~1 tokens · get: 24]"
        );
        assert_eq!(
            folded(&store, 6, None),
            "[folded · recall reply from ai · ~1 tokens · get: 26]"
        );
        // unanswered: whole
        assert_eq!(
            folded(&store[..2], 1, None),
            "[reply to 7 · ai recall · ↑20]\ntiny"
        );
    }

    fn cut_store(thinking: Option<&str>, next: Option<&str>) -> Vec<Value> {
        let mut a = row(30, "assistant", "Sending.\n<invoke name=\"send\">");
        if let Some(t) = thinking {
            a["thinking"] = json!(t);
        }
        let mut v = vec![a];
        if let Some(n) = next {
            v.push(row(31, "user", n));
        }
        v
    }

    #[test]
    fn a_cut_row_folds_at_once_with_its_action() {
        let fix = correction_for("Sending.\n<invoke name=\"send\">");
        let think = "t".repeat(28);
        for (store, incoming) in [
            (cut_store(Some(&think), None), Some(fix.as_str())),
            (cut_store(Some(&think), Some(&fix)), None),
        ] {
            let shown = folded(&store, 0, incoming);
            assert!(
                shown.starts_with(
                    "Sending.\n[cut · you wrote <invoke name=\"send\"> after thinking ~7 tokens"
                ),
                "{shown}"
            );
            assert!(shown.contains(" · nothing ran · ACTION: "), "{shown}");
            assert!(shown.ends_with(" · full: 30]"), "{shown}");
        }
        // no thinking, no segment; a one-line answer has no prefix
        let mut one = cut_store(None, None);
        one[0]["content"] = json!("<invoke name=\"send\">");
        let shown = folded(&one, 0, Some(&fix));
        assert!(
            shown.starts_with("[cut · you wrote <invoke name=\"send\"> · nothing ran"),
            "{shown}"
        );
    }

    #[test]
    fn an_unsupported_tag_correction_gives_the_caret_instruction_as_action() {
        let fix = correction_for("Hello <b>bold</b> text");
        assert!(fix.contains("[unsupported tag — not read]"), "{fix}");
        let mut a = row(30, "assistant", "Hello <b>");
        a["thinking"] = json!("t".repeat(4));
        let shown = folded(&[a], 0, Some(&fix));
        assert!(
            shown.contains("ACTION: `<b>` is not part of the call grammar — delete it · full: 30]"),
            "{shown}"
        );
    }

    #[test]
    fn a_row_before_an_ordinary_reply_or_a_not_dispatched_report_is_not_a_cut() {
        let mut a = row(30, "assistant", "text\n<call name=\"done\"></call>");
        a["thinking"] = json!("t".repeat(4));
        let ordinary = "[reply to 5 · notes read]\nbody";
        let not_run = "[reply to - · - -]\ncall NOT dispatched: a send names its target";
        for next in [ordinary, not_run, "plain user text"] {
            let shown = folded(&[a.clone(), row(31, "user", next)], 0, None);
            assert_eq!(
                shown,
                "[thought ~1 tokens · id 30]\ntext\n<call name=\"done\"></call>"
            );
        }
        let shown = folded(&[a.clone()], 0, Some(not_run));
        assert!(shown.starts_with("[thought ~"), "{shown}");
    }

    #[test]
    fn docs_replies_fold_over_the_reference_limit_and_a_reflect_never_folds() {
        use crate::constants::REFERENCE_FOLD_AT;
        let over = "x".repeat(REFERENCE_FOLD_AT * 4 + 4);
        let at = "x".repeat(REFERENCE_FOLD_AT * 4);
        // a reflect stays whole at any size, answered or not
        let big = format!(
            "[reply to 3 \u{b7} ai reflect]\nhead line\n{}",
            "x".repeat(AUTOFOLD_AT * 8)
        );
        assert!(!will_fold(&big));
        let kept = [row(7, "user", &big), row(8, "assistant", "ok")];
        assert_eq!(autofold(&kept, 0, &kept[0], None).1, "whole");
        for verb in ["docs"] {
            let tag = format!("[reply to 3 \u{b7} ai {verb}]\n");
            assert!(will_fold(&format!("{tag}{over}")), "{verb}");
            assert!(!will_fold(&format!("{tag}{at}")), "{verb}");
            let store = [
                row(7, "user", &format!("{tag}head line\n{over}")),
                row(8, "assistant", "ok"),
            ];
            let (out, form) = autofold(&store, 0, &store[0], None);
            assert_eq!(form, "anchor");
            assert_eq!(
                out["content"].as_str().unwrap(),
                format!(
                    "[folded \u{b7} {verb} reply from ai \u{b7} ~{} tokens \u{b7} {verb} again for the current state]",
                    REFERENCE_FOLD_AT + 4
                )
            );
            // one under the limit stays inline; so does an unanswered one
            let small = [
                row(7, "user", &format!("{tag}{at}")),
                row(8, "assistant", "ok"),
            ];
            assert_eq!(autofold(&small, 0, &small[0], None).1, "whole");
            assert_eq!(autofold(&store[..1], 0, &store[0], None).1, "whole");
        }
        // any other verb keeps the large-body limit
        let other = format!(
            "[reply to 3 \u{b7} ai read]\n{}",
            "x".repeat(REFERENCE_FOLD_AT * 4 + 4)
        );
        assert!(!will_fold(&other));
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
        let (out, form) = autofold(&store, 0, &store[0], None);
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
            let (out, _) = autofold(&store, 0, &store[0], None);
            assert_eq!(out["content"], json!(plain), "{keep:?}");
        }
        let at = "y".repeat(KEEP_SUMMARY_MAX * 4);
        let store = kept_row(&big, Some(&at));
        let (out, _) = autofold(&store, 0, &store[0], None);
        assert!(out["content"].as_str().unwrap().ends_with(&at));
    }
}
