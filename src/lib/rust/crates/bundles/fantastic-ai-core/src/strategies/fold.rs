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
//! A summary that stays OVER ITS BUDGET (or is CUT OFF by it) after one
//! re-summarise folds nothing: the span stays in view, counted as refused.
//!
//! Mirrors python `ai_core/strategies/fold.py`.

use super::strip::{strip, LOW};
use super::{Projection, ProjectionCtx};
use crate::constants::NOTICE_ENVELOPE_RESERVE;
use crate::context::{estimate_one, estimate_tokens};
use crate::folds::{
    chain_text, is_fold, level, make_fold, quoted_title, render, row_id, row_parent, row_role,
};
use crate::memory::{save_history, MemoryStoreError};
use crate::provider::Provider;
use fantastic_bundle::{py_split_whitespace, py_splitlines, py_str, py_strip, py_truthy};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

// The tunables live in [`crate::constants`]; re-exported so every
// `crate::strategies::fold::…` path keeps working.
pub use crate::constants::{
    FOLD_RUN, INDEX_USER_CHARS, KEEP_TAIL, MAX_TITLE, SPAN, SUMMARY_FLOOR, SUMMARY_SHARE,
    THINK_ALLOWANCE, WINDOW_SHARE,
};

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

/// The fold ask — python `FOLD_PROMPT`, `{budget}` filled in.
pub const FOLD_PROMPT: &str =
    "FOLD this finished piece of work in AT MOST ~{budget} tokens. First line: a \
one-sentence title (what it was and how it ended). Then a blank line. Then these \
sections, each one line or a few, in this order, omitting none (write `none`):\n\
ASKED: what was asked.\n\
DONE: what was found or decided — AND WHY.\n\
BINDING: rules, constraints and choices that still bind.\n\
ERRORS: every error, quoted verbatim.\n\
OPEN: what is unfinished.\n\
If the piece contains earlier fold summaries, MERGE them — on a conflict the newer \
statement wins, and unfinished work is carried forward. Output only that.\n\n";

/// The one re-summarise for an OVERSIZED summary — python `RESUMMARIZE`.
pub const RESUMMARIZE: &str =
    "Your summary below is ~{got} tokens; the budget is {budget}. Rewrite it to fit — \
keep the title line, the reasons, the binding rules and every quoted error; drop \
narration. Output only the summary.\n\n";

/// The one re-summarise for a CUT summary — python `RESUMMARIZE_CUT`.
pub const RESUMMARIZE_CUT: &str =
    "Your summary below was CUT OFF at the budget ({budget} tokens) before it ended. \
Write it again, shorter, so it ENDS within the budget — keep the title line, the \
reasons, the binding rules and every quoted error; drop narration. Output only \
the summary.\n\n";

/// The runtime's own lines — never "what the user said".
const RUNTIME: [&str; 8] = [
    "[reply to ",
    "[no call",
    "[context-notice",
    "[fold ",
    "[recall",
    "[late reply",
    "[cleared ",
    "[fork of ",
];

fn path_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(
            r"[\w./-]+\.(?:txt|md|py|json|ya?ml|js|ts|rs|swift|toml|csv|log|sh|html|css)\b",
        )
        .expect("path pattern")
    })
}

fn ident_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"\b[0-9a-f]{8,}\b").expect("ident pattern"))
}

/// What a fold must never lose, extracted WITHOUT the model: the files and the
/// identifiers its members name, and what the USER said, verbatim (newest kept
/// first when over the cap). Appended to the summary — no summariser can drop it.
/// Python `mechanical_index`.
#[must_use]
pub fn mechanical_index(rows: &[Value], chain: &[usize]) -> String {
    let content = |r: &Value| r.get("content").map_or_else(String::new, py_str);
    let text = chain
        .iter()
        .map(|&i| content(&rows[i]))
        .collect::<Vec<_>>()
        .join("\n");
    let files: Vec<String> = path_re()
        .find_iter(&text)
        .map(|m| m.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(30)
        .collect();
    let idents: Vec<String> = ident_re()
        .find_iter(&text)
        .map(|m| m.as_str().to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(20)
        .collect();
    let mut said: Vec<String> = Vec::new();
    let mut used = 0usize;
    for &i in chain.iter().rev() {
        let r = &rows[i];
        let c = content(r);
        let c = py_strip(&c);
        let user = r.get("role").and_then(Value::as_str) == Some("user");
        let kinded = r.get("kind").is_some_and(py_truthy);
        if !user || kinded || c.is_empty() || RUNTIME.iter().any(|p| c.starts_with(p)) {
            continue;
        }
        let mut c = py_split_whitespace(c).join(" ");
        if used + c.chars().count() > INDEX_USER_CHARS {
            let room = INDEX_USER_CHARS.saturating_sub(used);
            c = format!("{} …", c.chars().take(room).collect::<String>());
        }
        used += c.chars().count();
        said.insert(0, c);
        if used >= INDEX_USER_CHARS {
            break;
        }
    }
    let mut lines: Vec<String> = Vec::new();
    if !files.is_empty() {
        lines.push(format!("files: {}", files.join(", ")));
    }
    if !idents.is_empty() {
        lines.push(format!("identifiers: {}", idents.join(", ")));
    }
    lines.extend(said.into_iter().map(|u| format!("user said: {u}")));
    if lines.is_empty() {
        String::new()
    } else {
        format!("INDEX (mechanical):\n{}", lines.join("\n"))
    }
}

/// The summary budget for a span — python `summary_budget`.
#[must_use]
pub fn summary_budget(ctx_budget: i64, rows: &[Value], chain: &[usize]) -> i64 {
    let span: Vec<Value> = chain.iter().map(|&i| rows[i].clone()).collect();
    let span = estimate_tokens(&span);
    SUMMARY_FLOOR
        .max(((span as f64 * SUMMARY_SHARE) as i64).min((ctx_budget as f64 * WINDOW_SHARE) as i64))
}

/// `(title, body)` off a summary's text — `None` when it is no summary (empty, or
/// still opening a think block). Python `_split`.
fn split(text: &str) -> Option<(String, String)> {
    let lines: Vec<&str> = py_splitlines(text)
        .into_iter()
        .map(py_strip)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() || lines[0].starts_with("<think") {
        return None;
    }
    let title = py_strip(lines[0].trim_matches('"').trim_matches(['*', '#']));
    let joined = lines[1..].join("\n");
    let body = py_strip(&joined);
    let cut = cut_title(title);
    Some((
        if cut.is_empty() {
            "(untitled)".to_string()
        } else {
            cut
        },
        // `body or title` — THE UNCUT TITLE.
        if body.is_empty() {
            title.to_string()
        } else {
            body.to_string()
        },
    ))
}

/// `(title, body, degraded)`, or `None` when the summary stays OVER ITS BUDGET (or
/// is CUT OFF by it) after one re-summarise — then nothing is folded (the span
/// stays in view; loud in `context_status`), never a truncated summary.
///
/// The budget is stated in the prompt; the call's `max_tokens` is the budget plus
/// [`THINK_ALLOWANCE`] (a reasoning model's thought counts against it);
/// `reserve` is what the fold row spends on its mechanical index, so summary +
/// index fit. A provider that stopped at `max_tokens` says so — a cut-off summary
/// is treated like an oversized one, never kept. Python `_summarize`.
pub async fn summarize(
    provider: &Arc<dyn Provider>,
    ctx_budget: i64,
    rows: &[Value],
    chain: &[usize],
    reserve: i64,
) -> Option<(String, String, bool)> {
    let degraded = || {
        let t = titles_of(rows, chain);
        Some((cut_title(&t), t, true))
    };
    let budget = SUMMARY_FLOOR.max(summary_budget(ctx_budget, rows, chain) - reserve);
    let ask = |content: String| async move {
        let (text, cut) = crate::strategies::summarize(
            provider,
            &[json!({"role": "user", "content": content})],
            Some(budget + THINK_ALLOWANCE),
        )
        .await;
        (py_strip(&text).to_string(), cut)
    };
    let (mut text, mut cut) = ask(format!(
        "{}{}",
        FOLD_PROMPT.replace("{budget}", &budget.to_string()),
        chain_text(rows, chain)
    ))
    .await;
    if text.is_empty() {
        return degraded();
    }
    let got = estimate_one(&json!({"role": "user", "content": text}));
    if cut || got > budget {
        // ONE re-summarise, telling the model the numbers.
        let tell = if cut {
            RESUMMARIZE_CUT.replace("{budget}", &budget.to_string())
        } else {
            RESUMMARIZE
                .replace("{got}", &got.to_string())
                .replace("{budget}", &budget.to_string())
        };
        (text, cut) = ask(format!("{tell}{text}")).await;
        if text.is_empty()
            || cut
            || estimate_one(&json!({"role": "user", "content": text})) > budget
        {
            return None;
        }
    }
    match split(&text) {
        None => degraded(),
        Some((title, body)) => Some((title, body, false)),
    }
}

/// Summarize `idxs` and append ONE fold row for them — or `None` when the summary
/// would not fit its budget (nothing folded). A fold row in `idxs` is just a
/// member — that is how folds nest: the next fold over this stretch of the view
/// takes the earlier fold as one of its rows, and its level is one above the
/// deepest member.
pub async fn write_fold(
    ctx: &ProjectionCtx<'_>,
    rows: &[Value],
    idxs: &[usize],
) -> Result<Option<Value>, MemoryStoreError> {
    let ids: Vec<i64> = idxs.iter().filter_map(|&i| row_id(&rows[i])).collect();
    let lvl = 1 + idxs
        .iter()
        .filter(|&&i| is_fold(&rows[i]))
        .map(|&i| level(rows, &rows[i]))
        .max()
        .unwrap_or(0);
    // THE INDEX IS INSIDE THE BUDGET: built first, its size reserved from the
    // summary's.
    let index = mechanical_index(rows, idxs);
    let reserve = if index.is_empty() {
        0
    } else {
        estimate_one(&json!({"role": "user", "content": index}))
    };
    let Some((title, mut text, degraded)) =
        summarize(ctx.provider, ctx.budget, rows, idxs, reserve).await
    else {
        return Ok(None);
    };
    if !index.is_empty() {
        text = format!("{text}\n\n{index}");
    }
    let id = ctx.kernel.mint_id();
    let parent = idxs.first().and_then(|&i| row_parent(&rows[i]));
    let fr = make_fold(&ids, &title, &text, degraded, id, lvl, parent);
    save_history(ctx.self_id, ctx.kernel, std::slice::from_ref(&fr)).await?;
    Ok(Some(fr))
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

/// FOLD UNTIL IT FITS: when the rendered view is over `budget`, fold its oldest
/// stretches down to the low watermark; then tidy the head (adjacent folds
/// coalesce), budget or no budget. Returns `(rows + the fold rows written, the
/// view, folded, refused)`. The turn's `fold` projection and `context_recall` (a
/// store longer than the window, e.g. written under a larger one) share it.
/// Python `fold_down`.
///
/// # Errors
/// The memory store refused a fold row.
pub async fn fold_down(
    rows: &[Value],
    ctx: &ProjectionCtx<'_>,
    budget: i64,
) -> Result<(Vec<Value>, Vec<Value>, usize, usize), MemoryStoreError> {
    let mut rows: Vec<Value> = rows.to_vec();
    let mut view = render(&rows);
    let mut folded = 0usize;
    let mut refused = 0usize;
    if estimate_tokens(&view) > budget && rows.len() > 2 {
        let target = (budget as f64 * LOW) as i64;
        // ONE MOVE, REPEATED: fold the oldest stretch of the view — rows, folds,
        // or a mix — until under the low watermark or nothing foldable is left.
        while estimate_tokens(&view) > target {
            let span = next_span(&rows, &view, KEEP_TAIL, SPAN);
            if span.is_empty() {
                break;
            }
            let Some(fr) = write_fold(ctx, &rows, &span).await? else {
                refused += 1; // the summary would not fit: this span stays; stop here
                break;
            };
            rows.push(fr);
            folded += 1;
            view = render(&rows);
        }
    }
    // TIDY THE HEAD, budget or no budget. Coalescing used to happen only while
    // the view was OVER the watermark, so a projection that dropped under it left
    // its adjacent folds standing — and they stayed standing. A session therefore
    // grew one fold per span at the head, forever.
    //
    // Terminates: each pass replaces `FOLD_RUN` adjacent folds with ONE, so the
    // run shrinks by `FOLD_RUN - 1` every time and `next_span` stops selecting
    // once fewer than `FOLD_RUN` remain.
    while refused == 0 {
        let span = next_span(&rows, &view, KEEP_TAIL, SPAN);
        if span.is_empty() || !span.iter().all(|&i| is_fold(&rows[i])) {
            break;
        }
        let Some(fr) = write_fold(ctx, &rows, &span).await? else {
            refused += 1;
            break;
        };
        rows.push(fr);
        folded += 1;
        view = render(&rows);
    }
    Ok((rows, view, folded, refused))
}

/// Fold the oldest stretches of the view down to the low watermark, tidy the
/// head, then hand what is left to `strip`.
pub async fn fold(body: &[Value], ctx: &ProjectionCtx<'_>) -> Result<Projection, MemoryStoreError> {
    let (_, view, folded, refused) =
        fold_down(body, ctx, ctx.budget - NOTICE_ENVELOPE_RESERVE).await?;
    // Stage two and three: strip bodies in what is left, then cut — strip's own
    // logic.
    let out = strip(&view, ctx).await?;
    Ok(Projection {
        body: out.body,
        summary: None,
        omitted_marker: out.omitted_marker,
        stripped: out.stripped,
        stripped_bytes: out.stripped_bytes,
        folded,
        fold_refused: refused,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderError, ProviderEvent, ProviderReport, ProviderStream};
    use async_trait::async_trait;
    use std::sync::Mutex;

    /// A provider that answers each call with the next scripted completion, and
    /// reports `max_tokens` as its stop reason for the first `cuts` calls. Logs
    /// the `max_tokens` it was handed and the ask it was sent.
    struct Scripted {
        answers: Mutex<Vec<String>>,
        cuts: usize,
        asked: Mutex<Vec<(Option<i64>, String)>>,
        report: Mutex<ProviderReport>,
    }

    impl Scripted {
        fn new(answers: &[&str], cuts: usize) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.iter().rev().map(|s| s.to_string()).collect()),
                cuts,
                asked: Mutex::new(Vec::new()),
                report: Mutex::new(ProviderReport::default()),
            })
        }
    }

    #[async_trait]
    impl Provider for Scripted {
        async fn chat(
            &self,
            messages: &[Value],
            max_tokens: Option<i64>,
        ) -> Result<ProviderStream, ProviderError> {
            let ask = messages
                .last()
                .and_then(|m| m.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let n = {
                let mut a = self.asked.lock().unwrap();
                a.push((max_tokens, ask));
                a.len()
            };
            self.report.lock().unwrap().stop_reason =
                (n <= self.cuts).then(|| "max_tokens".to_string());
            let text = self.answers.lock().unwrap().pop().unwrap_or_default();
            let ev: Vec<Result<ProviderEvent, ProviderError>> =
                vec![Ok(ProviderEvent::Token(text))];
            Ok(Box::pin(futures_util::stream::iter(ev)))
        }
        fn model(&self) -> String {
            "scripted".to_string()
        }
        fn report(&self) -> ProviderReport {
            self.report.lock().unwrap().clone()
        }
        fn clear_stop_reason(&self) {
            self.report.lock().unwrap().stop_reason = None;
        }
    }

    fn one(content: &str) -> Vec<Value> {
        vec![json!({"role": "user", "id": 1, "content": content})]
    }

    fn rows6() -> Vec<Value> {
        let mut rows =
            vec![json!({"role": "user", "id": 1, "content": "the goal: write report.md"})];
        for i in 0..6 {
            rows.push(json!({"role": "assistant", "id": 10 + i, "content": "x".repeat(2000)}));
        }
        rows
    }

    #[tokio::test]
    async fn summary_body_falls_back_to_the_uncut_title() {
        let long = "t".repeat(200);
        let p = Scripted::new(&[&long], 0);
        let dp: Arc<dyn Provider> = p;
        // a span big enough that a 200-char answer is inside its summary budget
        let span = "x".repeat(20_000);
        let (title, body, degraded) = summarize(&dp, 100_000, &one(&span), &[0], 0).await.unwrap();
        assert!(!degraded);
        assert_eq!(title.chars().count(), MAX_TITLE, "the TITLE is cut");
        assert_eq!(body, long, "the BODY is the uncut title");
    }

    #[tokio::test]
    async fn summary_lines_split_like_python_splitlines() {
        let dp: Arc<dyn Provider> = Scripted::new(&["the title\u{1c}the body"], 0);
        let (title, body, _) = summarize(&dp, 100_000, &one("x"), &[0], 0).await.unwrap();
        assert_eq!(title, "the title");
        assert_eq!(body, "the body");
        let dp: Arc<dyn Provider> = Scripted::new(&["\r\n\"A title\"\r\n\r\nthe body\r\n"], 0);
        let (title, body, _) = summarize(&dp, 100_000, &one("x"), &[0], 0).await.unwrap();
        assert_eq!(title, "A title");
        assert_eq!(body, "the body");
        let dp: Arc<dyn Provider> = Scripted::new(&["**\"t\"**"], 0);
        let (title, _, _) = summarize(&dp, 100_000, &one("x"), &[0], 0).await.unwrap();
        assert_eq!(title, "\"t\"");
    }

    #[tokio::test]
    async fn an_empty_summary_degrades_to_titles() {
        let dp: Arc<dyn Provider> = Scripted::new(&["   \u{1d} "], 0);
        let (_, _, degraded) = summarize(&dp, 100_000, &one("x"), &[0], 0).await.unwrap();
        assert!(degraded, "python strips the information separators too");
    }

    #[tokio::test]
    async fn an_oversized_summary_is_resummarised_once_then_refused() {
        let big = format!("TITLE\n\n{}", "word ".repeat(5000));
        let p = Scripted::new(&[&big, &big], 0);
        let dp: Arc<dyn Provider> = p.clone();
        let out = summarize(&dp, 4000, &rows6(), &[1, 2, 3, 4, 5], 0).await;
        assert!(
            out.is_none(),
            "still over budget after the retry — nothing is folded"
        );
        let asked = p.asked.lock().unwrap();
        assert_eq!(asked.len(), 2);
        assert!(asked[1].1.contains("Rewrite it to fit"));
        assert!(asked[0].1.contains("AT MOST ~") && asked[0].1.contains("ERRORS:"));
    }

    #[tokio::test]
    async fn a_retry_that_fits_is_the_summary() {
        let big = format!("TITLE\n\n{}", "word ".repeat(5000));
        let dp: Arc<dyn Provider> = Scripted::new(&[&big, "TITLE\n\nshort summary"], 0);
        let (title, body, degraded) = summarize(&dp, 4000, &rows6(), &[1, 2, 3, 4, 5], 0)
            .await
            .unwrap();
        assert_eq!(title, "TITLE");
        assert_eq!(body, "short summary");
        assert!(!degraded);
    }

    #[tokio::test]
    async fn a_summary_cut_at_its_budget_is_resummarised_once() {
        let p = Scripted::new(&["TITLE\n\npartial", "TITLE\n\nwhole summary"], 1);
        let dp: Arc<dyn Provider> = p.clone();
        let rows = rows6();
        let (_, body, _) = summarize(&dp, 4000, &rows, &[1, 2, 3, 4, 5], 0)
            .await
            .unwrap();
        assert_eq!(body, "whole summary");
        let want = summary_budget(4000, &rows, &[1, 2, 3, 4, 5]);
        let bound = want + THINK_ALLOWANCE; // the thought counts against max_tokens
        let asked: Vec<Option<i64>> = p.asked.lock().unwrap().iter().map(|(m, _)| *m).collect();
        assert_eq!(asked, vec![Some(bound), Some(bound)]);
    }

    #[tokio::test]
    async fn a_summary_cut_twice_is_refused_nothing_folded() {
        let dp: Arc<dyn Provider> = Scripted::new(&["TITLE\n\npartial", "TITLE\n\npartial"], 2);
        assert!(summarize(&dp, 4000, &rows6(), &[1, 2, 3, 4, 5], 0)
            .await
            .is_none());
    }

    #[test]
    fn the_mechanical_index_keeps_files_ids_and_the_users_words() {
        let rows = vec![
            json!({"role": "user", "id": 1, "content": "please check filler_3.txt for deadbeef99"}),
            json!({"role": "assistant", "id": 2, "content": "reading notes/a.md"}),
            json!({"role": "user", "id": 3, "content": "[reply to 9 · notes read]\nbody"}),
        ];
        let idx = mechanical_index(&rows, &[0, 1, 2]);
        assert!(
            idx.contains("filler_3.txt") && idx.contains("notes/a.md"),
            "{idx}"
        );
        assert!(idx.contains("deadbeef99"));
        assert!(idx.contains("user said: please check filler_3.txt for deadbeef99"));
        assert!(
            !idx.contains("[reply to"),
            "runtime lines are not the user's words"
        );
    }
}
