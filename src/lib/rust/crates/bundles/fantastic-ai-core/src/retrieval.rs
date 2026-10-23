//! Retrieval: FIND the line anywhere in memory (`grep`), start a new thread
//! (`context_clear`), and rebuild the agent's VIEW from its store
//! (`context_recall`).
//!
//! A reply too large to inline is held IN RAM under its pid for this run only and
//! the model is handed a digest + its `origin` line; grep reaches the body while
//! it is held, and the store (every row ever written, behind every clear, inside
//! every fold) plus every granted scope always. There is no `pull`: after a
//! restart the model re-reads from the origin line.
//!
//! Mirrors python `ai_core/core.py` `_grep` / `_context_clear` / `_context_recall`.

use crate::assembly::system_content;
use crate::assembly::{listed_entries, render_history};
use crate::constants::{BUDGET_FLOOR, NOTICE_ENVELOPE_RESERVE};
use crate::context::{
    budget as agent_budget, estimate_one, fit_reply, inline_cap, recent_n as cfg_recent_n,
};
use crate::events::to_caller;
use crate::folds::member_anchor;
use crate::folds::{expand_ids, quoted_title, render as fold_render, row_content, row_id};
use crate::helpers::{agent_meta, now_secs};
use crate::history_view::{parse_reply_tag, reply_blocks};
use crate::memory::{head, load_history, save_history, save_view, store_call, MemResult};
use crate::provider::{Provider, ProviderEvent};
use crate::strategies::fold::{append_fold, fold_down, summarize_span, summary_title};
use crate::strategies::strip::{pid_of, LOW};
use crate::strategies::ProjectionCtx;
use crate::think::without_thinking;
use crate::view::{apply_res, res_of};
use fantastic_bundle::{
    py_int, py_int_or, py_split_whitespace, py_splitlines, py_str, py_strip, py_truthy,
};
use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// A message id as an int — an int, or its decimal string (a model quotes what
/// it copied out of a tag). Anything else is not an id. Python `_as_id`.
///
/// THE TWO BRANCHES DIFFER ON PURPOSE: an int must be POSITIVE, a digit string
/// need not be (`'0'` → 0, which a lookup then answers as a readable not_found).
/// The string is python-stripped and its digits are ASCII only (python's
/// `isascii() and isdigit()`): `'٣'` and `'²'` are not ids, and `'+5'` is not a
/// digit string either.
#[must_use]
pub fn as_id(x: &Value) -> Option<i64> {
    match x {
        Value::Bool(_) => None,
        // python: a float is not an `int`.
        Value::Number(n) if n.is_f64() => None,
        Value::Number(n) => n.as_i64().filter(|&i| i > 0),
        Value::String(s) => {
            let t = py_strip(s);
            if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
                py_int(&Value::String(t.to_string())).ok()
            } else {
                None
            }
        }
        _ => None,
    }
}

fn bad_arg(msg: &str, arg: &str) -> Value {
    json!({"error": msg, "reason": "bad_arg", "arg": arg})
}

/// `grep`: find the LINE that mentions something ANYWHERE in memory — every row
/// this agent ever wrote or received (behind every clear, inside every fold), and
/// the reply bodies still held in memory this run. Python `_grep`.
///
/// args: `query` (req — case-insensitive substring), `ids?` (row ids / pids / fold
/// ids to search; a fold id searches its members), `limit?`, `width?` (chars of
/// each matched line, from its start; default 200). Returns `{matches:[{id, line,
/// text}], total, truncated}`.
///
/// `model_window` is the backend's own window
/// ([`crate::agent_loop::BackendConfig::model_window`]) — the inline cap a page
/// is measured against.
pub async fn grep(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    model_window: i64,
) -> MemResult<Value> {
    let null = Value::Null;
    // python `str(payload.get("query") or "").strip().lower()` — a NUMERIC query
    // is its digits, not "no query".
    let raw_q = payload.get("query").unwrap_or(&null);
    let q = py_strip(&if py_truthy(raw_q) {
        py_str(raw_q)
    } else {
        String::new()
    })
    .to_lowercase();
    if q.is_empty() {
        return Ok(bad_arg("grep: `query` is required", "query"));
    }
    // python `int(payload.get(k) or dflt)`, each knob under its OWN try.
    let num = |k: &str, dflt: i64| py_int_or(payload.get(k).unwrap_or(&null), dflt);
    let limit = num("limit", 100).map_or(100, |l| l.clamp(1, 100)) as usize;
    let width = num("width", 200).map_or(200, |w| w.clamp(20, 20_000)) as usize;

    let ids: Option<Vec<i64>> = match payload.get("ids") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => {
            let parsed: Vec<Option<i64>> = a.iter().map(as_id).collect();
            if parsed.iter().any(Option::is_none) {
                return Ok(bad_arg(
                    "grep: `ids` must be a list of message ids / pids (ints)",
                    "ids",
                ));
            }
            Some(parsed.into_iter().flatten().collect())
        }
        Some(_) => {
            return Ok(bad_arg(
                "grep: `ids` must be a list of message ids / pids (ints)",
                "ids",
            ))
        }
    };

    let mut matches: Vec<Value> = Vec::new();
    let mut total = 0usize;
    let mut seen_lines: HashSet<String> = HashSet::new();
    // IDENTICAL LINES ONCE. A task the human repeats per delivery can match dozens
    // of times in one run and push the one body hit past the inline cap. The first
    // occurrence keeps its address.
    let mut take =
        |rid: Value, n: Value, line: &str, matches: &mut Vec<Value>, total: &mut usize| {
            let key = py_split_whitespace(line).join(" ");
            if !seen_lines.insert(key) {
                return;
            }
            *total += 1;
            if matches.len() < limit {
                let cut: String = py_strip(line).chars().take(width).collect();
                matches.push(json!({"id": rid, "line": n, "text": cut}));
            }
        };

    // BODIES FIRST: what a grep is for is the line inside a body never read
    // whole — rows that merely mention the query come after, so the content hit is
    // never the one the inline cap cuts. Bodies live in RAM for this run only.
    let bodies = crate::memory::bodies(agent_id, kernel);
    let mut in_reach: Option<HashSet<i64>> = None;
    if let Some(ids) = &ids {
        // A fold id is a DESCENT: its members are reply rows, and their bodies
        // hang off the pids on their tags (a descent that ignores them finds
        // nothing). An id that is itself a pid counts.
        let rows = load_history(agent_id, kernel, true).await?;
        let scope: HashSet<i64> = expand_ids(&rows, ids).into_iter().collect();
        let mut reach: HashSet<i64> = ids.iter().copied().collect();
        reach.extend(scope.iter().copied());
        for m in &rows {
            if !row_id(m).is_some_and(|i| scope.contains(&i)) {
                continue;
            }
            if m.get("role").and_then(Value::as_str) != Some("user") {
                continue;
            }
            let c = m.get("content").and_then(Value::as_str).unwrap_or("");
            for block in reply_blocks(c) {
                if let Some(p) = pid_of(block).and_then(|t| as_id(&Value::String(t.to_string()))) {
                    reach.insert(p);
                }
            }
        }
        in_reach = Some(reach);
    }
    for (pid, text) in &bodies {
        if in_reach.as_ref().is_some_and(|r| !r.contains(pid)) {
            continue;
        }
        for (n, line) in py_splitlines(text).into_iter().enumerate() {
            if line.to_lowercase().contains(&q) {
                take(json!(pid), json!(n + 1), line, &mut matches, &mut total);
            }
        }
    }
    // YOUR ROWS — the WHOLE conversation, searched by the store itself.
    let mut req = Map::new();
    req.insert("type".to_string(), json!("search"));
    req.insert("query".to_string(), json!(q));
    req.insert("limit".to_string(), json!(1000));
    req.insert("width".to_string(), json!(width));
    if let Some(ids) = &ids {
        req.insert("ids".to_string(), json!(ids));
    }
    let res = store_call(agent_id, kernel, Value::Object(req)).await?;
    for m in res
        .get("matches")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
    {
        let text = m
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        take(
            m.get("id").cloned().unwrap_or(Value::Null),
            m.get("line").cloned().unwrap_or(json!(0)),
            &text,
            &mut matches,
            &mut total,
        );
    }
    let (kept, cut) = fit_reply(
        &matches,
        inline_cap(&agent_meta(agent_id, kernel), model_window),
        Some(limit as i64),
    );
    let more = cut || total > kept.len();
    // PYTHON'S KEY ORDER: matches, total, truncated, next.
    let mut out = Map::new();
    out.insert("matches".to_string(), json!(kept));
    out.insert("total".to_string(), json!(total));
    out.insert("truncated".to_string(), json!(more));
    if more {
        if let Some(last) = kept.last() {
            out.insert(
                "next".to_string(),
                json!({"id": last.get("id"), "line": last.get("line")}),
            );
        }
    }
    Ok(Value::Object(out))
}

const RECALL_PROMPT: &str = "You are rebuilding your own working context. Below is the HEAD of your
conversation as one line per row: id · kind · title · size · ↑parent. Fold rows stand for
finished stretches (their members are one descent away). Everything is currently at
DIGEST resolution; your budget for the body is {budget} tokens and the digests use
{used}. Replies ALREADY IN THIS HISTORY (their calls need not be repeated; the task itself may be unfinished):
{done}
Choose which rows you want IN FULL next turn (expansions cost their size) and say in one
sentence why. Reply JSON only:
{\"expand\": [<id>, ...], \"why\": \"<one sentence>\"}

{lines}";

/// The first balanced `{…}` object in `text`, parsed. Python `_first_json`.
#[must_use]
pub fn first_json(text: &str) -> Option<Map<String, Value>> {
    let i = text.find('{')?;
    let mut depth = 0i32;
    for (off, ch) in text[i..].char_indices() {
        depth += i32::from(ch == '{') - i32::from(ch == '}');
        if depth == 0 {
            return match serde_json::from_str(&text[i..i + off + ch.len_utf8()]) {
                Ok(Value::Object(m)) => Some(m),
                _ => None,
            };
        }
    }
    None
}

/// One plain completion, reasoning stripped. Python `_compose`.
async fn compose(provider: &Arc<dyn Provider>, prompt: &str) -> String {
    let Ok(mut stream) = provider
        .chat(&[json!({"role": "user", "content": prompt})], None)
        .await
    else {
        return String::new();
    };
    let mut parts: Vec<String> = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProviderEvent::Token(t)) => parts.push(t),
            Err(_) => return String::new(),
        }
    }
    without_thinking(&parts.join(""))
}

/// The most of a standing task a clear row carries.
const STANDING_TASK_CAP: usize = 4000;

/// The standing task of a thread: the `task` its opening clear row carried, else its
/// first user message that is not a reply, a fold or bookkeeping. `None` when the
/// thread has none yet.
fn standing_task(rows: &[Value]) -> Option<String> {
    for r in rows {
        let kind = r.get("kind").and_then(Value::as_str);
        if kind == Some("clear") {
            if let Some(t) = r.get("task").and_then(Value::as_str) {
                return Some(t.to_string());
            }
            continue;
        }
        if kind.is_some() || r.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let c = r
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if c.is_empty() || c.starts_with("[reply to ") {
            continue;
        }
        let mut t: String = c.chars().take(STANDING_TASK_CAP).collect();
        if c.chars().count() > STANDING_TASK_CAP {
            t.push_str(" … (cut)");
        }
        return Some(t);
    }
    None
}

/// `context_clear`: a NEW THREAD on the same store — a CLEAR-FOLD: one row whose
/// members are every row since the previous clear (no summary), and the model's
/// view begins at its `[cleared <id> · N rows]` line. Nothing is deleted or
/// forgotten: grep, recall and `history {fold: <id>}` reach every member. The
/// thread's standing task (its first message) is copied onto the clear row, so a
/// restarted agent still has the message that set its role in view (#218).
///
/// args: `why?` (a sentence, whitespace-collapsed and capped at 200). Returns
/// `{cleared, rows}`. Mirrors python `_context_clear`.
pub async fn context_clear(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    model_window: i64,
) -> MemResult<Value> {
    let meta = agent_meta(agent_id, kernel);
    let rows = head(agent_id, kernel).await?;
    // The standing task rides across the clear (#218): the thread's first message
    // is what set the agent's role, and a restart must not lose it.
    let task = standing_task(&rows);
    let members: Vec<i64> = rows
        .iter()
        .filter(|r| {
            !r.get("kind")
                .and_then(Value::as_str)
                .is_some_and(|k| crate::memory::BOOKKEEPING.contains(&k))
        })
        .filter_map(|r| r.get("id").and_then(Value::as_i64))
        .collect();
    // python `" ".join(str(payload.get("why") or "").split())[:200]`.
    let raw = payload.get("why").cloned().unwrap_or(Value::Null);
    let text = if py_truthy(&raw) {
        py_str(&raw)
    } else {
        String::new()
    };
    let why: String = py_split_whitespace(&text)
        .join(" ")
        .chars()
        .take(200)
        .collect();
    let rid = kernel.mint_id();
    save_history(
        agent_id,
        kernel,
        &[json!({
            "role": "user",
            "id": rid,
            "parent": Value::Null,
            "kind": "clear",
            "refs": members,
            "title": if why.is_empty() { "cleared" } else { why.as_str() },
            "content": format!(
                "[cleared {rid} · {} rows]{} — a new thread; the rows before it are this \
                 clear's members, reachable by id (grep, recall, history fold={rid}).{}",
                members.len(),
                if why.is_empty() { String::new() } else { format!(" {why}") },
                task.as_deref().map_or_else(String::new, |t| format!(
                    "\n\nStanding task — the first message of this agent's work, kept \
                     across clears:\n{t}"
                ))
            ),
            "task": task.map_or(Value::Null, Value::from),
        })],
    )
    .await?;
    // The persisted view names rows behind the clear — empty it, so the view is the
    // store from the clear on. RAM is scratch; the store is never cleared.
    save_view(
        agent_id,
        kernel,
        &[],
        agent_budget(&meta, model_window),
        "cleared",
    )
    .await?;
    let st = crate::state::state_for(agent_id);
    *st.projection.lock().expect("projection poisoned") = None;
    *st.compaction_mark.lock().expect("mark poisoned") = None;
    crate::strategies::strip::forget(agent_id.as_str());
    *st.folds_since.lock().expect("folds_since poisoned") = 0;
    *st.recall_due.lock().expect("recall_due poisoned") = false;
    // ONE CHAIN PER AGENT (#227): a new thread starts a new chain (Bug 2).
    st.chain
        .lock()
        .expect("chain poisoned")
        .reset("send: the context was cleared before a turn read this message");
    *st.view_source.lock().expect("view_source poisoned") = None;
    Ok(json!({"cleared": rid, "rows": members.len()}))
}

/// The persisted view entry for a row the view keeps: its id and resolution, and
/// its body when a trimmer changed it, so the next turn renders the same message
/// the model saw before the fold.
fn kept_entry(row: &Value, msg: &Value, res: &str) -> Value {
    let mut e = json!({"id": row_id(row), "res": res});
    if msg != &crate::view::apply_res(row, res) {
        e["content"] = msg["content"].clone();
    }
    e
}

/// A fold or a clear: the rows a span may not cross.
fn is_boundary(row: &Value) -> bool {
    crate::folds::is_fold(row) || row.get("kind").and_then(Value::as_str) == Some("clear")
}

/// `context_fold` (operator only — not in the model's howto): fold a chosen
/// stretch of the agent's VIEW into ONE fold row, the same kind and shape an
/// automatic fold writes (members in `refs`, reachable by `history {fold}`,
/// `recall`, `grep`). Nothing is deleted; the view becomes the rows before the
/// range (unchanged), the fold, then the rows after.
///
/// args: `from?` / `to?` (row ids, inclusive, consecutive in the view; default
/// `from` = the first row after the last fold or clear in the view, default `to`
/// = the last row), `summary?` (the fold's text, used verbatim; absent, the fold
/// summarizer writes it). Returns `{folded, rows, summarized}`; `summarized` is
/// false only when the summarizer gave nothing and the fold carries its titles.
///
/// Refused, with the reason named: an empty range, an id not in the view, a range
/// crossing a fold or clear row, a turn running for this agent.
///
/// # Errors
/// The memory store refused a read or a write; a summary would not fit its budget
/// after every re-summarise; a member has no known place (the turn fails loud,
/// as an automatic fold does).
pub async fn context_fold(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    provider: &Arc<dyn Provider>,
    model_window: i64,
) -> Result<Value, crate::assembly::BlockFail> {
    use crate::folds::anchors_of;
    use crate::memory::load_view;
    use fantastic_kernel::refusal::{bad, refusal_hint, with, BAD_ARG, LOCKED};
    let verb = "context_fold";
    // A turn holds the FIFO lock for its whole run: the view must not move under it.
    let st = crate::state::state_for(agent_id);
    let lock = Arc::clone(&st.lock);
    let Ok(_turn) = lock.try_lock() else {
        return Ok(refusal_hint(
            LOCKED,
            format!("{verb}: a turn is running for {agent_id}"),
            "fold once the running turn has answered",
        ));
    };
    // The view is rendered as the record's caching mode renders it, so the fold
    // persists exactly the view the next turn will send. A primer pins its prefix.
    let meta = agent_meta(agent_id, kernel);
    let (caching, primer) = match crate::caching::settings(&meta) {
        Ok(s) => s,
        Err(why) => {
            let mut v = refusal_hint(
                BAD_ARG,
                format!("{verb}: {why}"),
                "set caching to none, cleanup or provider_cache",
            );
            with(&mut v, "arg", Value::from("caching"));
            return Ok(v);
        }
    };
    // `from` / `to`: absent or null is the default; anything else must be an id.
    let id_arg = |key: &str| -> Result<Option<i64>, Value> {
        match payload.get(key).filter(|v| !v.is_null()) {
            None => Ok(None),
            Some(raw) => as_id(raw)
                .map(Some)
                .ok_or_else(|| bad(verb, key, "a row id (int)")),
        }
    };
    let (from, to) = match (id_arg("from"), id_arg("to")) {
        (Ok(f), Ok(t)) => (f, t),
        (Err(e), _) | (_, Err(e)) => return Ok(e),
    };
    // `summary`: absent or null writes the summarizer's; a given one is the text.
    let summary = match payload.get("summary").filter(|v| !v.is_null()) {
        None => None,
        Some(Value::String(s)) if !s.trim().is_empty() => Some(s.as_str()),
        Some(_) => return Ok(bad(verb, "summary", "a non-empty string")),
    };
    let rows = load_history(agent_id, kernel, false).await?;
    let anchors = anchors_of(&crate::memory::head(agent_id, kernel).await?);
    let listed = listed_entries(&rows, load_view(agent_id, kernel).await?.as_ref());
    let view = render_history(&rows, listed.as_deref(), caching.cleans());
    let pos_of = |id: i64| view.iter().position(|r| row_id(&rows[r.idx]) == Some(id));
    let empty = |arg: &str, why: &str| {
        let mut v = refusal_hint(
            BAD_ARG,
            format!("{verb}: the range is empty — {why}"),
            "name a range of rows that are in the view, from first to last",
        );
        with(&mut v, "arg", Value::from(arg));
        v
    };
    if view.is_empty() {
        return Ok(empty("from", "the view holds no rows"));
    }
    // The default `from` is the row after the last fold or clear in the view.
    let default_from = view
        .iter()
        .rposition(|r| is_boundary(&rows[r.idx]))
        .map_or(0, |p| p + 1);
    let from_p = match from {
        None => default_from,
        Some(id) => match pos_of(id) {
            Some(p) => p,
            None => return Ok(bad(verb, "from", "a row id in this agent's view")),
        },
    };
    let to_p = match to {
        None => view.len() - 1,
        Some(id) => match pos_of(id) {
            Some(p) => p,
            None => return Ok(bad(verb, "to", "a row id in this agent's view")),
        },
    };
    if from_p > to_p {
        return Ok(empty("to", "to comes before from"));
    }
    // THE PRIMER IS PINNED: the cached prefix ends at its row, so a range that starts at
    // or before that row (a range before it, or one that includes it) would move bytes
    // the provider has cached. A primer not in this view pins nothing.
    if let Some(pp) = primer.and_then(pos_of) {
        if from_p <= pp {
            let mut v = refusal_hint(
                BAD_ARG,
                format!(
                    "{verb}: the primer is pinned — row {} ends the cached prefix, so a range \
                     may not start at or before it",
                    primer.unwrap_or_default()
                ),
                "fold rows after the primer; the primer and the rows before it stay as they are",
            );
            with(&mut v, "arg", Value::from("from"));
            return Ok(v);
        }
    }
    let span: Vec<usize> = view[from_p..=to_p].iter().map(|r| r.idx).collect();
    if span.iter().any(|&i| is_boundary(&rows[i])) {
        let mut v = refusal_hint(
            BAD_ARG,
            format!("{verb}: the range crosses a fold or clear row"),
            "fold the rows on each side of it separately; a fold row is never folded again by a range",
        );
        with(&mut v, "arg", Value::from("from"));
        return Ok(v);
    }
    let anchor =
        member_anchor(&rows, &span, &anchors).map_err(crate::assembly::BlockFail::FoldAnchor)?;
    let (title, body, summarized) = match summary {
        // Verbatim: the body IS the given text, its first line the title.
        Some(text) => (summary_title(text), text.to_string(), true),
        None => {
            let (title, body, degraded) = summarize_span(provider, &rows, &span).await?;
            (title, body, !degraded)
        }
    };
    let fold = append_fold(
        agent_id,
        kernel,
        &rows,
        &span,
        anchor,
        &title,
        &body,
        !summarized,
    )
    .await?;
    let fid = row_id(&fold).unwrap_or_default();
    // THE NEW VIEW: the rows before the range as they were rendered (a trimmed body
    // kept as trimmed, a dropped row left out), the fold where the range began, then
    // the rows after. The fold is the newest row, so the array carries every kept row.
    let mut ids: Vec<Value> = Vec::with_capacity(view.len() - span.len() + 1);
    for (p, r) in view.iter().enumerate() {
        if p == from_p {
            ids.push(json!({"id": fid, "res": "fold"}));
        }
        if (from_p..=to_p).contains(&p) {
            continue;
        }
        ids.push(kept_entry(&rows[r.idx], &r.msg, &r.res));
    }
    save_view(
        agent_id,
        kernel,
        &ids,
        agent_budget(&meta, model_window),
        verb,
    )
    .await?;
    Ok(json!({"folded": fid, "rows": span.len(), "summarized": summarized}))
}

/// `context_forget`: compact this agent's OWN store — drop the rows the model can
/// no longer see and keep every fold. Python `_context_forget`, rule for rule:
/// under the turn lock, ONE `anchors` row for the folds written without an
/// `anchor` whose place is about to rest on it, ONE `prune {ids}`, and the head
/// cache read again from the store (the prune cascaded each pruned member out of
/// every fold's refs).
///
/// # Errors
/// The memory store refused a read, the append or the prune.
pub async fn context_forget(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> MemResult<Value> {
    use crate::folds::{anchors_of, chains, is_fold, is_open, recorded_anchors, refs_of};
    use crate::memory::{refresh_head, rows_of, BOOKKEEPING, TURN_ROLES};
    let include_cleared = match payload.get("include_cleared") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Ok(fantastic_kernel::refusal::bad(
                "context_forget",
                "include_cleared",
                "a bool",
            ))
        }
    };
    let st = crate::state::state_for(agent_id);
    let lock = Arc::clone(&st.lock);
    let _turn = lock.lock().await;
    let kind = |r: &Value| r.get("kind").and_then(Value::as_str).map(str::to_string);
    let head_rows = refresh_head(agent_id, kernel).await?;
    let everything = rows_of(
        &store_call(agent_id, kernel, json!({"type": "read"})).await?,
        agent_id,
    );
    let anchors = anchors_of(&head_rows);
    let by_id: std::collections::HashMap<i64, &Value> = head_rows
        .iter()
        .filter_map(|r| row_id(r).map(|id| (id, r)))
        .collect();
    let head_folds: Vec<&Value> = head_rows.iter().filter(|r| is_fold(r)).collect();
    let turns: Vec<Value> = head_rows
        .iter()
        .filter(|r| !kind(r).is_some_and(|k| BOOKKEEPING.contains(&k.as_str())))
        .filter(|r| {
            r.get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| TURN_ROLES.contains(&role))
        })
        .cloned()
        .collect();
    let open_ids: HashSet<i64> = chains(&turns)
        .into_iter()
        .filter(|c| is_open(&turns, c))
        .flat_map(|c| {
            c.into_iter()
                .filter_map(|i| row_id(&turns[i]))
                .collect::<Vec<_>>()
        })
        .collect();
    let mut doomed: std::collections::BTreeSet<i64> = std::collections::BTreeSet::new();
    // 1. the rows folds already summarise — under a fold whose place is known
    for f in &head_folds {
        if !row_id(f).is_some_and(|id| anchors.contains_key(&id)) {
            continue;
        }
        let mut stack = refs_of(f);
        let mut seen: HashSet<i64> = HashSet::new();
        while let Some(x) = stack.pop() {
            if !seen.insert(x) {
                continue;
            }
            let Some(r) = by_id.get(&x) else {
                continue;
            };
            if is_fold(r) {
                stack.extend(refs_of(r));
            } else if !open_ids.contains(&x) {
                doomed.insert(x);
            }
        }
    }
    // 2. every view row but the one in force
    let current = head_rows
        .iter()
        .rev()
        .find(|r| kind(r).as_deref() == Some("view"))
        .and_then(row_id);
    doomed.extend(
        everything
            .iter()
            .filter(|r| kind(r).as_deref() == Some("view"))
            .filter_map(row_id)
            .filter(|id| Some(*id) != current),
    );
    // 3. behind the last clear, on request; the clear row itself stays
    if include_cleared {
        if let Some(last) = everything
            .iter()
            .rposition(|r| kind(r).as_deref() == Some("clear"))
        {
            doomed.extend(everything[..last].iter().filter_map(row_id));
        }
    }
    // A fold written before folds carried `anchor` is placed by deriving from its
    // members — about to go. Its place is recorded first, in ONE `anchors` row; an
    // older `anchors` row is superseded by the newest one that holds it all.
    let want: std::collections::BTreeMap<i64, i64> = head_folds
        .iter()
        .filter(|f| crate::folds::own_anchor(f).is_none())
        .filter_map(|f| row_id(f).and_then(|id| anchors.get(&id).map(|a| (id, *a))))
        .collect();
    let rows_of_anchors: Vec<&Value> = head_rows
        .iter()
        .filter(|r| kind(r).as_deref() == Some("anchors"))
        .collect();
    let newest = rows_of_anchors.last().copied();
    let holds = newest.is_some_and(|n| {
        let rec = recorded_anchors(std::slice::from_ref(n));
        want.iter().all(|(k, v)| rec.get(k) == Some(v))
    });
    if !want.is_empty() && !holds {
        let map: Map<String, Value> = want
            .iter()
            .map(|(k, v)| (k.to_string(), Value::from(*v)))
            .collect();
        save_history(
            agent_id,
            kernel,
            &[json!({"kind": "anchors", "v": 1, "anchors": map})],
        )
        .await?;
        doomed.extend(rows_of_anchors.iter().filter_map(|r| row_id(r)));
    } else {
        let keep = if want.is_empty() {
            None
        } else {
            newest.and_then(row_id)
        };
        doomed.extend(
            rows_of_anchors
                .iter()
                .filter_map(|r| row_id(r))
                .filter(|id| Some(*id) != keep),
        );
    }
    let mut pruned = 0i64;
    if !doomed.is_empty() {
        let res = store_call(
            agent_id,
            kernel,
            json!({"type": "prune", "ids": doomed.iter().collect::<Vec<_>>()}),
        )
        .await?;
        pruned = res.get("pruned").and_then(Value::as_i64).unwrap_or(0);
    }
    // THE STORE IS THE TRUTH: the prune cascaded each pruned member out of every
    // fold's refs, so the head cache is read again, not patched.
    crate::memory::forget_head(agent_id, kernel);
    let fresh = head(agent_id, kernel).await?;
    let after = anchors_of(&fresh);
    let left: Vec<&Value> = fresh.iter().filter(|r| is_fold(r)).collect();
    let lost = left
        .iter()
        .filter(|f| !row_id(f).is_some_and(|id| after.contains_key(&id)))
        .count();
    Ok(json!({"pruned": pruned, "kept_folds": left.len(), "unanchored": lost}))
}

/// `context_recall`: rebuild this agent's VIEW from its store — with
/// `context_strategy` fold, a head longer than the budget is FOLDED first (oldest
/// spans into fold rows, as a turn's projection folds); then fit the head to the
/// budget at digest resolution, ask the model which rows it wants in full,
/// persist the id list, log the decision as a row, and drop RAM scratch. Runs on
/// boot, after a fold batch, or on request. `client_id` is the turn's caller
/// (none for the verb); a recall that folded tells it, as a projection does.
///
/// args: `dry?` (mechanical fit only — no model call). Returns
/// `{kept, expanded, dropped, folded, budget, why, degraded}`.
pub async fn context_recall(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    provider: Option<&Arc<dyn Provider>>,
    model_window: i64,
    system_prompt: Option<&str>,
    client_id: Option<&str>,
) -> Result<Value, crate::assembly::BlockFail> {
    let meta = agent_meta(agent_id, kernel);
    let mut rows = load_history(agent_id, kernel, false).await?;
    if rows.is_empty() {
        return Ok(
            json!({"kept": 0, "expanded": 0, "dropped": 0, "folded": 0, "why": "empty store", "degraded": false}),
        );
    }
    // The block the turn will actually use (`system_content`), not the default one:
    // a record or per-call `system_prompt` changes how much room the body gets.
    let sys_tokens = estimate_one(&json!({
        "role": "system",
        "content": system_content(agent_id, kernel, system_prompt).await?,
    }));
    let body_budget = (agent_budget(&meta, model_window) - sys_tokens - NOTICE_ENVELOPE_RESERVE)
        .max(BUDGET_FLOOR);
    let target = (body_budget as f64 * LOW) as i64;
    // PYTHON TRUTHINESS: `dry:1`, `dry:"false"` and `dry:[0]` are all dry.
    // `as_bool` accepted only a JSON `true`, so a model that wrote `1` got a
    // model call it had asked not to make.
    let dry = payload.get("dry").is_some_and(py_truthy);
    let provider = provider.filter(|_| !dry);
    // A HEAD LONGER THAN THE BUDGET FOLDS FIRST. A store
    // written under a larger window — another model, a planted past — is folded
    // the way the turn's own `fold` projection folds it: oldest spans into fold
    // rows, folds of folds as they pile up; the fit below then only stubs replies
    // and drops what still does not fit. Folding takes the model (a summary per
    // span), so a `dry` recall does not fold.
    let mut folded = 0usize;
    let anchors = crate::folds::anchors_of(&head(agent_id, kernel).await?);
    if let Some(p) = provider {
        if meta.get("context_strategy").and_then(Value::as_str) == Some("fold") {
            let fctx = ProjectionCtx {
                budget: body_budget,
                recent_n: cfg_recent_n(&meta),
                provider: p,
                self_id: agent_id,
                kernel,
                client_id: client_id.unwrap_or(""),
                anchors: &anchors,
            };
            let (r, _, f, _) = fold_down(&rows, &fctx, body_budget).await?;
            rows = r;
            folded = f;
        }
    }
    let view: Vec<Value> = fold_render(&rows, &anchors)
        .into_iter()
        .filter(|r| row_id(r).is_some())
        .collect();
    if view.is_empty() {
        return Ok(
            json!({"kept": 0, "expanded": 0, "dropped": 0, "folded": 0, "why": "no addressable rows", "degraded": false}),
        );
    }
    let digest = |r: &Value| -> Value {
        if res_of(r) == "full" && row_content(r).starts_with("[reply to ") {
            apply_res(r, "stub")
        } else {
            r.clone()
        }
    };
    let cost = |r: &Value| -> i64 {
        estimate_one(&json!({
            "role": r.get("role").and_then(Value::as_str).unwrap_or("user"),
            "content": row_content(r),
        }))
    };
    // 1. MECHANICAL FIT — never fails. The goal turn always in; then newest first.
    let mut picked: HashSet<usize> = HashSet::from([0]);
    let mut used = cost(&digest(&view[0]));
    for i in (1..view.len()).rev() {
        let c = cost(&digest(&view[i]));
        if used + c > target {
            break;
        }
        picked.insert(i);
        used += c;
    }
    let mut head: Vec<usize> = picked.into_iter().collect();
    head.sort_unstable();

    // 2. ONE MODEL CALL — the model chooses expansions; a bad answer changes nothing.
    let mut why = "mechanical fit".to_string();
    let mut degraded = true;
    let mut expand: HashSet<i64> = HashSet::new();
    if let Some(provider) = provider {
        let lines: Vec<String> = head
            .iter()
            .map(|&i| {
                let r = &view[i];
                let content = row_content(r);
                // python `r.get("title") or (<quoted title> or content[:80])` —
                // TRUTHINESS, so an empty title falls through, and a non-string
                // one is printed by `str()`.
                let title = match r.get("title") {
                    Some(t) if py_truthy(t) => py_str(t),
                    _ => match quoted_title(content) {
                        Some(t) => t.to_string(),
                        None => content
                            .chars()
                            .take(80)
                            .collect::<String>()
                            .replace('\n', " "),
                    },
                };
                // `kind or role`, printed RAW through `str()`.
                let kind = match r.get("kind") {
                    Some(k) if py_truthy(k) => py_str(k),
                    _ => r.get("role").map_or_else(String::new, py_str),
                };
                // `parent or '-'`: truthiness, so a 0/null parent is `-`.
                let parent = match r.get("parent") {
                    Some(p) if py_truthy(p) => py_str(p),
                    _ => "-".to_string(),
                };
                format!(
                    "{} · {kind} · \"{title}\" · {}B · ↑{parent}",
                    row_id(r).unwrap_or(0),
                    content.chars().count()
                )
            })
            .collect();
        // What the store already answers: every reply row names the call it
        // closed, so a rebooted model does not re-run reads whose results it is
        // about to see.
        let mut done: Vec<String> = Vec::new();
        for r in &view {
            let content = row_content(r);
            let Some((agent, verb)) = parse_reply_tag(content) else {
                continue;
            };
            let line = match quoted_title(content) {
                Some(t) => format!("{agent} {verb} · \"{t}\""),
                None => format!("{agent} {verb}"),
            };
            if !done.contains(&line) {
                done.push(line);
            }
        }
        let done_text = if done.is_empty() {
            "(nothing yet)".to_string()
        } else {
            done[done.len().saturating_sub(40)..].join("\n")
        };
        let prompt = RECALL_PROMPT
            .replace("{budget}", &body_budget.to_string())
            .replace("{used}", &used.to_string())
            .replace("{done}", &done_text)
            .replace("{lines}", &lines.join("\n"));
        // python `if ans:` — an EMPTY object is no answer at all, so `degraded`
        // stays true and `why` stays `mechanical fit`.
        let answered = first_json(&compose(provider, &prompt).await).filter(|a| !a.is_empty());
        if let Some(ans) = answered {
            let head_ids: HashSet<i64> = head.iter().filter_map(|&i| row_id(&view[i])).collect();
            // python `_as_list(ans.get("expand"))`: a list as is, a scalar
            // wrapped, falsy empty.
            let raw_expand = ans.get("expand").unwrap_or(&Value::Null);
            let items: Vec<Value> = if !py_truthy(raw_expand) {
                Vec::new()
            } else if let Value::Array(a) = raw_expand {
                a.clone()
            } else {
                vec![raw_expand.clone()]
            };
            let want: Vec<i64> = items
                .iter()
                .filter_map(as_id)
                .filter(|x| head_ids.contains(x))
                .collect();
            // python `str(ans.get("why") or "")[:300] or "chosen"` — `42` reads
            // `42` and `["a"]` reads `['a']`; only a FALSY why is `chosen`.
            why = match ans.get("why") {
                Some(w) if py_truthy(w) => py_str(w).chars().take(300).collect::<String>(),
                _ => String::new(),
            };
            if why.is_empty() {
                why = "chosen".to_string();
            }
            degraded = false;
            // charge each expansion at full size, newest first; drop what does not fit
            let mut total = used;
            for x in want.iter().rev() {
                let Some(r) = view.iter().find(|v| row_id(v) == Some(*x)) else {
                    continue;
                };
                let extra = cost(r) - cost(&digest(r));
                if total + extra <= body_budget {
                    expand.insert(*x);
                    total += extra;
                }
            }
        }
    }
    let ids: Vec<Value> = head
        .iter()
        .map(|&i| {
            let r = &view[i];
            let id = row_id(r).unwrap_or(0);
            let res = if r.get("kind").and_then(Value::as_str) == Some("fold") {
                "fold"
            } else if expand.contains(&id) || !row_content(r).starts_with("[reply to ") {
                "full"
            } else {
                "stub"
            };
            json!({"id": id, "res": res})
        })
        .collect();
    save_view(agent_id, kernel, &ids, body_budget, &why).await?;
    // the decision is a ROW — the next reload reads why the last recall chose as it did
    let rid = kernel.mint_id();
    // THE LIVE TASK IS NAMED. The view keeps the first root (the goal) at the top,
    // and after a recall a model with several roots can re-answer the FIRST one
    // and call done. The newest root is
    // what is live; say so. Over the STORE, not the folded render: a root a fold
    // collapsed is still the live one if nothing newer came.
    let live = rows.iter().rev().find(|r| {
        r.get("role").and_then(Value::as_str) == Some("user")
            // python `not r.get("parent")`: a 0 parent is falsy too.
            && !r.get("parent").is_some_and(py_truthy)
            // python `r.get("kind") in (None, "", "user")` — EXACTLY those three.
            && matches!(
                r.get("kind"),
                None | Some(Value::Null) | Some(Value::String(_))
            )
            && r.get("kind")
                .and_then(Value::as_str)
                .is_none_or(|k| k.is_empty() || k == "user")
            && parse_reply_tag(row_content(r)).is_none()
    });
    let live_txt = live.map_or(String::new(), |l| {
        let text: String = py_split_whitespace(row_content(l))
            .join(" ")
            .chars()
            .take(100)
            .collect();
        format!(" · live task: {} \"{text}\"", row_id(l).unwrap_or(0))
    });
    save_history(
        agent_id,
        kernel,
        &[json!({
            "role": "user",
            "id": rid,
            "parent": Value::Null,
            "kind": "recall",
            "title": why,
            "content": format!(
                "[recall {rid} · kept {} rows, {} in full] {why}{live_txt}",
                ids.len(), expand.len()
            ),
        })],
    )
    .await?;
    // RAM is scratch, reconstructible by definition; the store is never cleared.
    let st = crate::state::state_for(agent_id);
    *st.projection.lock().expect("projection poisoned") = None;
    *st.compaction_mark.lock().expect("mark poisoned") = None;
    crate::strategies::strip::forget(agent_id.as_str());
    if let (true, Some(c)) = (folded > 0, client_id.filter(|c| !c.is_empty())) {
        // The caller is told its context was compacted, as by a turn's projection.
        to_caller(
            kernel,
            agent_id,
            c,
            json!({
                "type": "context", "source": agent_id.as_str(), "ts": now_secs(),
                "phase": "compacted",
                "detail": {
                    "strategy": "fold",
                    "recall": true,
                    "dropped_messages": view.len() - ids.len(),
                    "kept_messages": ids.len(),
                    "summarized": false,
                    "pinned_facts": 0,
                    "stripped": 0,
                    "stripped_bytes": 0,
                    "folded": folded,
                },
            }),
        )
        .await;
    }
    Ok(json!({
        "kept": ids.len(),
        "expanded": expand.len(),
        "dropped": view.len() - ids.len(),
        "folded": folded,
        "budget": body_budget,
        "why": why,
        "degraded": degraded,
    }))
}
