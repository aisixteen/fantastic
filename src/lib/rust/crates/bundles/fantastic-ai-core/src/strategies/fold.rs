//! `fold` — self-inflicted, mechanical compaction that leaves marks the model can walk.
//!
//! At the watermark, oldest first, one batch down to LOW:
//!   1. FOLD closed chains — one summarizing call per chain (title + body), one
//!      appended fold row each with `refs`; the members stay one descent away
//!   2. still over → STRIP bodies in open chains (the `strip` strategy, unchanged)
//!   3. still over → CUT (strip's own second stage)
//!
//! THE ONE STRATEGY THAT WRITES. Every other strategy is a pure projection; this
//! one appends fold rows to the durable store, because a fold is a mark that must
//! persist — the next turn has to see the same collapse, or the prefix rewrites
//! every turn. Bookkeeping from above, same path as the conversation itself.
//! Nothing is ever edited.
//!
//! If the summarizer fails, the fold is still written — refs plus the members'
//! joined titles — and says so on its own line. Degraded, loud, never silent.
//!
//! Mirrors python `ai_core/strategies/fold.py`.

use super::strip::{strip, LOW};
use super::{Projection, ProjectionCtx};
use crate::context::{estimate_tokens, NOTICE_ENVELOPE_RESERVE};
use crate::folds::{
    chain_text, is_fold, level, make_fold, quoted_title, render, row_id, row_parent, row_role,
};
use crate::history::append_history;
use crate::provider::{Provider, ProviderEvent};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

/// The longest a fold title may be, in chars.
pub const MAX_TITLE: usize = 140;

/// Rows at the end of the view that never fold — the live tail.
pub const KEEP_TAIL: usize = 6;

/// Rows per fold at the watermark — small enough to summarize, large enough to
/// matter.
pub const SPAN: usize = 8;

/// Adjacent folds at the head of the view before they fold as a group.
///
/// THREE, not four. A real run folds at most once per turn — each projection
/// drops the view under the watermark and stops — so the head accumulates folds
/// one at a time and this threshold is the whole story. At four it never fired:
/// measured 2026-09-11 (python), `nested_folds` and `flat_roots` both ended
/// `levels [1]`, a head that grows one fold per span forever, which is the
/// growth this constant exists to stop.
///
/// Three still skips-then-batches, so depth grows as log₃ of the history rather
/// than one level per cut.
pub const FOLD_RUN: usize = 3;

/// The joined `· "title"` marks of a span — what a degraded fold carries instead
/// of a summary.
fn titles_of(rows: &[Value], chain: &[usize]) -> String {
    let mut out: Vec<&str> = Vec::new();
    for &i in chain {
        if let Some(t) = quoted_title(rows[i].get("content").and_then(Value::as_str).unwrap_or(""))
        {
            if !out.contains(&t) {
                out.push(t);
            }
        }
    }
    if out.is_empty() {
        "(untitled)".to_string()
    } else {
        out.join("; ")
    }
}

fn cut_title(t: &str) -> String {
    t.chars().take(MAX_TITLE).collect()
}

/// One summarizer call for one closed chain — `(title, body, degraded)`.
async fn summarize(
    provider: &Arc<dyn Provider>,
    rows: &[Value],
    chain: &[usize],
) -> (String, String, bool) {
    let degraded = || {
        let t = titles_of(rows, chain);
        (cut_title(&t), t, true)
    };
    let ask = vec![json!({
        "role": "user",
        "content": format!(
            "FOLD this finished piece of work. First line: a one-sentence title (what it \
             was and how it ended). Then a blank line. Then 2-5 sentences: what was asked, \
             what was found or decided, anything that still binds. Output only that.\n\n{}",
            chain_text(rows, chain)
        ),
    })];
    let Ok(stream) = provider.chat(&ask).await else {
        return degraded();
    };
    let mut parts: Vec<String> = Vec::new();
    let mut stream = stream;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ProviderEvent::Token(t)) => parts.push(t),
            Err(_) => return degraded(),
        }
    }
    let text = parts.join("");
    let text = text.trim();
    if text.is_empty() {
        return degraded();
    }
    // The title is the first NON-EMPTY line; a leading blank line or a stray
    // quote must not become the title. Reasoning is already stripped by the
    // summarizer; a summary that still opens a think block is not a summary —
    // degrade to titles.
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() || lines[0].starts_with("<think") {
        return degraded();
    }
    let title = lines[0].trim_matches('"').trim_matches(['*', '#']).trim();
    let body = lines[1..].join("\n");
    let body = body.trim();
    let title = cut_title(title);
    let title = if title.is_empty() {
        "(untitled)".to_string()
    } else {
        title
    };
    let body = if body.is_empty() {
        title.clone()
    } else {
        body.to_string()
    };
    (title, body, false)
}

/// Summarize `idxs` and append ONE fold row for them. A fold row in `idxs` is
/// just a member — that is how folds nest: the next fold over this stretch of
/// the view takes the earlier fold as one of its rows, and its level is one
/// above the deepest member.
pub async fn write_fold(ctx: &ProjectionCtx<'_>, rows: &[Value], idxs: &[usize]) -> Value {
    let ids: Vec<i64> = idxs.iter().filter_map(|&i| row_id(&rows[i])).collect();
    let lvl = 1 + idxs
        .iter()
        .filter(|&&i| is_fold(&rows[i]))
        .map(|&i| level(rows, &rows[i]))
        .max()
        .unwrap_or(0);
    let (title, text, degraded) = summarize(ctx.provider, rows, idxs).await;
    let id = ctx.kernel.rows.mint_id();
    let parent = idxs.first().and_then(|&i| row_parent(&rows[i]));
    let fr = make_fold(&ids, &title, &text, degraded, id, lvl, parent);
    append_history(
        ctx.self_id,
        ctx.kernel,
        ctx.client_id,
        std::slice::from_ref(&fr),
    )
    .await;
    fr
}

/// The oldest foldable stretch of the VIEW, as indices into `rows`.
///
/// Never the goal turn, never the last `keep_tail` rows, and never ending
/// between a call and its answer. Folds are ordinary members, but NOT the first
/// pick: a stretch that starts on the previous fold wraps it, and doing that
/// every time builds a linked list of summaries-of-summaries (measured
/// 2026-09-06, python: twelve levels deep in 26 cuts, the model lost in it for
/// 50 turns). So: skip the folds at the head and take the oldest NON-fold rows;
/// only when `FOLD_RUN` folds sit adjacent at the head do they become the span
/// themselves. Depth then grows with the log of the history, not its length.
#[must_use]
pub fn next_span(rows: &[Value], view: &[Value], keep_tail: usize, size: usize) -> Vec<usize> {
    let by_id: HashMap<i64, usize> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, r)| row_id(r).map(|id| (id, i)))
        .collect();
    let head: Vec<&Value> = if view.len() > keep_tail + 1 {
        view[1..view.len() - keep_tail]
            .iter()
            .filter(|r| row_id(r).is_some_and(|id| by_id.contains_key(&id)))
            .collect()
    } else {
        Vec::new()
    };
    let mut lead = 0;
    while lead < head.len() && is_fold(head[lead]) {
        lead += 1;
    }
    let take: Vec<&Value> = if lead >= FOLD_RUN {
        head[..lead].to_vec() // fold the folds — same operation, same summarizer
    } else {
        let end = (lead + size).min(head.len());
        let mut t = head[lead..end].to_vec();
        while t.last().is_some_and(|r| row_role(r) == "assistant") {
            t.pop();
        }
        t
    };
    if take.len() < 2 {
        return Vec::new();
    }
    take.iter()
        .filter_map(|r| row_id(r).and_then(|id| by_id.get(&id)).copied())
        .collect()
}

/// Fold the oldest stretches of the view down to the low watermark, tidy the
/// head, then hand what is left to `strip`.
pub async fn fold(body: &[Value], ctx: &ProjectionCtx<'_>) -> Projection {
    let mut rows: Vec<Value> = body.to_vec();
    let mut view = render(&rows);
    let budget = ctx.budget - NOTICE_ENVELOPE_RESERVE;
    let mut folded = 0usize;
    if estimate_tokens(&view) > budget && rows.len() > 2 {
        let target = (budget as f64 * LOW) as i64;
        // ONE MOVE, REPEATED: fold the oldest stretch of the view — rows, folds,
        // or a mix — until under the low watermark or nothing foldable is left.
        while estimate_tokens(&view) > target {
            let span = next_span(&rows, &view, KEEP_TAIL, SPAN);
            if span.is_empty() {
                break;
            }
            let fr = write_fold(ctx, &rows, &span).await;
            rows.push(fr);
            folded += 1;
            view = render(&rows);
        }
    }
    // TIDY THE HEAD, budget or no budget. Coalescing used to happen only while
    // the view was OVER the watermark, so a projection that dropped under it left
    // its adjacent folds standing — and they stayed standing, because the next
    // projection folds ordinary rows first and only revisits the head when it is
    // over budget with `FOLD_RUN` folds already there. A session therefore grew
    // one fold per span at the head, forever.
    //
    // Terminates: each pass replaces `FOLD_RUN` adjacent folds with ONE, so the
    // run shrinks by `FOLD_RUN - 1` every time and `next_span` stops selecting
    // once fewer than `FOLD_RUN` remain.
    loop {
        let span = next_span(&rows, &view, KEEP_TAIL, SPAN);
        if span.is_empty() || !span.iter().all(|&i| is_fold(&rows[i])) {
            break;
        }
        let fr = write_fold(ctx, &rows, &span).await;
        rows.push(fr);
        folded += 1;
        view = render(&rows);
    }
    // Stage two and three: strip bodies in what is left, then cut — strip's own
    // logic.
    let out = strip(&view, ctx).await;
    Projection {
        body: out.body,
        summary: None,
        omitted_marker: out.omitted_marker,
        stripped: out.stripped,
        stripped_bytes: out.stripped_bytes,
        folded,
    }
}
