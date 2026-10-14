//! Context-overflow strategies (the projection logic). A strategy maps the
//! conversation BODY (history + live user turn, WITHOUT the system block)
//! to a [`Projection`]: a shortened body that fits the budget + a structured
//! artifact (a summary, or an omitted-span flag). It does NOT fabricate a
//! notice turn — the ONE canonical `[context-notice]` is composed at the
//! seam ([`crate::projection`]). The durable store is NEVER touched.
//!
//! Selection is STATIC per agent (`context_strategy` meta, default
//! `compact`); there is no runtime try-X-else-Y (NO-FALLBACKS). `memgpt`
//! was removed — its persist nudge is now universal in the seam notice.

pub mod fold;
pub mod strip;

use crate::constants::NOTICE_ENVELOPE_RESERVE;
use crate::context::estimate_one;
use crate::provider::{Provider, ProviderEvent};
use fantastic_bundle::{py_str, py_strip};
use fantastic_kernel::{AgentId, Kernel};
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
    /// `strip`: reply blocks reduced to their stub this projection.
    pub stripped: usize,
    /// `strip`: bytes that left the view with those stubs.
    pub stripped_bytes: i64,
    /// `fold`: fold rows written this projection.
    pub folded: usize,
}

/// Everything a strategy needs, WITHOUT importing the seam. `budget` is the
/// budget for the conversation BODY (the window budget minus the already
/// measured system block) — strategies fit the body to it.
///
/// Mirrors python `ai_core.context.ProjectionCtx`. `client_id` is here for the
/// same reason python carries it: `fold` is the one strategy that WRITES, and a
/// fold row is a mark that must persist.
pub struct ProjectionCtx<'a> {
    /// Token budget for the conversation BODY.
    pub budget: i64,
    /// Verbatim recent turns `compact`/`truncate` keep.
    pub recent_n: usize,
    /// The backend provider, for the strategies that summarize.
    pub provider: &'a Arc<dyn Provider>,
    /// The agent this projection is for.
    pub self_id: &'a AgentId,
    /// The kernel — the row store `fold` writes its marks to.
    pub kernel: &'a Arc<Kernel>,
    /// The conversation this projection is for.
    pub client_id: &'a str,
}

/// Keep the largest SUFFIX of `turns` that fits `budget`, always including
/// the last turn (the live request).
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
    kept
}

/// Split `body` into (overflow, recent) at the last `recent_n` turns.
pub fn recent_split(body: &[Value], recent_n: usize) -> (Vec<Value>, Vec<Value>) {
    let start = body.len().saturating_sub(recent_n);
    (body[..start].to_vec(), body[start..].to_vec())
}

/// THE ONE SUMMARIZER — python `_make_summarizer`.
///
/// A plain text completion (no tools; providers stream raw text) over the span
/// rendered as `role: content` lines, wrapped in the summarizer system block.
/// Every side call in this crate goes through it, `fold` included.
///
/// `max_tokens` bounds the output (a fold summary's budget); the second value
/// says whether the provider stopped AT that bound (`stop_reason ==
/// "max_tokens"`), so a cut-off summary is never kept as if it were whole.
///
/// NO INPUT CUT. A REASONING MODEL THINKS OUT LOUD HERE TOO: unstripped,
/// model-written folds carry titles beginning `<think>The user wants me to
/// summarize…`. Side calls strip like the main turn.
pub async fn summarize(
    provider: &Arc<dyn Provider>,
    msgs: &[Value],
    max_tokens: Option<i64>,
) -> (String, bool) {
    let rendered: String = msgs
        .iter()
        .map(|m| {
            let role = match m.get("role") {
                Some(r) => py_str(r),
                None => "?".to_string(),
            };
            let content = m.get("content").map_or_else(String::new, py_str);
            format!("{role}: {content}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = vec![
        json!({"role": "system", "content":
            "Summarize the conversation excerpt below concisely, PRESERVING names, \
             decisions, facts, preferences, and unresolved tasks. Output ONLY the summary."}),
        json!({"role": "user", "content": rendered}),
    ];
    provider.clear_stop_reason();
    let Ok(mut stream) = provider.chat(&prompt, max_tokens).await else {
        return (String::new(), false);
    };
    let mut parts: Vec<String> = Vec::new();
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProviderEvent::Token(t)) => parts.push(t),
            Err(_) => return (String::new(), false),
        }
    }
    let cut = provider.report().stop_reason.as_deref() == Some("max_tokens");
    (crate::think::without_thinking(&parts.join("")), cut)
}

/// Summarize the overflow via [`summarize`]; degrade to a stub on ANY failure (a
/// degraded artifact — the full transcript is whole in the durable store, so this
/// is not a fallback chain). Python `safe_summary`.
pub async fn safe_summary(provider: &Arc<dyn Provider>, overflow: &[Value]) -> String {
    let (s, _cut) = summarize(provider, overflow, None).await;
    let s = py_strip(&s);
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
            stripped: 0,
            stripped_bytes: 0,
            folded: 0,
        };
    }
    let summary = safe_summary(provider, &overflow).await;
    // Reserve room for the notice the seam prepends: the wrapper envelope +
    // the summary text itself (the seam embeds this exact summary).
    let summary_cost = estimate_one(&json!({"role": "user", "content": summary}));
    let avail = (budget - NOTICE_ENVELOPE_RESERVE - summary_cost).max(0);
    // PIN THE GOAL. The first turn is the task framing, and it is the OLDEST — so it is
    // the first thing the overflow split takes. A summary is a lossy retelling; the one
    // turn that must survive verbatim is the instruction the whole run serves.
    // Without the pin the agent reaches the correct conclusion, then ends "healthy
    // budget, no tasks" — it has not refused to act, it no longer knows what it is
    // doing. `truncate` pins this turn too. Cost is one turn of budget, charged
    // before the tail.
    let Some(first) = body.first() else {
        return Projection {
            body: fit_tail(&recent_turns, avail),
            summary: Some(summary),
            omitted_marker: false,
            stripped: 0,
            stripped_bytes: 0,
            folded: 0,
        };
    };
    let head_cost = estimate_one(first);
    let kept = fit_tail(&recent_turns, (avail - head_cost).max(0));
    // Only re-add it if the recent window does not already carry it (short bodies).
    let body = if kept.first() == Some(first) {
        kept
    } else {
        let mut out = vec![first.clone()];
        out.extend(kept);
        out
    };
    Projection {
        body,
        summary: Some(summary),
        omitted_marker: false,
        stripped: 0,
        stripped_bytes: 0,
        folded: 0,
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
            stripped: 0,
            stripped_bytes: 0,
            folded: 0,
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
        stripped: 0,
        stripped_bytes: 0,
        folded: 0,
    }
}

/// The strategy an agent gets when its record names none — python
/// `ai_core.strategies.DEFAULT_STRATEGY`. It applies ONLY to an ABSENT key: a
/// `context_strategy` written on the record is read raw, and an unknown value
/// (including `null`) is a config refusal, never a silent fall-through here.
pub const DEFAULT_STRATEGY: &str = "compact";

/// The registry's names in insertion order — python `strategy_names()`, whose
/// `str(list)` is what the unknown-strategy refusal prints.
pub const STRATEGY_NAMES: [&str; 4] = ["compact", "truncate", "strip", "fold"];

/// The known strategy names (config validity check; unknown ⇒ caller errors).
pub fn is_known_strategy(name: &str) -> bool {
    STRATEGY_NAMES.contains(&name)
}
