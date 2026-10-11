//! THE FOLD/STRIP/RETRIEVAL MACHINERY, pinned claim by claim.
//!
//! These are the parts of the Context Protocol that only python carried until
//! now: a fold is a MARK (not a deletion), a strip drops reply BODIES (not
//! turns), and `grep`/`recall` are how the model reaches what left its view. Each
//! test states the claim as a failure, because the failure is what the mechanism
//! exists to prevent.

use crate::folds::{
    chains, clean_title, expand_ids, is_open, make_fold, quoted_title, render, row_id,
    SUMMARY_INPUT_CAP,
};
use crate::strategies::fold::{next_span, FOLD_RUN, KEEP_TAIL};
use crate::strategies::strip::{is_reply, pid_of, stub};
use serde_json::{json, Value};

/// A delivered reply row — the shape strip reduces.
fn reply(id: i64, pid: i64, parent: Option<i64>, body: &str) -> Value {
    json!({
        "role": "user",
        "id": id,
        "parent": parent,
        "content": format!("[reply to {pid} · notes read · \"chunk {pid}\"]\n{body}"),
    })
}

fn turn(id: i64, role: &str, parent: Option<i64>, content: &str) -> Value {
    json!({"role": role, "id": id, "parent": parent, "content": content})
}

// ─── folds: the mark ────────────────────────────────────────────

#[test]
fn a_fold_hides_its_members_and_sits_where_the_first_one_was() {
    // THE WHOLE POINT OF A MARK. If the members stayed visible the fold would be
    // pure cost; if the fold landed at the END the history would reorder itself
    // under the model every time one was written.
    let rows = vec![
        turn(1, "user", None, "the goal"),
        turn(2, "assistant", Some(1), "working"),
        turn(3, "user", Some(1), "more"),
        turn(4, "user", None, "the live tail"),
        make_fold(
            &[2, 3],
            "did the work",
            "it went fine",
            false,
            5,
            1,
            Some(1),
        ),
    ];
    let view = render(&rows);
    let ids: Vec<i64> = view.iter().filter_map(row_id).collect();
    assert_eq!(
        ids,
        vec![1, 5, 4],
        "the fold must stand where its first member stood, with the members gone: {ids:?}"
    );
}

#[test]
fn a_fold_of_folds_hides_the_folds_it_wraps() {
    // HOW DEPTH WORKS. A fold is an ordinary member, so the next fold over the
    // same stretch takes it — and the inner ones must leave the view, or the
    // head carries every level at once.
    let inner_a = make_fold(&[2, 3], "a", "aa", false, 10, 1, Some(1));
    let inner_b = make_fold(&[4, 5], "b", "bb", false, 11, 1, Some(1));
    let rows = vec![
        turn(1, "user", None, "the goal"),
        turn(2, "user", Some(1), "x"),
        turn(3, "user", Some(1), "y"),
        turn(4, "user", Some(1), "z"),
        turn(5, "user", Some(1), "w"),
        inner_a,
        inner_b,
        make_fold(&[10, 11], "both", "everything", false, 12, 2, Some(1)),
    ];
    let ids: Vec<i64> = render(&rows).iter().filter_map(row_id).collect();
    assert_eq!(
        ids,
        vec![1, 12],
        "only the OUTER fold may remain in the view: {ids:?}"
    );
}

#[test]
fn a_fold_id_descends_to_the_rows_it_collapsed() {
    // THE DESCENT PATH, recursive. `grep <fold id>` is worthless if it resolves
    // to the fold and stops — the caller asked to look INSIDE.
    let rows = vec![
        make_fold(&[2, 3], "a", "aa", false, 10, 1, None),
        make_fold(&[10, 4], "b", "bb", false, 11, 2, None),
    ];
    let mut got = expand_ids(&rows, &[11]);
    got.sort_unstable();
    assert_eq!(
        got,
        vec![2, 3, 4],
        "a fold must expand transitively: {got:?}"
    );
}

#[test]
fn a_chain_is_its_root_plus_everything_parented_into_it() {
    // CHAINS ARE DERIVED. Nothing stores a thread id; `parent` is the whole
    // structure, and two interleaved chains must not merge.
    let rows = vec![
        turn(1, "user", None, "task A"),
        turn(2, "user", None, "task B"),
        turn(3, "assistant", Some(1), "on A"),
        turn(4, "assistant", Some(2), "on B"),
        turn(5, "user", Some(3), "more A"),
    ];
    let got = chains(&rows);
    assert_eq!(got, vec![vec![0, 2, 4], vec![1, 3]], "{got:?}");
}

#[test]
fn a_chain_whose_last_turn_made_a_send_is_still_open() {
    // A REPLY IS OWED, so the stretch is not finished and must not fold.
    let rows = vec![
        turn(1, "user", None, "go"),
        turn(
            2,
            "assistant",
            Some(1),
            r#"<call name="send"><target_id>notes</target_id><type>read</type></call>"#,
        ),
    ];
    assert!(
        is_open(&rows, &[0, 1]),
        "a pending send leaves the chain open"
    );

    let closed = vec![
        turn(1, "user", None, "go"),
        turn(2, "assistant", Some(1), "done looking"),
        reply(3, 9, Some(2), "the answer"),
    ];
    assert!(!is_open(&closed, &[0, 1, 2]), "a settled chain is closed");
}

#[test]
fn a_fold_title_can_neither_close_its_tag_nor_quote_itself() {
    let t = clean_title("a \"quoted\" thing] with   space");
    assert!(!t.contains(']'), "{t}");
    assert!(!t.contains('"'), "{t}");
    assert_eq!(t, "a 'quoted' thing) with space");
    assert_eq!(clean_title("   "), "(untitled)");
    assert_eq!(clean_title(&"x".repeat(300)).chars().count(), 140);
}

#[test]
fn a_rendered_rows_title_is_read_back_off_its_reply_tag() {
    // WHAT A DEGRADED FOLD IS MADE OF. With no summarizer the fold carries its
    // members' own titles instead, and those live on the reply tags — after the
    // ` · ` that separates them from the verb.
    let r = reply(3, 42, None, "the body");
    let content = r.get("content").and_then(Value::as_str).unwrap();
    assert_eq!(quoted_title(content), Some("chunk 42"), "{content}");
    assert_eq!(quoted_title("no marks here"), None);
    // A FOLD HEAD IS NOT IN THAT GRAMMAR — it reads `] "title"`, so the title
    // comes off the row's own field, never scraped back out of the text.
    let f = make_fold(&[1, 2], "what happened", "body", false, 7, 1, None);
    assert_eq!(
        f.get("title").and_then(Value::as_str),
        Some("what happened")
    );
}

// ─── next_span: WHAT folds, and in what order ───────────────────

#[test]
fn a_span_never_takes_the_goal_turn_or_the_live_tail() {
    // THE TWO PINS. The goal is the instruction the whole run serves; the tail
    // is what the model is working on right now. Folding either is how a run
    // forgets what it is doing.
    let mut rows = vec![turn(1, "user", None, "THE GOAL")];
    for i in 2..=20 {
        rows.push(turn(i, "user", Some(1), &format!("row {i}")));
    }
    let view = render(&rows);
    let span = next_span(&rows, &view, KEEP_TAIL, i64::MAX);
    assert!(!span.is_empty(), "a 20-row view has something to fold");
    assert!(
        !span.contains(&0),
        "the goal turn must never fold: {span:?}"
    );
    let last_foldable = rows.len() - KEEP_TAIL;
    assert!(
        span.iter().all(|&i| i < last_foldable),
        "the live tail must never fold: {span:?} against tail at {last_foldable}"
    );
}

#[test]
fn a_span_never_ends_between_a_call_and_its_answer() {
    // A HALF-FOLDED EXCHANGE is worse than an unfolded one: the model reads a
    // summary that stops mid-question.
    let mut rows = vec![turn(1, "user", None, "goal")];
    for i in 0..12 {
        let id = 2 + i * 2;
        rows.push(turn(id, "assistant", Some(1), "asking"));
        rows.push(reply(id + 1, id, Some(id), "answered"));
    }
    let view = render(&rows);
    let span = next_span(&rows, &view, KEEP_TAIL, i64::MAX);
    assert!(!span.is_empty());
    let last = *span.last().unwrap();
    assert_ne!(
        rows[last].get("role").and_then(Value::as_str),
        Some("assistant"),
        "a span may not end on an assistant turn whose reply is outside it"
    );
}

#[test]
fn folds_at_the_head_are_skipped_until_there_are_fold_run_of_them() {
    // THE ANTI-LINKED-LIST RULE. A span that starts on the previous fold wraps
    // it, and doing that every time builds summaries-of-summaries (measured
    // 2026-09-06 in python: twelve levels in 26 cuts, the model lost in it for
    // 50 turns). Below the threshold: take ordinary rows. At it: fold the folds.
    let mut rows = vec![turn(1, "user", None, "goal")];
    // The rows each fold collapsed — present, so the fold has somewhere to sit.
    for k in 0..FOLD_RUN {
        rows.push(turn(900 + k as i64, "user", Some(1), "folded away"));
    }
    for k in 0..(FOLD_RUN - 1) {
        rows.push(make_fold(
            &[900 + k as i64],
            "old",
            "body",
            false,
            100 + k as i64,
            1,
            Some(1),
        ));
    }
    for i in 0..14 {
        rows.push(turn(200 + i, "user", Some(1), &format!("row {i}")));
    }
    let view = render(&rows);
    let span = next_span(&rows, &view, KEEP_TAIL, i64::MAX);
    assert!(
        span.iter().all(|&i| !crate::folds::is_fold(&rows[i])),
        "under the threshold the folds at the head are SKIPPED: {span:?}"
    );

    // At the threshold they become the span themselves.
    let mut rows2 = vec![turn(1, "user", None, "goal")];
    for k in 0..FOLD_RUN {
        rows2.push(turn(900 + k as i64, "user", Some(1), "folded away"));
    }
    for k in 0..FOLD_RUN {
        rows2.push(make_fold(
            &[900 + k as i64],
            "old",
            "body",
            false,
            100 + k as i64,
            1,
            Some(1),
        ));
    }
    for i in 0..10 {
        rows2.push(turn(200 + i, "user", Some(1), &format!("row {i}")));
    }
    let view2 = render(&rows2);
    let span2 = next_span(&rows2, &view2, KEEP_TAIL, i64::MAX);
    assert_eq!(span2.len(), FOLD_RUN, "{span2:?}");
    assert!(
        span2.iter().all(|&i| crate::folds::is_fold(&rows2[i])),
        "at the threshold the span IS the run of folds: {span2:?}"
    );
}

#[test]
fn a_view_with_nothing_foldable_yields_no_span() {
    // TERMINATION. The fold loop runs `while over budget`; an empty span is the
    // only thing that stops it.
    let rows = vec![turn(1, "user", None, "goal"), turn(2, "user", None, "one")];
    let view = render(&rows);
    assert!(next_span(&rows, &view, KEEP_TAIL, i64::MAX).is_empty());
}

// ─── strip: bodies go, titles stay ──────────────────────────────

#[test]
fn a_stripped_block_keeps_its_tag_and_names_what_it_cost() {
    // THE TAG IS THE ADDRESS. A stub that lost its `[reply to <pid> …]` line is
    // unrecallable — the bytes are gone AND the model cannot ask for them back.
    let block = "[reply to 42 · notes read · \"survey chunk 2\"]\nabcdefghij";
    let s = stub(block);
    assert!(
        s.starts_with("[reply to 42 · notes read · \"survey chunk 2\"]"),
        "{s}"
    );
    assert!(s.contains("(stripped: 10 bytes — recall by pid)"), "{s}");
    assert_eq!(pid_of(block), Some("42"));
}

#[test]
fn only_a_delivered_reply_is_strippable() {
    // The model's OWN prose is the distillation of the reply it just read — it
    // is tiny and it is what survives the body. Stripping it would delete the
    // only thing left.
    assert!(is_reply(&reply(1, 7, None, "body")));
    assert!(!is_reply(&turn(2, "assistant", None, "my reading of it")));
    assert!(!is_reply(&turn(3, "user", None, "a person talking")));
}

#[test]
fn a_tag_that_is_not_a_reply_block_yields_no_pid() {
    assert_eq!(pid_of("just some prose"), None);
    assert_eq!(
        pid_of("[reply to 9]"),
        None,
        "the ` · ` separator is required"
    );
}

// ─── python-parity: text bytes the model reads ──────────────────────────────

#[test]
fn chain_text_is_python_json_dumps() {
    // `json.dumps(items, ensure_ascii=False, default=str)` — `, `/`: ` separators
    // and RAW non-ASCII. `serde_json::to_string` was compact (`,`/`:`), so every
    // fold's summarizer input differed from python's by two chars per key.
    let rows = vec![json!({"role": "user", "id": 1, "content": "a — b"})];
    assert_eq!(
        crate::folds::chain_text(&rows, &[0]),
        "[{\"role\": \"user\", \"id\": 1, \"content\": \"a — b\"}]"
    );
}

#[test]
fn a_stub_says_how_big_the_body_was_in_code_points() {
    // python prints `len(body)` — CODE POINTS. A byte count reported a non-ASCII
    // body as bigger than the model's own `pull` of it.
    let block = "[reply to 7 \u{b7} notes read]\néé";
    assert_eq!(
        stub(block),
        "[reply to 7 \u{b7} notes read] (stripped: 2 bytes — recall by pid)"
    );
}

#[test]
fn quoted_title_bounds_on_code_points() {
    // python `· "([^"]{1,140})"` counts CHARACTERS: a 140-character non-ASCII
    // title is inside the bound, and a byte count put it outside.
    let t = "é".repeat(140);
    let content = format!("[reply to 1 \u{b7} n r \u{b7} \"{t}\"]");
    assert_eq!(quoted_title(&content), Some(t.as_str()));
    let too_long = "é".repeat(141);
    let content = format!("[reply to 1 \u{b7} n r \u{b7} \"{too_long}\"]");
    assert_eq!(quoted_title(&content), None);
}

#[test]
fn chain_text_elision_tail_is_python_slice() {
    // python `c[: per//2] + … + c[-per//2 :]`: `-per // 2` FLOORS, so the TAIL is
    // one longer than the head when `per` is odd. The elided count is the same
    // either way, so a halved tail silently dropped one character.
    let per = SUMMARY_INPUT_CAP.max(400); // one row ⇒ per = SUMMARY_INPUT_CAP
    let body: String = "abcdefghij".repeat((per / 10) + 5);
    let rows = vec![json!({"role": "user", "id": 1, "content": body.clone()})];
    let out = crate::folds::chain_text(&rows, &[0]);
    let n = body.chars().count();
    let tail: String = body.chars().skip(n - per.div_ceil(2)).collect();
    // The rendered JSON holds the tail verbatim (no escapes in this body).
    assert!(
        out.contains(&tail[..40]),
        "the tail must start {} chars from the end",
        per.div_ceil(2)
    );
}

/// A fold is sized by TOKENS, never a row count (owner, 2026-10-11): `next_span`
/// takes the oldest rows until their tokens reach `need`, and no further.
#[test]
fn a_span_is_sized_by_tokens_not_rows() {
    let mut rows = vec![turn(1, "user", None, "goal")];
    for i in 2..=40 {
        rows.push(turn(
            i,
            "user",
            Some(1),
            &format!("row {i} {}", "x".repeat(400)),
        ));
    }
    let view = render(&rows);
    let one = crate::context::estimate_tokens(std::slice::from_ref(&rows[1]));
    let small = next_span(&rows, &view, KEEP_TAIL, one * 3);
    assert_eq!(small.len(), 3, "need of ~3 rows takes 3 rows: {small:?}");
    let all = next_span(&rows, &view, KEEP_TAIL, i64::MAX);
    assert_eq!(
        all.len(),
        40 - KEEP_TAIL - 1,
        "everything but the goal and the tail"
    );
}
