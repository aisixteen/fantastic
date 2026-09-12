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
use fantastic_kernel::{AgentId, Kernel};
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Arc;

/// A message id as an int — an int, or its decimal string (a model quotes what
/// it copied out of a tag). Anything else is not an id. Python `_as_id`.
#[must_use]
pub fn as_id(x: &Value) -> Option<i64> {
    match x {
        Value::Bool(_) => None,
        Value::Number(n) => n.as_i64().filter(|&i| i > 0),
        Value::String(s) => {
            let t = s.trim();
            if !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) {
                t.parse::<i64>().ok().filter(|&i| i > 0)
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
    let cap = pull_cap_chars(&meta);
    let offset = payload
        .get("offset")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()))
        .unwrap_or(0)
        .max(0) as usize;
    let length = payload
        .get("length")
        .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()))
        .map_or(cap, |n| n.max(0) as usize);
    if length > cap {
        return bad_arg(
            &format!(
                "pull: length {length} exceeds the {cap}-char page cap — a page must fit \
                 INLINE where it is going; ask for less and page"
            ),
            "length",
        );
    }
    let got = kernel.rows.body_get(pid, offset, length.max(1));
    if got.is_none() {
        // A fold id: its body is on the row itself.
        for r in load_history(agent_id, kernel, DEFAULT_CLIENT_ID).await {
            if r.get("kind").and_then(Value::as_str) == Some("fold") && row_id(&r) == Some(pid) {
                let text = r.get("body").and_then(Value::as_str).unwrap_or("");
                let chars: Vec<char> = text.chars().collect();
                let cut: String = chars.iter().take(length).collect();
                return json!({
                    "pid": pid,
                    "offset": 0,
                    "text": cut,
                    "next_offset": length.min(chars.len()),
                    "eof": length >= chars.len(),
                    "refs": r.get("refs").cloned().unwrap_or(json!([])),
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
            "content": serde_json::to_string(&out).unwrap_or_default()
        })) > cap_tok
    {
        let keep = (text.chars().count() as f64 * 0.8) as usize;
        text = text.chars().take(keep).collect();
        out = reply(&text, offset + text.chars().count(), false);
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
    let q = payload
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if q.is_empty() {
        return bad_arg("grep: `query` is required", "query");
    }
    let num = |k: &str, dflt: i64| -> i64 {
        payload
            .get(k)
            .and_then(|v| v.as_i64().or_else(|| v.as_str()?.trim().parse().ok()))
            .filter(|&n| n != 0)
            .unwrap_or(dflt)
    };
    let limit = num("limit", 100).clamp(1, 100) as usize;
    // The per-line cut. A bare default (200) the model can raise: a rule written
    // as one long line lost its tail at 200 and the model concluded grep could
    // not reach it (measured 2026-09-08, python fold_long).
    let width = num("width", 200).clamp(20, 20_000) as usize;

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
    // A fold id in `ids` is a DESCENT: it expands to the rows it collapsed.
    let scope: Option<HashSet<i64>> = ids
        .as_ref()
        .map(|v| expand_ids(&rows, v).into_iter().collect());

    let mut matches: Vec<Value> = Vec::new();
    let mut total = 0usize;
    let mut seen_lines: HashSet<String> = HashSet::new();
    // IDENTICAL LINES ONCE. A task the human repeats per delivery matched 28
    // times in one run and pushed the one body hit past the inline cap (measured
    // 2026-09-06, python recall_after_fold). The first occurrence keeps its
    // address.
    let mut scan = |rid: i64, text: &str, matches: &mut Vec<Value>, total: &mut usize| {
        for (n, line) in text.lines().enumerate() {
            if !line.to_lowercase().contains(&q) {
                continue;
            }
            let key = line.split_whitespace().collect::<Vec<_>>().join(" ");
            if !seen_lines.insert(key) {
                continue;
            }
            *total += 1;
            if matches.len() < limit {
                let cut: String = line.trim().chars().take(width).collect();
                matches.push(json!({"id": rid, "line": n + 1, "text": cut}));
            }
        }
    };

    // The bodies in reach: the pids on the reply tags of the rows considered —
    // every row of the conversation, or, under a scope, the rows the scope names
    // (a fold's members ARE reply rows, and their bodies hang off the pids in
    // their tags — a descent that ignored them found nothing, measured
    // 2026-09-08, python: 22 greps, 22 zeros). An id that is itself a pid counts.
    let mut pids: Vec<i64> = Vec::new();
    for m in &rows {
        if scope
            .as_ref()
            .is_some_and(|s| row_id(m).is_none_or(|id| !s.contains(&id)))
        {
            continue;
        }
        if m.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let text = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            other => serde_json::to_string(&other).unwrap_or_default(),
        };
        for block in reply_blocks(&text) {
            if let Some(p) = pid_of(block).and_then(|t| t.parse::<i64>().ok()) {
                pids.push(p);
            }
        }
    }
    if let Some(s) = &scope {
        let mut ordered: Vec<i64> = s.iter().copied().collect();
        ordered.sort_unstable();
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
        if scope.as_ref().is_some_and(|s| !s.contains(&rid)) {
            continue;
        }
        let text = match m.get("content") {
            Some(Value::String(s)) => s.clone(),
            other => serde_json::to_string(&other).unwrap_or_default(),
        };
        scan(rid, &text, &mut matches, &mut total);
    }
    let (kept, cut) = fit_reply(
        &matches,
        inline_cap(&agent_meta(agent_id, kernel)),
        Some(limit),
    );
    let more = cut || total > kept.len();
    let mut out = Map::new();
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
    out.insert("matches".to_string(), json!(kept));
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
    let dry = payload.get("dry").and_then(Value::as_bool).unwrap_or(false);
    if let (Some(provider), false) = (provider, dry) {
        let lines: Vec<String> = head
            .iter()
            .map(|&i| {
                let r = &view[i];
                let content = row_content(r);
                let title = r
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| quoted_title(content).map(str::to_string))
                    .unwrap_or_else(|| {
                        content
                            .chars()
                            .take(80)
                            .collect::<String>()
                            .replace('\n', " ")
                    });
                let kind = r
                    .get("kind")
                    .and_then(Value::as_str)
                    .filter(|k| !k.is_empty())
                    .unwrap_or_else(|| r.get("role").and_then(Value::as_str).unwrap_or(""));
                let parent = r
                    .get("parent")
                    .and_then(Value::as_i64)
                    .map_or("-".to_string(), |p| p.to_string());
                format!(
                    "{} · {kind} · \"{title}\" · {}B · ↑{parent}",
                    row_id(r).unwrap_or(0),
                    content.len()
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
        if let Some(ans) = first_json(&compose(provider, &prompt).await) {
            let head_ids: HashSet<i64> = head.iter().filter_map(|&i| row_id(&view[i])).collect();
            let want: Vec<i64> = ans
                .get("expand")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(as_id)
                        .filter(|x| head_ids.contains(x))
                        .collect()
                })
                .unwrap_or_default();
            why = ans
                .get("why")
                .and_then(Value::as_str)
                .map(|s| s.chars().take(300).collect::<String>())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "chosen".to_string());
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
            && r.get("parent").and_then(Value::as_i64).is_none()
            && r.get("kind")
                .and_then(Value::as_str)
                .is_none_or(|k| k.is_empty() || k == "user")
            && parse_reply_tag(row_content(r)).is_none()
    });
    let live_txt = live.map_or(String::new(), |l| {
        let text: String = row_content(l)
            .split_whitespace()
            .collect::<Vec<_>>()
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
