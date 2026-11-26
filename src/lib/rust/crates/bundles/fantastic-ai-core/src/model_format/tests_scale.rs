//! The scale measurement (#766, W11): X1 of spec 2.11. Run by hand:
//! `cargo test -p fantastic-ai-core scale::cold_and_warm -- --ignored --nocapture`.
//!
//! A history of about 1M tokens (4 MB of text), mostly calls and reply batches, through
//! the chat's render cache as a pass drives it: a cold render after a switch D -> H, then
//! warm passes with 1, 10 and 100 new rows. The time inside the adapter's `render` is
//! measured apart from the rest of the pass (the digest and compare of every row, the
//! chain), so the two parts are printed separately.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

use serde_json::{json, Value};

use super::cache::RenderCache;
use super::default::Tags;
use super::hermes::Hermes;
use super::tests_cache::{run, Opts};
use super::Renderer;
use crate::tool_parse::render_call;

/// About 1M tokens at four bytes a token.
const TARGET_BYTES: usize = 4_000_000;

/// An adapter that counts its renders and the time spent in them.
struct Timed<'r> {
    inner: &'r dyn Renderer,
    calls: AtomicUsize,
    nanos: AtomicU64,
}

impl<'r> Timed<'r> {
    fn new(inner: &'r dyn Renderer) -> Self {
        Self {
            inner,
            calls: AtomicUsize::new(0),
            nanos: AtomicU64::new(0),
        }
    }

    /// `(renders, seconds inside them)` since the last call, and reset.
    fn take(&self) -> (usize, f64) {
        let n = self.calls.swap(0, Ordering::SeqCst);
        let t = self.nanos.swap(0, Ordering::SeqCst);
        (n, t as f64 / 1e9)
    }
}

impl Renderer for Timed<'_> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn rev(&self) -> u32 {
        self.inner.rev()
    }
    fn render(&self, text: &str) -> String {
        let start = Instant::now();
        let out = self.inner.render(text);
        let spent = u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX);
        self.nanos.fetch_add(spent, Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);
        out
    }
}

/// Row `i`: an assistant answer with two calls, or a reply batch of two blocks.
fn row(i: i64) -> Value {
    if i % 2 == 0 {
        let mut a = serde_json::Map::new();
        a.insert("target_id".into(), json!("notes"));
        a.insert(
            "payload".into(),
            json!({"type": "write", "path": format!("f{i}.rs"),
                   "bytes": format!("fn f{i}() {{\n    let x = \"ї й {i}\";\n}}\n").repeat(12)}),
        );
        let send = render_call("send", &a);
        let mut d = serde_json::Map::new();
        d.insert("text".into(), json!(format!("крок {i}")));
        let line = render_call("line_summary", &d);
        json!({"id": i, "role": "assistant", "content": format!("Пишу файл {i}.\n{send}\n{line}")})
    } else {
        let body = format!("рядок {i}: готово, без помилок; ").repeat(20);
        json!({"id": i, "role": "user", "content": format!(
            "[reply to {i}1 · notes write · ↑{i} · 1 of 2 in batch {i}]\n{{\"ok\": true}}\n\n\
             [reply to {i}2 · notes read · ↑{i} · 2 of 2 in batch {i}]\n{body}")})
    }
}

/// A view of rows `1..=n`, between a system message and an arriving one.
fn view(n: i64) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": "SYS"})];
    out.extend((1..=n).map(row));
    out.push(json!({"role": "user", "content": "go"}));
    out
}

/// The number of rows whose text adds up to about [`TARGET_BYTES`].
fn rows_for_target() -> i64 {
    let mut bytes = 0usize;
    let mut n = 0i64;
    while bytes < TARGET_BYTES {
        n += 1;
        bytes += row(n)["content"].as_str().map_or(0, str::len);
    }
    n
}

// X1
#[test]
#[ignore = "the scale measurement; run by hand (spec 2.11)"]
fn cold_and_warm() {
    let n = rows_for_target();
    let c = <RenderCache>::new();
    let (d, h) = (Timed::new(&Tags), Timed::new(&Hermes));
    let source: usize = view(n)
        .iter()
        .filter_map(|m| m["content"].as_str())
        .map(str::len)
        .sum();
    println!(
        "X1: {n} rows, {source} bytes of text (about {} tokens)",
        source / 4
    );

    // D first: every row mapped once and kept, as for any type
    let m = view(n);
    let start = Instant::now();
    let first = run(&c, &d, &m, &Opts::default());
    let (renders, _) = d.take();
    println!(
        "X1 D: {:.3} s, {renders} renders of whole messages",
        start.elapsed().as_secs_f64()
    );
    assert_eq!(first.rows, n as usize, "the default type keeps its rows");

    // the switch to H: cold over every row
    let start = Instant::now();
    let cold = run(&c, &h, &m, &Opts::default());
    let total = start.elapsed().as_secs_f64();
    let (renders, cold_render) = h.take();
    println!(
        "X1 H cold: {total:.3} s, render {cold_render:.3} s, hash and compare {:.3} s, {renders} renders",
        total - cold_render
    );
    assert!(cold.cold);
    // the system and the arriving message are rendered every time; the rows once
    assert_eq!(renders, n as usize + 2, "cold: every row");
    assert_eq!(cold.rows, n as usize);
    let stats = c.stats();
    assert_eq!(stats.bytes, stats.counted);
    assert!(stats.bytes <= super::cache::bound_of(&serde_json::Map::new()));

    let mut last_kept = cold.kept;
    let mut have = n;
    let mut warm_100 = 0.0;
    for add in [1i64, 10, 100] {
        have += add;
        let m = view(have);
        let start = Instant::now();
        let warm = run(&c, &h, &m, &Opts::default());
        let total = start.elapsed().as_secs_f64();
        let (renders, render) = h.take();
        println!(
            "X1 H warm +{add}: {total:.3} s, render {render:.3} s, hash and compare {:.3} s, \
             {renders} renders, kept {}",
            total - render,
            warm.kept
        );
        assert_eq!(warm.rows, add as usize, "warm +{add}: only the new rows");
        assert_eq!(renders, add as usize + 2, "warm +{add}");
        assert!(
            warm.kept > last_kept,
            "warm +{add}: kept grows with the history"
        );
        last_kept = warm.kept;
        if add == 100 {
            warm_100 = render;
        }
        let stats = c.stats();
        assert_eq!(stats.bytes, stats.counted);
    }
    assert!(
        warm_100 < cold_render / 10.0,
        "warm render at 100 new rows {warm_100:.3} s, cold {cold_render:.3} s"
    );
}
