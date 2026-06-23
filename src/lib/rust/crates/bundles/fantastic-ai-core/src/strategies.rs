//! Context-overflow strategies (the projection logic). A strategy maps the
//! conversation BODY (history + live user turn, WITHOUT the system block)
//! to a [`Projection`]: a shortened body that fits the budget + a structured
//! artifact (a summary, or an omitted-span flag). It does NOT fabricate a
//! notice turn — the ONE canonical `[context-notice]` is composed at the
//! seam ([`crate::projection`]). The durable store is NEVER touched.
//!
//! Tool-pairing is load-bearing: the OpenAI/NIM wire rejects a `role:tool`
//! that isn't preceded by its `assistant.tool_calls` turn. Every cut drops
//! orphaned leading `role:tool` messages so the body stays wire-valid.
//!
//! Selection is STATIC per agent (`context_strategy` meta, default
//! `compact`); there is no runtime try-X-else-Y (NO-FALLBACKS). `memgpt`
//! was removed — its persist nudge is now universal in the seam notice.

use crate::context::{estimate_one, NOTICE_ENVELOPE_RESERVE};
use crate::provider::{Provider, ProviderEvent};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;

/// The stub used when a summarizer is unavailable / fails — a degraded
/// artifact (the full transcript is whole in the durable store), NOT a
/// fallback chain.
pub const STUB_SUMMARY: &str = "[Earlier conversation omitted — summary unavailable]";

/// What a strategy returns: the projected body + the artifact the seam needs
/// to compose the canonical context-notice. NEVER a fabricated user turn.
#[derive(Clone, Debug, Default)]
pub struct Projection {
    /// Projected conversation body — NO notice turn, NO system block.
    pub body: Vec<Value>,
    /// compact: the LLM summary text; truncate/none: `None`.
    pub summary: Option<String>,
    /// truncate: an earlier span was elided in place.
    pub omitted_marker: bool,
}

fn is_tool(turn: &Value) -> bool {
    turn.get("role").and_then(Value::as_str) == Some("tool")
}

/// Drop leading `role:tool` messages whose owning `assistant.tool_calls`
/// turn is not present — the model wire would reject them.
pub fn drop_orphan_tools(turns: Vec<Value>) -> Vec<Value> {
    let mut i = 0;
    while i < turns.len() && is_tool(&turns[i]) {
        i += 1;
    }
    turns[i..].to_vec()
}

/// Keep the largest SUFFIX of `turns` that fits `budget`, always including
/// the last turn (the live request), then drop orphaned leading tool replies.
pub fn fit_tail(turns: &[Value], budget: i64) -> Vec<Value> {
    if turns.is_empty() {
        return Vec::new();
    }
    let last = turns.len() - 1;
    let mut kept = vec![turns[last].clone()];
    let mut used = estimate_one(&turns[last]);
    for t in turns[..last].iter().rev() {
        let c = estimate_one(t);
        if used + c > budget {
            break;
        }
        kept.insert(0, t.clone());
        used += c;
    }
    drop_orphan_tools(kept)
}

/// Split `body` into (overflow, recent) at the last `recent_n` turns,
/// snapping the boundary back so `recent` never STARTS on an orphan tool.
pub fn recent_split(body: &[Value], recent_n: usize) -> (Vec<Value>, Vec<Value>) {
    let mut start = body.len().saturating_sub(recent_n);
    while start > 0 && is_tool(&body[start]) {
        start -= 1;
    }
    (body[..start].to_vec(), body[start..].to_vec())
}

/// Summarize a span of turns via the backend provider (tool-free completion);
/// degrade to a stub on ANY failure. The full transcript stays whole in the
/// durable store, so a degraded summary is acceptable (not a fallback chain).
pub async fn safe_summary(provider: &Arc<dyn Provider>, overflow: &[Value]) -> String {
    let rendered: String = overflow
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
            let content = m
                .get("content")
                .map(|c| match c.as_str() {
                    Some(s) => s.to_string(),
                    None => serde_json::to_string(c).unwrap_or_default(),
                })
                .unwrap_or_default();
            format!("{role}: {content}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let rendered: String = rendered.chars().take(20000).collect();
    let prompt = vec![
        json!({"role": "system", "content":
            "Summarize the conversation excerpt below concisely, PRESERVING names, \
             decisions, facts, preferences, and unresolved tasks. Output ONLY the summary."}),
        json!({"role": "user", "content": rendered}),
    ];
    let stream = match provider.chat(&prompt).await {
        Ok(s) => s,
        Err(_) => return STUB_SUMMARY.to_string(),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut stream = stream;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProviderEvent::Token(t)) => parts.push(t),
            Ok(ProviderEvent::ToolCall { .. }) => {}
            Err(_) => return STUB_SUMMARY.to_string(),
        }
    }
    let s = parts.join("");
    let s = s.trim();
    if s.is_empty() {
        STUB_SUMMARY.to_string()
    } else {
        s.to_string()
    }
}

/// `compact` (DEFAULT): keep the recent turns verbatim + an LLM summary of
/// the overflow. The summary rides the artifact; the seam wraps it.
pub async fn compact(
    body: &[Value],
    recent: usize,
    budget: i64,
    provider: &Arc<dyn Provider>,
) -> Projection {
    let (overflow, recent_turns) = recent_split(body, recent);
    if overflow.is_empty() {
        return Projection {
            body: fit_tail(body, budget),
            summary: None,
            omitted_marker: false,
        };
    }
    let summary = safe_summary(provider, &overflow).await;
    // Reserve room for the notice the seam prepends: the wrapper envelope +
    // the summary text itself (the seam embeds this exact summary).
    let summary_cost = estimate_one(&json!({"role": "user", "content": summary}));
    let avail = (budget - NOTICE_ENVELOPE_RESERVE - summary_cost).max(0);
    Projection {
        body: fit_tail(&recent_turns, avail),
        summary: Some(summary),
        omitted_marker: false,
    }
}

/// `truncate`: keep the first (task-framing) turn + the recent turns, drop the
/// middle. NO summarizer. The elision is reported via `omitted_marker`.
pub fn truncate(body: &[Value], budget: i64) -> Projection {
    if body.len() <= 1 {
        return Projection {
            body: fit_tail(body, budget),
            summary: None,
            omitted_marker: false,
        };
    }
    let first = body[0].clone();
    // Reserve room for the notice the seam prepends (no summary in truncate).
    let head_cost = estimate_one(&first) + NOTICE_ENVELOPE_RESERVE;
    let tail = fit_tail(&body[1..], (budget - head_cost).max(0));
    let mut out = vec![first];
    out.extend(tail);
    Projection {
        body: out,
        summary: None,
        omitted_marker: true,
    }
}

/// The known strategy names (config validity check; unknown ⇒ caller errors).
pub fn is_known_strategy(name: &str) -> bool {
    matches!(name, "compact" | "truncate")
}
