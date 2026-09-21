//! FULL ASYNC retrieval: page a stored reply body by pid (`pull`), or FIND the
//! line first (`grep`). Plus `context_recall` — rebuild the agent's VIEW from
//! its store.
//!
//! These three are what make an unbounded reply non-fatal. A reply too large to
//! inline is stored under its pid and the model is handed a digest; it then
//! greps for the line it wants and pulls only that. Without them a 240KB ledger
//! dump is a turn that cannot fit.
//!
//! Mirrors python `ai_core/core.py` `_pull` / `_grep` / `_context_recall`.

use crate::assembly::default_system_content;
use crate::context::{
    budget as agent_budget, estimate_one, fit_reply, inline_cap, pull_cap_chars, BUDGET_FLOOR,
    NOTICE_ENVELOPE_RESERVE,
};
use crate::folds::{expand_ids, quoted_title, render as fold_render, row_content, row_id};
use crate::helpers::{agent_meta, DEFAULT_CLIENT_ID};
use crate::history::{append_history, load_history};
use crate::history_view::{parse_reply_tag, reply_blocks};
use crate::provider::{Provider, ProviderEvent};
use crate::strategies::strip::{pid_of, LOW};
use crate::think::without_thinking;
use crate::view::{apply_res, res_of, save_view};
use fantastic_bundle::{
    decimal_value, py_int, py_int_or, py_json_dumps, py_set_order, py_split_whitespace,
    py_splitlines, py_str, py_strip, py_truthy,
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
/// need not be (`'0'` → 0, which `pull` then answers as a readable not_found).
/// The string is python-stripped and its digits are any Unicode decimal (`'٣'`
/// → 3); `'+5'` is not a digit string and is not an id.
#[must_use]
pub fn as_id(x: &Value) -> Option<i64> {
    match x {
        Value::Bool(_) => None,
        // python: a float is not an `int`.
        Value::Number(n) if n.is_f64() => None,
        Value::Number(n) => n.as_i64().filter(|&i| i > 0),
        Value::String(s) => {
            let t = py_strip(s);
            if !t.is_empty() && t.chars().all(|c| decimal_value(c).is_some()) {
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

/// `pull`: read ONE PAGE of a reply body that was too large to inline.
///
/// args: `pid` (req — the pid on a settled reply's tag), `offset?` (0),
/// `length?` (chars; default and max = the inline cap). Returns
/// `{pid, offset, text, next_offset, eof}` — page with `offset: next_offset`
/// until `eof`.
pub async fn pull(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let Some(pid) = payload.get("pid").and_then(as_id) else {
        return bad_arg(
            "pull: `pid` (an int) is required — it is on the reply's tag line",
            "pid",
        );
    };
    let meta = agent_meta(agent_id, kernel);
    let cap = pull_cap_chars(&meta) as i64;
    // python: `offset = max(0, int(payload.get("offset") or 0))` and
    // `length = int(payload.get("length") or cap)` under ONE try/except — a
    // non-integer either side is the SAME refusal, and it carries no `arg`.
    let null = Value::Null;
    let (offset, length) = match (
        py_int_or(payload.get("offset").unwrap_or(&null), 0),
        py_int_or(payload.get("length").unwrap_or(&null), cap),
    ) {
        (Ok(o), Ok(l)) => (o.max(0), l),
        _ => {
            return json!({
                "error": "pull: offset/length must be integers",
                "reason": "bad_arg",
            })
        }
    };
    if length < 0 {
        return json!({
            "error": "pull: length must not be negative",
            "reason": "bad_arg",
        });
    }
    if length > cap {
        return bad_arg(
            &format!(
                "pull: length {length} exceeds the {cap}-char page cap — a page must fit \
                 INLINE where it is going; ask for less and page"
            ),
            "length",
        );
    }
    let got = kernel.rows.body_get(
        pid,
        usize::try_from(offset).unwrap_or(0),
        usize::try_from(length.max(1)).unwrap_or(1),
    );
    if got.is_none() {
        // A fold id: its body is on the row itself.
        for r in load_history(agent_id, kernel, DEFAULT_CLIENT_ID).await {
            if r.get("kind").and_then(Value::as_str) == Some("fold") && row_id(&r) == Some(pid) {
                // python `str(r.get("body") or "")` — a falsy body is the empty
                // string, a non-string one is its `str()`.
                let body = r.get("body").cloned().unwrap_or(Value::Null);
                let text = if py_truthy(&body) {
                    py_str(&body)
                } else {
                    String::new()
                };
                let chars: Vec<char> = text.chars().collect();
                let n = chars.len() as i64;
                // `text[:length]` with PYTHON SLICE SEMANTICS: a negative length
                // counts from the end, and `next_offset`/`eof` stay signed.
                let end = if length < 0 {
                    (n + length).max(0)
                } else {
                    length.min(n)
                };
                let cut: String = chars[..usize::try_from(end).unwrap_or(0)].iter().collect();
                return json!({
                    "pid": pid,
                    "offset": 0,
                    "text": cut,
                    "next_offset": length.min(n),
                    "eof": length >= n,
                    "refs": match r.get("refs") {
                        Some(v) if py_truthy(v) => v.clone(),
                        _ => json!([]),
                    },
                    "title": r.get("title").cloned().unwrap_or(Value::Null),
                });
            }
        }
        return json!({
            "error": format!(
                "pull: no stored body for pid {pid} — only replies too large to inline \
                 are stored; small ones were already in your turn"
            ),
            "reason": "not_found",
        });
    }
    let (text, nxt, eof, total) = got.expect("checked above");
    let cap_tok = inline_cap(&meta);
    let reply = |t: &str, n: usize, e: bool| json!({"pid": pid, "offset": offset, "text": t, "next_offset": n, "eof": e, "total": total});
    // A PAGE FITS INLINE, MEASURED SERIALIZED. Measured 2026-09-06 (python): raw
    // pages sized to the cap in chars came back OVER it once JSON-escaped
    // (newlines, quotes) and were dehydrated — the model pulled its own pull.
    // Shrink until the reply itself fits.
    let mut text = text;
    let mut out = reply(&text, nxt, eof);
    while text.chars().count() > 64
        && estimate_one(&json!({
            "role": "user",
            "content": py_json_dumps(&out, true, None),
        })) > cap_tok
    {
        let keep = (text.chars().count() as f64 * 0.8) as usize;
        text = text.chars().take(keep).collect();
        out = reply(&text, offset as usize + text.chars().count(), false);
    }
    out
}

/// `grep`: find the LINE that mentions something, wherever it is — a stored body
/// never pulled, a digest line, an old turn.
///
/// args: `query` (req — case-insensitive substring), `ids?` (row ids / pids /
/// fold ids to search; default: every row of this conversation and every stored
/// reply body it received), `limit?`, `width?` (chars of each matched line, from
/// its start; default 200). Returns `{matches:[{id, line, text}], total, truncated}`.
///
/// THE SUPER SIMPLE ONE, deliberately. Measured 2026-09-06 (python): the model
/// spent four ~10k-token thinking turns byte-scanning one 6.8KB body in 300-char
/// pages to find a single line. "Find the line that mentions X" is the operation
/// it wanted, and this is it — substring, case-insensitive, over rows and stored
/// bodies, capped.
pub async fn grep(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
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
        return bad_arg("grep: `query` is required", "query");
    }
    // python `int(payload.get(k) or dflt)`, each knob under its OWN try
    // (2026-09-20): a bad `limit` no longer leaves `width` unbound.
    let num = |k: &str, dflt: i64| py_int_or(payload.get(k).unwrap_or(&null), dflt);
    let limit = num("limit", 100).map_or(100, |l| l.clamp(1, 100)) as usize;
    let width = num("width", 200).map_or(200, |w| w.clamp(20, 20_000)) as usize;

    let raw_ids = payload.get("ids");
    let ids: Option<Vec<i64>> = match raw_ids {
        Some(Value::Array(a)) => {
            let parsed: Vec<Option<i64>> = a.iter().map(as_id).collect();
            if parsed.iter().any(Option::is_none) {
                return bad_arg(
                    "grep: `ids` must be a list of message ids / pids (ints)",
                    "ids",
                );
            }
            Some(parsed.into_iter().flatten().collect())
        }
        Some(Value::Null) | None => None,
        Some(_) => {
            return bad_arg(
                "grep: `ids` must be a list of message ids / pids (ints)",
                "ids",
            )
        }
    };

    let rows = load_history(agent_id, kernel, DEFAULT_CLIENT_ID).await;
    // A fold id in `ids` is a DESCENT: it expands to the rows it collapsed. An
    // EMPTY list is falsy in python, so it means NO SCOPE — not "scope to
    // nothing", which scoped every row out.
    let scope: Option<Vec<i64>> = ids
        .as_ref()
        .filter(|v| !v.is_empty())
        .map(|v| py_set_order(&expand_ids(&rows, v)));
    let in_scope = |id: Option<i64>| -> bool {
        match &scope {
            None => true,
            Some(s) => id.is_some_and(|i| s.contains(&i)),
        }
    };

    let mut matches: Vec<Value> = Vec::new();
    let mut total = 0usize;
    let mut seen_lines: HashSet<String> = HashSet::new();
    // IDENTICAL LINES ONCE. A task the human repeats per delivery matched 28
    // times in one run and pushed the one body hit past the inline cap (measured
    // 2026-09-06, python recall_after_fold). The first occurrence keeps its
    // address.
    //
    // PYTHON'S LINE SET: `str.splitlines()` splits on `\r`, `\v`, `\f` and the
    // information separators too, and the dedupe key / the cut use python's
    // whitespace. Rust's `lines()` saw ONE line where python saw four.
    let mut scan = |rid: i64, text: &str, matches: &mut Vec<Value>, total: &mut usize| {
        for (n, line) in py_splitlines(text).into_iter().enumerate() {
            if !line.to_lowercase().contains(&q) {
                continue;
            }
            let key = py_split_whitespace(line).join(" ");
            if !seen_lines.insert(key) {
                continue;
            }
            *total += 1;
            if matches.len() < limit {
                let cut: String = py_strip(line).chars().take(width).collect();
                matches.push(json!({"id": rid, "line": n + 1, "text": cut}));
            }
        }
    };

    // A row's text: the content when it is a string, else `json.dumps(content,
    // default=str)` — ensure_ascii, so `{"k": "é"}` is scanned as
    // `{"k": "é"}` and grep `é` matches nothing while grep `u00e9` does.
    let row_text = |m: &Value| -> String {
        match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            other => py_json_dumps(other.unwrap_or(&Value::Null), true, None),
        }
    };

    // The bodies in reach: the pids on the reply tags of the rows considered —
    // every row of the conversation, or, under a scope, the rows the scope names
    // (a fold's members ARE reply rows, and their bodies hang off the pids in
    // their tags — a descent that ignored them found nothing, measured
    // 2026-09-08, python: 22 greps, 22 zeros). An id that is itself a pid counts.
    let mut pids: Vec<i64> = Vec::new();
    for m in &rows {
        if !in_scope(row_id(m)) {
            continue;
        }
        if m.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = row_text(m);
        for block in reply_blocks(&text) {
            if let Some(p) = pid_of(block).and_then(|t| as_id(&Value::String(t.to_string()))) {
                pids.push(p);
            }
        }
    }
    if let Some(s) = &scope {
        let mut ordered: Vec<i64> = s.clone();
        ordered.extend(pids);
        let mut seen = HashSet::new();
        pids = ordered.into_iter().filter(|p| seen.insert(*p)).collect();
    }
    // STORED BODIES FIRST: what a grep is for is the line inside a body never
    // pulled — rows that merely mention the query (the task, the model's own
    // plan) come after, so the content hit is never the one the inline cap cuts.
    for pid in &pids {
        if let Some((text, _, _, _)) = kernel.rows.body_get(*pid, 0, 4 * 1024 * 1024) {
            scan(*pid, &text, &mut matches, &mut total);
        }
    }
    for m in &rows {
        let Some(rid) = row_id(m) else { continue };
        if !in_scope(Some(rid)) {
            continue;
        }
        let text = row_text(m);
        scan(rid, &text, &mut matches, &mut total);
    }
    let (kept, cut) = fit_reply(
        &matches,
        inline_cap(&agent_meta(agent_id, kernel)),
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
    Value::Object(out)
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
        .chat(&[json!({"role": "user", "content": prompt})])
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

/// `context_recall`: rebuild this agent's VIEW from its store — fit the head to
/// the budget at digest resolution, then ask the model which rows it wants in
/// full, persist the id list, log the decision as a row, and drop RAM scratch.
/// Runs on boot, after a fold batch, or on request.
///
/// args: `dry?` (mechanical fit only — no model call). Returns
/// `{kept, expanded, dropped, budget, why, degraded}`.
pub async fn context_recall(
    agent_id: &AgentId,
    payload: &Value,
    kernel: &Arc<Kernel>,
    provider: Option<&Arc<dyn Provider>>,
) -> Value {
    let meta = agent_meta(agent_id, kernel);
    let rows = load_history(agent_id, kernel, DEFAULT_CLIENT_ID).await;
    if rows.is_empty() {
        return json!({"kept": 0, "expanded": 0, "dropped": 0, "why": "empty store", "degraded": false});
    }
    let sys_tokens = estimate_one(&json!({
        "role": "system",
        "content": default_system_content(agent_id, kernel).await,
    }));
    let body_budget =
        (agent_budget(&meta) - sys_tokens - NOTICE_ENVELOPE_RESERVE).max(BUDGET_FLOOR);
    let target = (body_budget as f64 * LOW) as i64;
    let view: Vec<Value> = fold_render(&rows)
        .into_iter()
        .filter(|r| row_id(r).is_some())
        .collect();
    if view.is_empty() {
        return json!({"kept": 0, "expanded": 0, "dropped": 0, "why": "no addressable rows", "degraded": false});
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
    // PYTHON TRUTHINESS: `dry:1`, `dry:"false"` and `dry:[0]` are all dry.
    // `as_bool` accepted only a JSON `true`, so a model that wrote `1` got a
    // model call it had asked not to make.
    let dry = payload.get("dry").is_some_and(py_truthy);
    if let (Some(provider), false) = (provider, dry) {
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
            // wrapped, falsy empty (2026-09-20; a truthy scalar used to raise).
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
    save_view(agent_id, kernel, &ids, body_budget, &why);
    // the decision is a ROW — the next reload reads why the last recall chose as it did
    let rid = kernel.rows.mint_id();
    // THE LIVE TASK IS NAMED. The view keeps the first root (the goal) at the top,
    // and after a recall a model with several roots re-answered the FIRST one and
    // called done (measured 2026-09-06, python fold_paging). The newest root is
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
    append_history(
        agent_id,
        kernel,
        DEFAULT_CLIENT_ID,
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
    .await;
    // RAM is scratch, reconstructible by definition; the store is never cleared.
    let st = crate::state::state_for(agent_id);
    *st.projection.lock().expect("projection poisoned") = None;
    *st.compaction_mark.lock().expect("mark poisoned") = None;
    crate::strategies::strip::forget(agent_id.as_str());
    json!({
        "kept": ids.len(),
        "expanded": expand.len(),
        "dropped": view.len() - ids.len(),
        "budget": body_budget,
        "why": why,
        "degraded": degraded,
    })
}
