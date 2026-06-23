//! The Context-Protocol seam: shape the assembled messages to fit the
//! agent's token budget via its configured `context_strategy`, prepend the
//! ONE canonical `[context-notice]`, and push a `context` event to the
//! caller. The `too_small` failsafe is a failfast (model NOT called) that
//! also pushes a `context:too_small` event — NOT a fallback. The durable
//! store is untouched (the notice lives only in the model view). Mirrors the
//! Python `ai_core/core.py` `_project_context` / `_context_notice`.

use crate::context::{
    budget as agent_budget, estimate_one, estimate_tokens, recent_n as cfg_recent_n,
    resolve_context_window, NOTICE_ENVELOPE_RESERVE,
};
use crate::events::{to_caller, CallerRoute};
use crate::helpers::{agent_meta, now_secs};
use crate::history::load_history;
use crate::provider::Provider;
use crate::state::BackendState;
use crate::strategies::{compact, drop_orphan_tools, is_known_strategy, truncate};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// The ONE canonical inbound context-notice — composed at the SEAM from a
/// strategy's projection artifact. A `role:user` turn (the role every backend
/// reliably attends to). Carries the protocol affordances: `recall` to page
/// dropped turns back, and persist-to-memory. Model view ONLY — never the store.
pub fn context_notice(
    strategy: &str,
    summary: Option<&str>,
    omitted_marker: bool,
    dropped_n: usize,
) -> Value {
    let mut lines = vec![format!(
        "[context-notice] Your conversation exceeded the window and was compacted \
         (strategy={strategy}, {dropped_n} earlier turn(s) dropped from THIS view)."
    )];
    if let Some(s) = summary {
        lines.push(format!("Summary of the dropped span:\n{s}"));
    } else if omitted_marker {
        lines.push("An earlier span was omitted in place.".to_string());
    }
    lines.push(
        "The full transcript is intact in durable storage. To page dropped turns back, \
         send {type:'recall', query?, limit?} to your OWN id. If the dropped span holds \
         durable facts (names, decisions, preferences), persist them to your memory agent \
         now via the send tool — the earlier turns are leaving your live view."
            .to_string(),
    );
    json!({"role": "user", "content": lines.join("\n")})
}

/// Shape `messages` to fit the budget, prepend the notice, emit the `context`
/// event. Returns `Ok(projected_model_messages)`, or `Err(error_value)` for
/// the `too_small` failsafe / an unknown-strategy config error. Sets the
/// public `state.projection` summary + the private `state.compaction_mark`.
#[allow(clippy::too_many_arguments)]
pub async fn project_context(
    provider: &Arc<dyn Provider>,
    self_id: &AgentId,
    state: &Arc<BackendState>,
    kernel: &Arc<Kernel>,
    client_id: &str,
    route: CallerRoute,
    name: &str,
    messages: Vec<Value>,
) -> Result<Vec<Value>, Value> {
    let meta = agent_meta(self_id, kernel);
    let b = agent_budget(&meta);
    if estimate_tokens(&messages) <= b {
        *state.projection.lock().expect("projection poisoned") = Some(json!({"fired": false}));
        return Ok(messages);
    }
    let system_block: Vec<Value> = messages.iter().take(1).cloned().collect();
    let body: Vec<Value> = messages.iter().skip(1).cloned().collect();
    if body.is_empty() {
        *state.projection.lock().expect("projection poisoned") = Some(json!({"fired": false}));
        return Ok(messages);
    }
    let sys_tokens = estimate_tokens(&system_block);
    let body_budget = b - sys_tokens;
    let last_cost = estimate_one(&body[body.len() - 1]);
    // The live user turn AND the notice envelope are both non-negotiable — if
    // there's no room for BOTH, fail loud (so the trim below never drops the live turn).
    if body_budget < last_cost + NOTICE_ENVELOPE_RESERVE {
        let window = resolve_context_window(&meta);
        let hint = format!(
            "the system prompt ({sys_tokens} tok) leaves no room in the {window}-token \
             window for even one turn; reduce agents/menu or raise context_window"
        );
        *state.projection.lock().expect("projection poisoned") =
            Some(json!({"fired": false, "too_small": true}));
        *state.compaction_mark.lock().expect("mark poisoned") = None;
        to_caller(
            kernel,
            self_id,
            client_id,
            route,
            json!({
                "type": "context", "source": self_id.as_str(), "ts": now_secs(),
                "phase": "too_small",
                "detail": {"context_window": window, "system_tokens": sys_tokens, "hint": hint},
            }),
        )
        .await;
        return Err(json!({"error": format!("{name}: context_insufficient — {hint}")}));
    }
    let strat_name = meta
        .get("context_strategy")
        .and_then(Value::as_str)
        .unwrap_or("compact")
        .to_string();
    if !is_known_strategy(&strat_name) {
        return Err(json!({
            "error": format!("{name}: unknown context_strategy {strat_name:?} (valid: compact, truncate)"),
        }));
    }
    let recent = cfg_recent_n(&meta);
    let proj = if strat_name == "truncate" {
        truncate(&body, body_budget)
    } else {
        compact(&body, recent, body_budget, provider).await
    };
    let dropped_pre = body.len().saturating_sub(proj.body.len());
    let notice = context_notice(
        &strat_name,
        proj.summary.as_deref(),
        proj.omitted_marker,
        dropped_pre,
    );
    // Single budget authority: the strategy reserved the envelope, but guard the
    // final fit anyway — trim oldest body turns (tool-pairing-safe). Never drop the
    // last (live) turn: the failsafe above guarantees room for [notice + live turn].
    let mut out_body = proj.body;
    loop {
        if out_body.len() <= 1 {
            break;
        }
        let mut probe = system_block.clone();
        probe.push(notice.clone());
        probe.extend(out_body.iter().cloned());
        if estimate_tokens(&probe) <= b {
            break;
        }
        out_body = drop_orphan_tools(out_body[1..].to_vec());
    }
    let dropped_n = body.len().saturating_sub(out_body.len());
    let summarized = proj.summary.is_some();
    *state.projection.lock().expect("projection poisoned") = Some(json!({
        "fired": true,
        "strategy": strat_name,
        "kept_turns": out_body.len(),
        "dropped_turns": dropped_n,
        "summarized": summarized,
    }));
    *state.compaction_mark.lock().expect("mark poisoned") =
        Some((body.len().saturating_sub(1), client_id.to_string()));
    to_caller(
        kernel,
        self_id,
        client_id,
        route,
        json!({
            "type": "context", "source": self_id.as_str(), "ts": now_secs(),
            "phase": "compacted",
            "detail": {
                "strategy": strat_name, "dropped_turns": dropped_n,
                "kept_turns": out_body.len(), "summarized": summarized,
            },
        }),
    )
    .await;
    let mut out = system_block;
    out.push(notice);
    out.extend(out_body);
    Ok(out)
}

/// Read-model over the durable transcript: AFTER the last compaction notice
/// (its cursor), did the model react? Scans the same client's thread for
/// `send` tool-calls — a `recall` to its OWN id, or a memory write
/// (`set`/`append`/`replace`). The reaction IS the 'ack'; this derives it.
/// `None` if no compaction has fired.
pub async fn derive_reaction(
    self_id: &AgentId,
    state: &Arc<BackendState>,
    kernel: &Arc<Kernel>,
) -> Option<Value> {
    let fired = state
        .projection
        .lock()
        .expect("projection poisoned")
        .as_ref()
        .and_then(|p| p.get("fired").and_then(Value::as_bool))
        .unwrap_or(false);
    let mark = state.compaction_mark.lock().expect("mark poisoned").clone();
    let (idx, client_id) = match (fired, mark) {
        (true, Some(m)) => m,
        _ => return None,
    };
    let store = load_history(self_id, kernel, &client_id).await;
    let mut recalled = false;
    let mut persisted = false;
    let mut recall_count: u64 = 0;
    for m in store.iter().skip(idx) {
        if m.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Tool calls now live as `<tool_call>` text inside the assistant content —
        // extract them with the same shared parser the loop uses.
        let content = m.get("content").and_then(Value::as_str).unwrap_or("");
        for (_name, args) in crate::tool_parse::extract_tool_calls(content) {
            let target = args.get("target_id").and_then(Value::as_str);
            let ptype = args
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(Value::as_str);
            if target == Some(self_id.as_str()) && ptype == Some("recall") {
                recalled = true;
                recall_count += 1;
            } else if matches!(ptype, Some("set") | Some("append") | Some("replace")) {
                persisted = true;
            }
        }
    }
    let mut out = Map::new();
    out.insert("recalled".to_string(), json!(recalled));
    out.insert("persisted".to_string(), json!(persisted));
    out.insert("recall_count".to_string(), json!(recall_count));
    Some(Value::Object(out))
}
