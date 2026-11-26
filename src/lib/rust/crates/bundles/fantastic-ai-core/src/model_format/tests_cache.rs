//! The render cache (#766, W5, W6): K1-K35, K39-K53 of spec 2.2.
//!
//! A pass is driven here as the turn drives it: `messages` in flight (the view with each
//! row's `id` and the form it took), mapped under a type declared with the renderer
//! under test, and the chat's bound. The counting renderer wraps a text in its type's
//! name, so a render is seen and counted.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use super::cache::{
    bound_of, epoch, epoch_of, why, Ctx, Hash, Hasher, RenderCache, Sha, View, Why,
};
use super::default::Tags;
use super::hermes::Hermes;
use super::map::Shown;
use super::{trace, Decl, Renderer};

// ─── helpers ─────────────────────────────────────────────────────────────────

/// A renderer that wraps a text in `<name>…</name>` and counts its calls.
pub(super) struct Count {
    pub(super) name: &'static str,
    pub(super) rev: u32,
    pub(super) calls: AtomicUsize,
}

impl Count {
    pub(super) fn new(name: &'static str) -> Self {
        Self {
            name,
            rev: 1,
            calls: AtomicUsize::new(0),
        }
    }

    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Renderer for Count {
    fn name(&self) -> &'static str {
        self.name
    }
    fn rev(&self) -> u32 {
        self.rev
    }
    fn render(&self, text: &str) -> String {
        self.calls.fetch_add(1, Ordering::SeqCst);
        format!("<{0}>{text}</{0}>", self.name)
    }
}

/// A digest of one byte that is always 0: every two sources collide (K20, K21).
pub(super) struct Zero;

impl Hasher for Zero {
    fn digest(_bytes: &[u8]) -> Hash {
        [0; 32]
    }
}

/// A stored row as the view carries it.
pub(super) fn row(id: i64, text: &str) -> Value {
    let role = if id % 2 == 1 { "user" } else { "assistant" };
    json!({"id": id, "role": role, "content": text})
}

/// The view of a pass: the system message, `rows`, then the arriving message.
pub(super) fn conv(rows: &[(i64, &str)], arriving: &str) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": "SYS"})];
    out.extend(rows.iter().map(|(id, t)| row(*id, t)));
    out.push(json!({"role": "user", "content": arriving}));
    out
}

/// `n` rows `1..=n`, each `"row <id>"` with `pad` more bytes.
pub(super) fn rows_n(from: i64, n: i64, pad: usize) -> Vec<(i64, String)> {
    (from..from + n)
        .map(|i| (i, format!("row {i} {}", "x".repeat(pad))))
        .collect()
}

pub(super) fn refs(rows: &[(i64, String)]) -> Vec<(i64, &str)> {
    rows.iter().map(|(i, t)| (*i, t.as_str())).collect()
}

/// What a pass hands the cache beside its messages, owned; `forms` are set on the
/// messages before the pass, as the assembly sets them.
pub(super) struct Opts {
    pub(super) epoch: Hash,
    pub(super) bound: usize,
    pub(super) forms: BTreeMap<i64, String>,
    pub(super) model: Option<String>,
    pub(super) provider: Option<String>,
    pub(super) last_send: Option<f64>,
}

impl Default for Opts {
    fn default() -> Self {
        Self {
            epoch: epoch(),
            bound: usize::MAX,
            forms: BTreeMap::new(),
            model: Some("m".into()),
            provider: Some("p".into()),
            last_send: Some(crate::helpers::now_secs()),
        }
    }
}

impl Opts {
    pub(super) fn ctx(&self) -> Ctx<'_> {
        Ctx {
            epoch: self.epoch,
            bound: self.bound,
            model: self.model.as_deref(),
            provider_id: self.provider.as_deref(),
            last_send: self.last_send,
        }
    }

    pub(super) fn bound(bound: usize) -> Self {
        Self {
            bound,
            ..Self::default()
        }
    }

    pub(super) fn form(mut self, id: i64, form: &str) -> Self {
        self.forms.insert(id, form.into());
        self
    }
}

/// One finished pass.
#[derive(Debug)]
pub(super) struct Done {
    pub(super) messages: Vec<Value>,
    pub(super) rows: usize,
    pub(super) kept: usize,
    pub(super) kept_endpoint: usize,
    pub(super) cold: bool,
    pub(super) dropped: Vec<(i64, Why)>,
    pub(super) links: Vec<Hash>,
    pub(super) events: Vec<Value>,
}

/// The type `r` renders for: a registered type's declaration with `r` as its calls; for a
/// test renderer, the default type's markers and sections under `r`'s name.
pub(super) fn decl_of(r: &dyn Renderer) -> Decl<'_> {
    Decl {
        name: r.name(),
        calls: r,
        ..*super::declared(r.name()).unwrap_or_else(|| super::of(super::DEFAULT))
    }
}

/// `messages` as a pass shows them: each row's form from `o`, as the assembly sets it.
pub(super) fn shown_of(messages: &[Value], o: &Opts) -> Vec<Shown> {
    messages
        .iter()
        .map(|m| {
            let mut s = Shown::of(m);
            if let Some(f) = s.id.and_then(|id| o.forms.get(&id)) {
                s.form = Some(f.clone());
            }
            s
        })
        .collect()
}

/// A pass's view, opened (pins held).
pub(super) fn open<'c, H: Hasher>(
    cache: &'c RenderCache<H>,
    r: &dyn Renderer,
    messages: &[Value],
    o: &Opts,
) -> View<'c, H> {
    cache.view(&decl_of(r), "ai", &shown_of(messages, o), &o.ctx())
}

/// A pass, run to its end.
pub(super) fn run<H: Hasher>(
    cache: &RenderCache<H>,
    r: &dyn Renderer,
    messages: &[Value],
    o: &Opts,
) -> Done {
    finish(open(cache, r, messages, o))
}

pub(super) fn finish<H: Hasher>(v: View<'_, H>) -> Done {
    let messages = v.messages.clone();
    let (rows, kept, kept_endpoint, cold, links) =
        (v.rows, v.kept, v.kept_endpoint, v.cold, v.links.clone());
    let mut ev = trace::Events::default();
    let dropped = v.finish(&mut ev, "turn");
    Done {
        messages,
        rows,
        kept,
        kept_endpoint,
        cold,
        dropped,
        links,
        events: ev.into_rows(),
    }
}

/// The view mapped with no cache.
pub(super) fn plain(r: &dyn Renderer, messages: &[Value]) -> Vec<Value> {
    super::map::messages(&decl_of(r), "ai", messages)
}

pub(super) fn cache() -> RenderCache {
    RenderCache::new()
}

fn exact<H: Hasher>(c: &RenderCache<H>) {
    let s = c.stats();
    assert_eq!(s.bytes, s.counted, "byte count: {s:?}");
}

fn ids_of<H: Hasher>(c: &RenderCache<H>, kind: &str) -> Vec<i64> {
    c.stats()
        .keys
        .into_iter()
        .filter(|(t, _)| t == kind)
        .map(|(_, id)| id)
        .collect()
}

/// The bytes an entry of a row of `text` counts under the counting renderer `name`: its
/// canonical source and `<name>text</name>`.
fn entry_size(text: &str, name: &str) -> usize {
    let source = super::map::canonical(&Shown::of(&json!({"content": text})));
    source.len() + text.len() + 2 * name.len() + 5
}

// ─── K1-K19 ──────────────────────────────────────────────────────────────────

// K1
#[test]
fn cold_renders_all() {
    let (c, r) = (cache(), Count::new("t"));
    let rows = rows_n(1, 5, 0);
    let d = run(&c, &r, &conv(&refs(&rows), "go"), &Opts::default());
    assert_eq!(d.rows, 5);
    assert!(d.cold);
    // the five rows, plus the system and the arriving message, rendered every time
    assert_eq!(r.calls(), 7);
    assert_eq!(ids_of(&c, "t"), vec![1, 2, 3, 4, 5]);
    assert_eq!(d.events[0]["event"], "render_cold");
    assert_eq!(d.events[0]["rows"], 5);
}

// K2
#[test]
fn warm_renders_new_only() {
    let (c, r) = (cache(), Count::new("t"));
    let rows = rows_n(1, 5, 0);
    run(&c, &r, &conv(&refs(&rows), "go"), &Opts::default());
    let more = rows_n(1, 8, 0);
    let before = r.calls();
    let d = run(&c, &r, &conv(&refs(&more), "go"), &Opts::default());
    assert_eq!(d.rows, 3);
    assert!(!d.cold);
    assert_eq!(r.calls() - before, 3 + 2);
    assert_eq!(d.events[0]["event"], "render_warm");
    assert_eq!(d.events[0]["rows"], 3);
}

// K3
#[test]
fn two_types_do_not_share() {
    let c = cache();
    let rows = [
        (
            1,
            r#"<call name="send"><routing><target_id>kernel</target_id></routing><payload><type>reflect</type></payload></call>"#,
        ),
        (2, "plain"),
    ];
    let m = conv(&rows, "go");
    let d = run(&c, &Tags, &m, &Opts::default());
    assert_eq!(
        ids_of(&c, "default"),
        vec![1, 2],
        "default keeps its entries too"
    );
    assert_eq!(d.messages, plain(&Tags, &m));
    let h = run(&c, &Hermes, &m, &Opts::default());
    assert_eq!(ids_of(&c, "hermes"), vec![1, 2]);
    assert_eq!(h.rows, 2);
    assert_eq!(h.messages, plain(&Hermes, &m));
    let t = Count::new("t");
    let d = run(&c, &t, &m, &Opts::default());
    assert_eq!(d.rows, 2, "no hermes entry serves another type");
    assert_eq!(ids_of(&c, "t"), vec![1, 2]);
}

// K4
#[test]
fn same_text_two_rows() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&[(1, "same"), (2, "same")], "go");
    assert_eq!(run(&c, &r, &m, &Opts::default()).rows, 2);
    assert_eq!(c.stats().entries, 2);
    assert_eq!(run(&c, &r, &m, &Opts::default()).rows, 0, "both hit");
    let m = conv(&[(1, "other"), (2, "same")], "go");
    let d = run(&c, &r, &m, &Opts::default().form(1, "anchor"));
    assert_eq!(d.rows, 1);
    assert_eq!(d.dropped, vec![(1, Why::Fold)]);
    assert_eq!(c.rendered("t", 2).as_deref(), Some("<t>same</t>"));
}

// K5
#[test]
fn moved_row_hits_render() {
    let (c, r) = (cache(), Count::new("t"));
    let a = run(
        &c,
        &r,
        &conv(&[(1, "a"), (2, "b"), (3, "c")], "go"),
        &Opts::default(),
    );
    let b = run(&c, &r, &conv(&[(2, "b"), (3, "c")], "go"), &Opts::default());
    assert_eq!(b.rows, 0, "row 2 at another position is a render hit");
    assert_ne!(a.links[2], b.links[1], "its link differs");
    assert_eq!(b.kept, 1);
    assert_eq!(a.messages[2], b.messages[1]);
}

// K6
#[test]
fn fold_in_the_middle() {
    let (c, r) = (cache(), Count::new("t"));
    let rows = rows_n(1, 5, 0);
    run(&c, &r, &conv(&refs(&rows), "go"), &Opts::default());
    let mut folded = rows.clone();
    folded[2].1 = "[folded · reply 3]".into();
    let d = run(
        &c,
        &r,
        &conv(&refs(&folded), "go"),
        &Opts::default().form(3, "anchor"),
    );
    assert_eq!(d.rows, 1);
    assert_eq!(d.dropped, vec![(3, Why::Fold)]);
    assert_eq!(d.messages[3]["content"], "<t>[folded · reply 3]</t>");
}

// K7
#[test]
fn thought_row_stable() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&[(1, "q"), (2, "[thought] the answer")], "go");
    let o = || Opts::default().form(2, "thought");
    let first = run(&c, &r, &m, &o());
    for _ in 0..3 {
        let d = run(&c, &r, &m, &o());
        assert_eq!(d.rows, 0);
        assert!(d.dropped.is_empty());
        assert_eq!(d.messages, first.messages);
    }
}

// K8
#[test]
fn stub_drops_with_fold() {
    let (c, r) = (cache(), Count::new("t"));
    run(
        &c,
        &r,
        &conv(&[(1, "q"), (2, "a long reflect body")], "go"),
        &Opts::default(),
    );
    let d = run(
        &c,
        &r,
        &conv(&[(1, "q"), (2, "[stub]")], "go"),
        &Opts::default().form(2, "anchor"),
    );
    assert_eq!(d.dropped, vec![(2, Why::Fold)]);
}

// K9
#[test]
fn fold_row_cuts_members() {
    let (c, r) = (cache(), Count::new("t"));
    let rows = rows_n(1, 5, 0);
    run(&c, &r, &conv(&refs(&rows), "go"), &Opts::default());
    let after = [
        (1, rows[0].1.as_str()),
        (10, "[fold of 2-4]"),
        (5, rows[4].1.as_str()),
    ];
    let d = run(&c, &r, &conv(&after, "go"), &Opts::default());
    assert_eq!(d.rows, 1, "the fold row renders once");
    assert_eq!(d.dropped, vec![(2, Why::Cut), (3, Why::Cut), (4, Why::Cut)]);
    assert_eq!(ids_of(&c, "t"), vec![1, 5, 10]);
    assert_eq!(run(&c, &r, &conv(&after, "go"), &Opts::default()).rows, 0);
}

// K10
#[test]
fn kept_summary_drops() {
    let (c, r) = (cache(), Count::new("t"));
    let o = || Opts::default().form(3, "anchor");
    run(
        &c,
        &r,
        &conv(&[(1, "q"), (2, "a"), (3, "[folded · reply]")], "go"),
        &o(),
    );
    let kept = conv(
        &[
            (1, "q"),
            (2, "a"),
            (3, "[folded · reply] kept: the file has 3 lines"),
        ],
        "go",
    );
    let d = run(&c, &r, &kept, &o());
    assert_eq!(d.dropped, vec![(3, Why::Fold)]);
    assert!(run(&c, &r, &kept, &o()).dropped.is_empty(), "once");
}

// K11
#[test]
fn recall_in_the_middle() {
    let (c, r) = (cache(), Count::new("t"));
    run(
        &c,
        &r,
        &conv(&[(1, "q"), (2, "[folded]"), (3, "x")], "go"),
        &Opts::default().form(2, "anchor"),
    );
    let d = run(
        &c,
        &r,
        &conv(&[(1, "q"), (2, "the whole reply"), (3, "x")], "go"),
        &Opts::default().form(2, "recalled"),
    );
    assert_eq!(d.dropped, vec![(2, Why::Recall)]);
    assert_eq!(d.rows, 1);
}

// K12
#[test]
fn recall_refold_round_trip() {
    let (c, r) = (cache(), Count::new("t"));
    let whole = conv(&[(1, "q"), (2, "the whole reply")], "go");
    let folded = conv(&[(1, "q"), (2, "[folded]")], "go");
    run(&c, &r, &whole, &Opts::default());
    let f1 = run(&c, &r, &folded, &Opts::default().form(2, "anchor"));
    let rc = run(&c, &r, &whole, &Opts::default().form(2, "recalled"));
    let f2 = run(&c, &r, &folded, &Opts::default().form(2, "anchor"));
    assert_eq!(f1.dropped, vec![(2, Why::Fold)]);
    assert_eq!(rc.dropped, vec![(2, Why::Recall)]);
    assert_eq!(f2.dropped, vec![(2, Why::Fold)]);
    assert_eq!(f1.messages, f2.messages);
}

// K13
#[test]
fn cut_releases_absent() {
    let rows = rows_n(1, 6, 0);
    let all = refs(&rows);
    // context_forget (a row pruned); context_clear (a new thread); a projection drop
    // (case, the rows in the view, the ids released as cut)
    type Case<'a> = (&'a str, Vec<(i64, &'a str)>, Vec<i64>);
    let cases: [Case; 3] = [
        (
            "forget",
            all.iter().copied().filter(|(i, _)| *i != 2).collect(),
            vec![2],
        ),
        ("clear", vec![(7, "new thread")], vec![1, 2, 3, 4, 5, 6]),
        ("drop", all[3..].to_vec(), vec![1, 2, 3]),
    ];
    for (what, view, gone) in cases {
        let (c, r) = (cache(), Count::new("t"));
        run(&c, &r, &conv(&all, "go"), &Opts::default());
        let v = open(&c, &r, &conv(&view, "go"), &Opts::default());
        assert_eq!(
            c.stats().entries,
            6 + usize::from(what == "clear"),
            "{what}: released at the pass's end"
        );
        let d = finish(v);
        let want: Vec<(i64, Why)> = gone.iter().map(|i| (*i, Why::Cut)).collect();
        assert_eq!(d.dropped, want, "{what}");
        for i in &gone {
            assert!(c.rendered("t", *i).is_none(), "{what}: {i}");
        }
        let kept = view.iter().filter(|(i, _)| *i <= 6).count();
        assert_eq!(d.rows, view.len() - kept, "{what}: the other rows hit");
    }
}

// K14
#[test]
fn neighbour_change_is_hit() {
    let (c, r) = (cache(), Count::new("t"));
    run(
        &c,
        &r,
        &conv(&[(1, "a"), (2, "b"), (3, "c")], "go"),
        &Opts::default(),
    );
    let d = run(
        &c,
        &r,
        &conv(&[(1, "a"), (2, "B"), (3, "c")], "go"),
        &Opts::default(),
    );
    assert_eq!(d.rows, 1);
    assert_eq!(d.dropped.len(), 1);
    assert_eq!(d.dropped[0].0, 2, "no drop for row 3");
}

// K15
#[test]
fn image_forms() {
    let (c, r) = (cache(), Count::new("t"));
    let pic = json!([{"type": "image", "ref": 9, "bytes": 10, "width": 2, "height": 2}]);
    let with = |text: &str| {
        let mut m = conv(&[(1, "look")], "go");
        m.insert(
            2,
            json!({"id": 2, "role": "user", "content": text, "images": pic}),
        );
        m
    };
    run(
        &c,
        &r,
        &with("[image 9]"),
        &Opts::default().form(2, "image"),
    );
    let line = run(
        &c,
        &r,
        &with("[image 9 · recall {id} to see it]"),
        &Opts::default().form(2, "image_anchor"),
    );
    assert_eq!(line.dropped, vec![(2, Why::Fold)]);
    let alt = run(
        &c,
        &r,
        &with("[image 9: a cat]"),
        &Opts::default().form(2, "image_alt"),
    );
    assert_eq!(alt.dropped, vec![(2, Why::Fold)]);
    assert_eq!(alt.messages[2]["images"], pic, "images carried");
    // non-string content is never cached
    let mut m = conv(&[(1, "look")], "go");
    m.insert(
        2,
        json!({"id": 3, "role": "user", "content": [{"type": "text", "text": "parts"}]}),
    );
    let d = run(&c, &r, &m, &Opts::default());
    assert!(c.rendered("t", 3).is_none());
    assert_eq!(d.messages[2]["content"], m[2]["content"]);
}

// K16
#[test]
fn arriving_then_stored() {
    let (c, r) = (cache(), Count::new("t"));
    run(&c, &r, &conv(&[(1, "a")], "hello"), &Opts::default());
    assert_eq!(
        ids_of(&c, "t"),
        vec![1],
        "the arriving message is not cached"
    );
    let d = run(
        &c,
        &r,
        &conv(&[(1, "a"), (2, "hello")], "next"),
        &Opts::default(),
    );
    assert_eq!(d.rows, 1, "rendered once under its id");
    assert_eq!(
        run(
            &c,
            &r,
            &conv(&[(1, "a"), (2, "hello")], "next"),
            &Opts::default()
        )
        .rows,
        0
    );
}

// K17
#[test]
fn in_progress_never_cached() {
    let (c, r) = (cache(), Count::new("t"));
    let mut m = conv(&[(1, "a"), (2, "b")], "go");
    // a message with no id in the middle
    m.insert(2, json!({"role": "user", "content": "no id"}));
    run(&c, &r, &m, &Opts::default());
    let size = c.stats();
    for _ in 0..3 {
        let before = r.calls();
        let d = run(&c, &r, &m, &Opts::default());
        assert_eq!(d.rows, 0);
        assert_eq!(
            r.calls() - before,
            3,
            "system, no-id and arriving rendered every time"
        );
        assert_eq!(c.stats().entries, size.entries);
        assert_eq!(c.stats().bytes, size.bytes);
    }
    // the re-ask pairs and a task pass's messages never reach the cache: they go through
    // `render` on their own (W6, W7), so the cache is the same after them
    let _ = r.render("a re-ask report");
    assert_eq!(c.stats().bytes, size.bytes);
}

// K18
#[test]
fn reply_batch_one_entry() {
    let c = cache();
    let batch = "[reply to 11 · notes write · ↑2 · 1 of 2 in batch 2]\n{\"ok\": true}\n\n[reply to 12 · kernel reflect · ↑2 · 2 of 2 in batch 2]\ncall NOT dispatched: refused. Nothing ran.";
    let m = conv(&[(1, "go"), (2, batch)], "next");
    run(&c, &Hermes, &m, &Opts::default());
    assert_eq!(
        ids_of(&c, "hermes"),
        vec![1, 2],
        "one entry for the batch row"
    );
    let d = run(&c, &Hermes, &m, &Opts::default());
    assert_eq!(d.rows, 0);
    assert_eq!(d.messages, plain(&Hermes, &m));
}

// K19
/// The default type keeps its rows like any type: mapped once, then hits; the bytes are
/// the stored texts (its renderer copies them).
#[test]
fn default_type_keeps_entries() {
    let c = cache();
    let m = conv(&refs(&rows_n(1, 6, 10)), "go");
    let first = run(&c, &Tags, &m, &Opts::default());
    assert_eq!(first.rows, 6);
    for _ in 0..2 {
        let d = run(&c, &Tags, &m, &Opts::default());
        assert_eq!(d.messages, plain(&Tags, &m));
        assert_eq!(d.messages, first.messages);
        assert_eq!(d.rows, 0);
    }
    for (i, (id, text)) in refs(&rows_n(1, 6, 10)).into_iter().enumerate() {
        assert_eq!(first.messages[i + 1]["content"], json!(text), "row {id}");
    }
    let s = c.stats();
    assert_eq!(s.entries, 6);
    assert_eq!(s.bytes, s.counted);
}

// ─── K20-K33 ─────────────────────────────────────────────────────────────────

// K20
#[test]
fn collision_falls_back_to_bytes() {
    let c: RenderCache<Zero> = RenderCache::new();
    let r = Count::new("t");
    run(&c, &r, &conv(&[(1, "first")], "go"), &Opts::default());
    let d = run(&c, &r, &conv(&[(1, "second")], "go"), &Opts::default());
    assert_eq!(d.rows, 1, "a miss by the byte compare");
    assert_eq!(d.messages[1]["content"], "<t>second</t>");
    assert_eq!(c.stats().collisions, 1);
    exact(&c);
}

// K21
#[test]
fn collision_equal_bytes_hits() {
    let c: RenderCache<Zero> = RenderCache::new();
    let r = Count::new("t");
    run(
        &c,
        &r,
        &conv(&[(1, "same"), (2, "other")], "go"),
        &Opts::default(),
    );
    let d = run(
        &c,
        &r,
        &conv(&[(1, "same"), (2, "other")], "go"),
        &Opts::default(),
    );
    assert_eq!(d.rows, 0);
    assert_eq!(c.stats().collisions, 0);
}

// K22
#[test]
fn lru_evicts_oldest() {
    let (c, r) = (cache(), Count::new("t"));
    let rows = rows_n(1, 8, 20);
    let one = entry_size(&rows[0].1, "t");
    let o = Opts::bound(4 * one);
    run(&c, &r, &conv(&refs(&rows), "go"), &o);
    assert_eq!(
        ids_of(&c, "t"),
        vec![5, 6, 7, 8],
        "the least recently used went first"
    );
    assert!(c.stats().bytes <= 4 * one);
    exact(&c);
}

// K23
#[test]
fn eviction_mid_pass() {
    let (c, r) = (cache(), Count::new("t"));
    let rows = rows_n(1, 6, 20);
    let one = entry_size(&rows[0].1, "t");
    let m = conv(&refs(&rows), "go");
    let v = open(&c, &r, &m, &Opts::bound(2 * one));
    let s = c.stats();
    assert_eq!(s.entries, 6, "every entry the pass uses is pinned");
    assert!(
        s.bytes > 2 * one,
        "the bound is exceeded until the pass ends"
    );
    assert_eq!(
        v.messages,
        plain(&Count::new("t"), &m),
        "the view is complete"
    );
    finish(v);
    assert!(c.stats().bytes <= 2 * one, "then met");
    exact(&c);
}

// K24
#[test]
fn pinned_never_evicted() {
    let c = cache();
    let (a, b, z) = (Count::new("a"), Count::new("b"), Count::new("z"));
    let one = entry_size("row 1 xx", "a");
    // the running pass holds rows 1 and 2: the least recently used entries
    let held = open(
        &c,
        &a,
        &conv(&[(1, "row 1 xx"), (2, "row 2 xx")], "go"),
        &Opts::default(),
    );
    run(
        &c,
        &b,
        &conv(&[(3, "row 3 xx"), (4, "row 4 xx")], "go"),
        &Opts::default(),
    );
    // a render over the bound: the pinned ones are skipped, the next unpinned go
    run(
        &c,
        &z,
        &conv(&[(5, "row 5 xx"), (6, "row 6 xx")], "go"),
        &Opts::bound(4 * one),
    );
    assert_eq!(ids_of(&c, "a"), vec![1, 2]);
    assert!(ids_of(&c, "b").is_empty());
    assert_eq!(ids_of(&c, "z"), vec![5, 6]);
    drop(held);
    assert_eq!(c.stats().pins, 0);
}

// K25
#[test]
fn evicted_rerenders_identical() {
    let c = cache();
    let (a, b) = (Count::new("a"), Count::new("b"));
    let m = conv(&refs(&rows_n(1, 4, 30)), "go");
    let first = run(&c, &a, &m, &Opts::default());
    run(&c, &b, &m, &Opts::bound(1));
    assert!(ids_of(&c, "a").is_empty(), "evicted");
    let again = run(&c, &a, &m, &Opts::default());
    assert_eq!(again.rows, 4, "rendered again");
    assert_eq!(again.messages, first.messages);
}

// K26
#[test]
fn old_type_evicted_first() {
    let c = cache();
    let (a, b) = (Count::new("a"), Count::new("b"));
    let rows = rows_n(1, 4, 10);
    let m = conv(&refs(&rows), "go");
    run(&c, &a, &m, &Opts::default());
    let fits = 4 * entry_size(&rows[0].1, "b");
    run(&c, &b, &m, &Opts::bound(fits));
    assert!(
        ids_of(&c, "a").is_empty(),
        "the unused type's entries were the oldest"
    );
    assert_eq!(ids_of(&c, "b"), vec![1, 2, 3, 4]);
}

// K27
#[test]
fn restart_cold_then_warm() {
    let m = conv(&refs(&rows_n(1, 5, 4)), "go");
    let before = cache();
    run(&before, &Hermes, &m, &Opts::default());
    let pre = run(&before, &Hermes, &m, &Opts::default());
    drop(before);
    let after = cache();
    let cold = run(&after, &Hermes, &m, &Opts::default());
    assert!(cold.cold);
    assert_eq!(cold.rows, 5);
    let warm = run(&after, &Hermes, &m, &Opts::default());
    assert_eq!(warm.rows, 0);
    assert_eq!(cold.messages, pre.messages);
    assert_eq!(warm.messages, pre.messages);
}

// K28
#[test]
fn epoch_change_resets() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&refs(&rows_n(1, 3, 0)), "go");
    let base_decl = decl_of(&r);
    let base: Vec<&Decl<'_>> = vec![&base_decl];
    let e1 = epoch_of(b"[rules]", &base);
    let once = |e: Hash| {
        run(
            &c,
            &r,
            &m,
            &Opts {
                epoch: e,
                ..Opts::default()
            },
        )
    };
    once(e1);
    assert_eq!(once(e1).rows, 0);
    // the rule bytes change
    let e2 = epoch_of(b"[other rules]", &base);
    let d = once(e2);
    assert!(d.cold);
    assert_eq!((d.rows, d.kept), (3, 0), "every entry and chain cleared");
    // an adapter's rev changes
    let bumped = Count {
        rev: 2,
        ..Count::new("t")
    };
    let e3 = epoch_of(b"[other rules]", &[&decl_of(&bumped)]);
    assert_ne!(e2, e3);
    let d = once(e3);
    assert_eq!((d.rows, d.kept), (3, 0));
    assert_eq!(c.stats().chains, 1);
}

/// `history.json` through the cache for `r`, in the prefix files' layout.
fn cached_history<H: Hasher>(c: &RenderCache<H>, r: &dyn Renderer) -> String {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("res/test/history.json")).expect("history.json");
    let rows: Vec<Value> = rows
        .into_iter()
        .filter(|r| r.get("content").is_some_and(Value::is_string))
        .collect();
    let o = Opts::default();
    let v = c.view(&decl_of(r), "ai", &shown_of(&rows, &o), &o.ctx());
    let mut out = String::new();
    for (row, m) in rows.iter().zip(&v.messages) {
        let id = row.get("id").map_or_else(String::new, Value::to_string);
        let role = row.get("role").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!("== row {id} {role} ==\n"));
        out.push_str(m["content"].as_str().unwrap_or(""));
        out.push('\n');
    }
    finish(v);
    out
}

// K29
#[test]
fn render_twice_identical() {
    for r in [&Tags as &dyn Renderer, &Hermes] {
        let c = cache();
        let first = cached_history(&c, r);
        assert_eq!(cached_history(&c, r), first, "{}: warm", r.name());
        assert_eq!(
            cached_history(&cache(), r),
            first,
            "{}: another cache",
            r.name()
        );
        assert_eq!(
            first,
            super::tests_determinism::rendered_history(r),
            "{}: uncached",
            r.name()
        );
    }
}

// K29
#[test]
fn recorded_prefix() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/model_format/res/test");
    for (r, file) in [
        (&Tags as &dyn Renderer, "prefix_d.txt"),
        (&Hermes, "prefix_h.txt"),
    ] {
        let text = std::fs::read_to_string(dir.join(file)).unwrap_or_else(|e| {
            panic!("{file}: recorded by W4's last step (section 3, Fixtures): {e}")
        });
        let (rev, digest, body) = super::tests_determinism::recorded(&text);
        assert_eq!(rev, r.rev(), "{file}: recorded under another rev");
        assert_eq!(
            digest,
            super::tests_determinism::rules_digest(),
            "{file}: another rule file"
        );
        assert_eq!(cached_history(&cache(), r), body, "{file}");
    }
}

// K30
#[test]
fn marks_do_not_touch_cache() {
    let (c, r) = (cache(), Count::new("t"));
    let pic = json!([{"type": "image", "ref": 3}]);
    let marked = |at: usize| {
        let mut m = conv(&refs(&rows_n(1, 4, 0)), "go");
        m[3]["images"] = pic.clone();
        m[at][crate::caching::CACHE_MARK] = json!(true);
        m
    };
    let a = run(&c, &r, &marked(2), &Opts::default());
    let s = c.stats();
    let b = run(&c, &r, &marked(4), &Opts::default());
    assert_eq!(b.rows, 0);
    assert!(b.dropped.is_empty());
    assert_eq!(a.links, b.links, "no link changes");
    assert_eq!(b.kept, b.links.len());
    assert_eq!(c.stats().keys, s.keys);
    assert_eq!(c.stats().bytes, s.bytes);
    assert_eq!(b.messages[4][crate::caching::CACHE_MARK], json!(true));
    assert!(b.messages[2].get(crate::caching::CACHE_MARK).is_none());
    assert_eq!(b.messages[3]["images"], pic);
    assert!(
        b.messages.iter().all(|m| m.get("id").is_none()),
        "no id is sent"
    );
}

/// Renders `drop me` to nothing, anything else as `Count` does.
struct Empties(Count);

impl Renderer for Empties {
    fn name(&self) -> &'static str {
        "e"
    }
    fn rev(&self) -> u32 {
        1
    }
    fn render(&self, text: &str) -> String {
        if text == "drop me" {
            String::new()
        } else {
            self.0.render(text)
        }
    }
}

// K31
#[test]
fn empty_render_keeps_position() {
    let c = cache();
    let r = Empties(Count::new("e"));
    let m = conv(&[(1, "a"), (2, "drop me"), (3, "c")], "go");
    for _ in 0..2 {
        let d = run(&c, &r, &m, &Opts::default());
        assert_eq!(d.messages.len(), m.len());
        assert_eq!(d.messages[2]["content"], "");
        assert_eq!(d.messages[2]["role"], "assistant");
        assert_eq!(d.messages[3]["content"], "<e>c</e>");
    }
}

// K32
#[test]
fn two_threads_one_cache() {
    let c = cache();
    let r = Count::new("t");
    let m = conv(&refs(&rows_n(1, 40, 8)), "go");
    let want = plain(&Count::new("t"), &m);
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                for _ in 0..20 {
                    let d = run(&c, &r, &m, &Opts::default());
                    assert_eq!(d.messages, want);
                }
            });
        }
    });
    let st = c.stats();
    assert_eq!(st.entries, 40, "one entry per key");
    assert_eq!(st.bytes, st.counted);
    assert_eq!(st.pins, 0);
    // both miss one key and both insert: every render waits for the other thread's, so
    // both lookups of row 1 miss before either inserts; the second replaces the first
    let fresh = cache();
    let gate = Gate(std::sync::Barrier::new(2), Count::new("t"));
    let one = conv(&[(1, "only row")], "go");
    let want = plain(&Count::new("t"), &one);
    std::thread::scope(|s| {
        for _ in 0..2 {
            s.spawn(|| {
                let d = run(&fresh, &gate, &one, &Opts::default());
                assert_eq!(d.rows, 1, "a miss on each thread");
                assert_eq!(d.messages, want);
            });
        }
    });
    let st = fresh.stats();
    assert_eq!((st.entries, st.bytes, st.pins), (1, st.counted, 0));
    assert_eq!(st.bytes, entry_size("only row", "t"));
}

/// A renderer whose every call waits for the other thread's (K32).
struct Gate(std::sync::Barrier, Count);

impl Renderer for Gate {
    fn name(&self) -> &'static str {
        "t"
    }
    fn rev(&self) -> u32 {
        1
    }
    fn render(&self, text: &str) -> String {
        self.0.wait();
        self.1.render(text)
    }
}

// K33
#[test]
fn two_chats_one_provider() {
    let (a, b) = (cache(), cache());
    let r = Count::new("t");
    let m = conv(&[(1, "one"), (2, "two")], "go");
    assert_eq!(run(&a, &r, &m, &Opts::default()).rows, 2);
    assert_eq!(run(&b, &r, &m, &Opts::default()).rows, 2, "two caches");
    let other = conv(&[(1, "uno"), (2, "dos")], "go");
    let da = run(&a, &r, &other, &Opts::default());
    assert_eq!(da.messages[1]["content"], "<t>uno</t>");
    let db = run(&b, &r, &m, &Opts::default());
    assert_eq!(db.rows, 0, "b is not served a's bytes");
    assert_eq!(db.messages[1]["content"], "<t>one</t>");
    // an eviction and a reset in a change nothing in b
    let sb = b.stats();
    run(&a, &r, &other, &Opts::bound(1));
    run(
        &a,
        &r,
        &other,
        &Opts {
            epoch: [7; 32],
            ..Opts::default()
        },
    );
    assert_eq!(b.stats(), sb);
    assert_eq!(run(&b, &r, &m, &Opts::default()).rows, 0);
}

// ─── K39-K53 ─────────────────────────────────────────────────────────────────

// K39
#[test]
fn byte_count_exact() {
    let c = cache();
    let (a, b) = (Count::new("a"), Count::new("b"));
    let rows = rows_n(1, 10, 7);
    run(&c, &a, &conv(&refs(&rows), "go"), &Opts::default());
    exact(&c);
    let mut changed = rows.clone();
    changed[3].1 = "[folded]".into();
    run(
        &c,
        &a,
        &conv(&refs(&changed), "go"),
        &Opts::default().form(4, "anchor"),
    );
    exact(&c);
    run(&c, &a, &conv(&refs(&changed[5..]), "go"), &Opts::default());
    exact(&c);
    run(&c, &b, &conv(&refs(&rows), "go"), &Opts::bound(200));
    exact(&c);
    assert!(c.stats().bytes <= 200);
}

// K40
#[test]
fn replaced_entry_not_stale() {
    let (c, r) = (cache(), Count::new("t"));
    for (text, form) in [
        ("[anchor]", "anchor"),
        ("the whole", "whole"),
        ("[anchor]", "anchor"),
    ] {
        let m = conv(&[(1, "q"), (2, text)], "go");
        let d = run(&c, &r, &m, &Opts::default().form(2, form));
        assert_eq!(d.messages, plain(&Count::new("t"), &m));
        assert_eq!(c.rendered("t", 2), Some(format!("<t>{text}</t>")));
    }
}

// K41
#[test]
fn pins_released_on_drop() {
    // cancelled, failed on the link, ended by the stop: each drops the view unfinished
    for why in ["cancel", "link", "stop"] {
        let (c, r) = (cache(), Count::new("t"));
        let m = conv(&refs(&rows_n(1, 5, 0)), "go");
        let v = open(&c, &r, &m, &Opts::bound(1));
        assert_eq!(c.stats().pins, 5, "{why}");
        drop(v);
        assert_eq!(c.stats().pins, 0, "{why}");
        assert_eq!(c.stats().entries, 0, "{why}: every entry is evictable");
    }
    // a pass whose task panics drops its view on the way out
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&refs(&rows_n(1, 3, 0)), "go");
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _v = open(&c, &r, &m, &Opts::default());
        panic!("the pass failed");
    }));
    assert_eq!(c.stats().pins, 0);
}

// K42
#[test]
fn why_table() {
    // the table of 1.6, row by row
    assert_eq!(why(Some("whole"), Some("anchor")), Why::Fold, "rank down");
    assert_eq!(
        why(Some("image"), Some("image_anchor")),
        Why::Fold,
        "full -> line"
    );
    assert_eq!(
        why(Some("image_anchor"), Some("image_alt")),
        Why::Fold,
        "line -> reduced"
    );
    assert_eq!(why(Some("anchor"), Some("whole")), Why::Recall, "rank up");
    assert_eq!(
        why(Some("image_alt"), Some("image_anchor")),
        Why::Recall,
        "reduced -> line"
    );
    assert_eq!(
        why(Some("anchor"), Some("stub")),
        Why::Fold,
        "reduced -> reduced"
    );
    assert_eq!(
        why(Some("whole"), Some("recalled")),
        Why::Recall,
        "whole -> recalled"
    );
    assert_eq!(
        why(Some("whole"), Some("whole")),
        Why::Changed,
        "full -> full, no recall"
    );
    assert_eq!(
        why(Some("thought"), Some("whole")),
        Why::Changed,
        "a thought host is whole"
    );
    // equal bytes under two forms: a hit, no drop
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&[(1, "q"), (2, "same text")], "go");
    run(&c, &r, &m, &Opts::default().form(2, "whole"));
    let d = run(&c, &r, &m, &Opts::default().form(2, "anchor"));
    assert_eq!((d.rows, d.dropped.len()), (0, 0));
    // each drop row through the cache
    let steps: [(&str, &str, Option<Why>); 6] = [
        ("whole", "a", None),
        ("anchor", "b", Some(Why::Fold)),
        ("stub", "c", Some(Why::Fold)),
        ("whole", "d", Some(Why::Recall)),
        ("recalled", "e", Some(Why::Recall)),
        ("whole", "f", Some(Why::Changed)),
    ];
    let (c, r) = (cache(), Count::new("t"));
    for (form, text, want) in steps {
        let d = run(
            &c,
            &r,
            &conv(&[(1, "q"), (2, text)], "go"),
            &Opts::default().form(2, form),
        );
        let got: Vec<Why> = d.dropped.iter().map(|(_, w)| *w).collect();
        assert_eq!(got, want.into_iter().collect::<Vec<_>>(), "{form} {text}");
    }
}

// K43
#[test]
fn trimmed_row_is_fold() {
    let (c, r) = (cache(), Count::new("t"));
    let long = "line\n".repeat(200);
    run(
        &c,
        &r,
        &conv(&[(1, "q"), (2, &long)], "go"),
        &Opts::default().form(2, "whole"),
    );
    // the trimmer cut the body down: the assembly relabels it `anchor`
    let d = run(
        &c,
        &r,
        &conv(&[(1, "q"), (2, &long[..40])], "go"),
        &Opts::default().form(2, "anchor"),
    );
    assert_eq!(d.dropped, vec![(2, Why::Fold)]);
    assert_eq!(d.rows, 1);
}

// K44
#[test]
fn bound_is_per_chat() {
    let (a, b) = (cache(), cache());
    let r = Count::new("t");
    let rows = rows_n(1, 4, 20);
    let one = entry_size(&rows[0].1, "t");
    let ma = conv(&refs(&rows), "go");
    run(&a, &r, &ma, &Opts::bound(4 * one));
    let sa = a.stats();
    let big = rows_n(1, 20, 20);
    run(&b, &r, &conv(&refs(&big), "go"), &Opts::bound(3 * one));
    assert_eq!(a.stats(), sa, "a loses no entry");
    assert_eq!(run(&a, &r, &ma, &Opts::bound(4 * one)).rows, 0);
    assert!(a.stats().bytes <= 4 * one);
    assert!(b.stats().bytes <= 3 * one);
}

// K45
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_chat_frees_cache() {
    let r = Count::new("t");
    let m = conv(&refs(&rows_n(1, 3, 0)), "go");
    let other = fantastic_kernel::AgentId::from("k45_other");
    run(
        &crate::state::state_for(&other).render_cache,
        &r,
        &m,
        &Opts::default(),
    );
    let other_stats = crate::state::state_for(&other).render_cache.stats();

    // dropped while a pass holds its guard: freed once the pass has ended
    let id = fantastic_kernel::AgentId::from("k45_dropped");
    let state = crate::state::state_for(&id);
    let weak = Arc::downgrade(&state);
    run(&state.render_cache, &r, &m, &Opts::default());
    let v = open(&state.render_cache, &r, &m, &Opts::default());
    crate::state::drop_state(&id);
    assert!(weak.upgrade().is_some(), "the running pass still holds it");
    let d = finish(v);
    assert_eq!(d.rows, 0, "the running pass ends without a panic");
    drop(state);
    assert!(weak.upgrade().is_none(), "the cache went with the state");

    // deleted
    let tmp = tempfile::TempDir::new().unwrap();
    let (kernel, agent) = crate::tests::mk_kernel(&tmp, "k45_del").await;
    let weak = {
        let state = crate::state::state_for(&agent);
        run(&state.render_cache, &r, &m, &Opts::default());
        Arc::downgrade(&state)
    };
    let out = kernel
        .operator_send(vec![fantastic_kernel::Item::new(
            fantastic_kernel::AgentId::from("kernel"),
            json!({"type": "delete_agent", "id": agent.as_str()}),
        )])
        .await
        .remove(0)
        .result;
    assert_eq!(out["deleted"], true, "{out}");
    assert!(weak.upgrade().is_none(), "deleted: freed");
    assert_eq!(
        crate::state::state_for(&other).render_cache.stats(),
        other_stats
    );
    crate::state::drop_state(&other);
}

// K46
#[test]
fn two_chats_concurrent() {
    let (a, b) = (cache(), cache());
    let (ra, rb) = (Count::new("a"), Count::new("b"));
    let ma = conv(&refs(&rows_n(1, 30, 5)), "go");
    let mb = conv(&refs(&rows_n(100, 30, 9)), "go");
    let (wa, wb) = (plain(&Count::new("a"), &ma), plain(&Count::new("b"), &mb));
    let one = entry_size(&rows_n(1, 1, 9)[0].1, "b");
    std::thread::scope(|s| {
        s.spawn(|| {
            for i in 0..30 {
                let o = Opts::bound(if i % 2 == 0 { usize::MAX } else { 10 * one });
                assert_eq!(run(&a, &ra, &ma, &o).messages, wa);
            }
        });
        s.spawn(|| {
            for i in 0..30 {
                let o = Opts::bound(if i % 3 == 0 { usize::MAX } else { 7 * one });
                assert_eq!(run(&b, &rb, &mb, &o).messages, wb);
            }
        });
    });
    for c in [&a, &b] {
        let s = c.stats();
        assert_eq!((s.bytes, s.pins), (s.counted, 0));
    }
}

// K47
#[test]
fn bounds_add_up() {
    const BOUND: usize = 64 * 1024;
    let caches: Vec<RenderCache> = (0..8).map(|_| cache()).collect();
    let r = Count::new("t");
    let m = conv(&refs(&rows_n(1, 100, 1000)), "go");
    for c in &caches {
        run(c, &r, &m, &Opts::bound(BOUND));
    }
    let mut total = 0;
    for c in &caches {
        let s = c.stats();
        assert!(s.bytes <= BOUND, "{}", s.bytes);
        assert!(s.bytes > BOUND / 2, "each chat fills its own bound");
        total += s.bytes;
    }
    assert!(total <= 8 * BOUND);
    assert!(total > BOUND, "the total grows with the number of chats");
}

// K48
#[test]
fn bound_default_is_256_mib() {
    assert_eq!(crate::constants::RENDER_CACHE_BYTES, 268_435_456);
    assert_eq!(bound_of(&Map::new()), crate::constants::RENDER_CACHE_BYTES);
    let mut meta = Map::new();
    meta.insert("render_cache_bytes".into(), Value::Null);
    assert_eq!(bound_of(&meta), crate::constants::RENDER_CACHE_BYTES);
}

const RECORD_YAML: &str = "base: ai_chat.tools\nsentence: A chat with a small render cache.\nrecord:\n  render_cache_bytes: 1000\n";

/// A kernel with the chat bundle and a meta bundle over it whose record sets the bound.
async fn chat_kernel(tmp: &tempfile::TempDir, meta: &str) -> Arc<fantastic_kernel::Kernel> {
    let meta = meta.to_string();
    crate::tests::base_kernel(tmp, move |k| {
        k.bundles.register(
            crate::chat_bundle::HANDLER_MODULE,
            crate::chat_bundle::AiChatBundle,
        );
        k.bundles
            .register_meta_yaml(&meta, RECORD_YAML)
            .expect("meta bundle");
    })
    .await
}

async fn kernel_state(kernel: &Arc<fantastic_kernel::Kernel>, payload: Value) -> Value {
    kernel
        .operator_send(vec![fantastic_kernel::Item::new(
            fantastic_kernel::AgentId::from("kernel_state"),
            payload,
        )])
        .await
        .remove(0)
        .result
}

async fn make_chat(
    kernel: &Arc<fantastic_kernel::Kernel>,
    id: &str,
    module: &str,
    bound: Option<i64>,
) {
    let mut create =
        json!({"type": "create_agent", "parent_id": "kernel", "handler_module": module, "id": id});
    if let Some(b) = bound {
        create["render_cache_bytes"] = json!(b);
    }
    let made = kernel_state(kernel, create).await;
    assert_eq!(made["id"], id, "{made}");
}

fn bound_on(kernel: &Arc<fantastic_kernel::Kernel>, id: &str) -> usize {
    bound_of(&crate::helpers::agent_meta(
        &fantastic_kernel::AgentId::from(id),
        kernel,
    ))
}

// K49
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_override_per_chat() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = chat_kernel(&tmp, "k49_small_chat").await;
    make_chat(&kernel, "k49_none", "ai_chat.tools", None).await;
    make_chat(&kernel, "k49_own", "ai_chat.tools", Some(5000)).await;
    make_chat(&kernel, "k49_bundle", "k49_small_chat", None).await;
    make_chat(&kernel, "k49_both", "k49_small_chat", Some(7000)).await;
    make_chat(&kernel, "k49_other", "ai_chat.tools", Some(9000)).await;
    assert_eq!(
        bound_on(&kernel, "k49_none"),
        crate::constants::RENDER_CACHE_BYTES
    );
    assert_eq!(bound_on(&kernel, "k49_own"), 5000, "the chat's own record");
    assert_eq!(
        bound_on(&kernel, "k49_bundle"),
        1000,
        "the bundle's when the record has none"
    );
    assert_eq!(
        bound_on(&kernel, "k49_both"),
        7000,
        "the record's when both"
    );
    assert_eq!(
        bound_on(&kernel, "k49_other"),
        9000,
        "neither takes the other's"
    );
}

// K50
#[test]
fn small_override_lru_and_rerender() {
    let mut meta = Map::new();
    meta.insert("render_cache_bytes".into(), json!(65_536));
    let bound = bound_of(&meta);
    assert_eq!(bound, 65_536);
    let c = cache();
    let r = Count::new("t");
    let rows = rows_n(1, 32, 4000);
    let m = conv(&refs(&rows), "go");
    // mid-pass: pinned entries stay, the bound is exceeded until the pass ends
    let v = open(&c, &r, &m, &Opts::bound(bound));
    assert_eq!(c.stats().entries, 32);
    assert!(c.stats().bytes > bound);
    let first = finish(v);
    assert!(c.stats().bytes <= bound, "then met");
    // the least recently used went first: the newest rows stay
    let ids = ids_of(&c, "t");
    assert!(!ids.is_empty() && ids.windows(2).all(|w| w[1] == w[0] + 1));
    assert_eq!(*ids.last().unwrap(), 32);
    // an evicted row renders again, byte-equal
    let again = run(&c, &r, &m, &Opts::bound(bound));
    assert_eq!(again.rows, 32 - ids.len());
    assert_eq!(again.messages, first.messages);
    assert_eq!(again.messages, plain(&Count::new("t"), &m));
}

// K51
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_bad_value_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = chat_kernel(&tmp, "k51_meta").await;
    make_chat(&kernel, "k51_chat", "ai_chat.tools", Some(4096)).await;
    for (i, bad) in [json!(0), json!(-5), json!(1.5), json!("big"), json!([1])]
        .into_iter()
        .enumerate()
    {
        let r = kernel_state(
            &kernel,
            json!({"type": "update_agent", "id": "k51_chat", "render_cache_bytes": bad}),
        )
        .await;
        assert!(r.get("error").is_some(), "{bad}: refused: {r}");
        assert!(
            r.to_string().contains("render_cache_bytes"),
            "{bad}: names the field: {r}"
        );
        assert_eq!(
            crate::helpers::agent_meta(&fantastic_kernel::AgentId::from("k51_chat"), &kernel)
                ["render_cache_bytes"],
            json!(4096),
            "{bad}: the record is unchanged"
        );
        assert_eq!(bound_on(&kernel, "k51_chat"), 4096);
        let made = kernel_state(
            &kernel,
            json!({"type": "create_agent", "parent_id": "kernel", "handler_module": "ai_chat.tools",
                   "id": format!("k51_new_{i}"), "render_cache_bytes": bad}),
        )
        .await;
        assert!(
            made.get("error").is_some(),
            "{bad}: a create is refused too: {made}"
        );
    }
    let r = kernel_state(
        &kernel,
        json!({"type": "update_agent", "id": "k51_chat", "render_cache_bytes": null}),
    )
    .await;
    assert!(r.get("error").is_none(), "null is taken: {r}");
    assert_eq!(
        bound_on(&kernel, "k51_chat"),
        crate::constants::RENDER_CACHE_BYTES,
        "null is the default"
    );
}

// K52
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bound_follows_the_record() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kernel = chat_kernel(&tmp, "k52_meta").await;
    make_chat(&kernel, "k52_a", "ai_chat.tools", Some(65_536)).await;
    make_chat(&kernel, "k52_b", "ai_chat.tools", Some(32_768)).await;
    let (a, b) = (cache(), cache());
    let r = Count::new("t");
    let m = conv(&refs(&rows_n(1, 24, 2000)), "go");
    let want = plain(&Count::new("t"), &m);
    let pass = |c: &RenderCache, id: &str| run(c, &r, &m, &Opts::bound(bound_on(&kernel, id)));
    assert_eq!(pass(&a, "k52_a").messages, want);
    assert_eq!(pass(&b, "k52_b").messages, want);
    let held = a.stats().entries;
    let set = |v: i64| {
        kernel_state(
            &kernel,
            json!({"type": "update_agent", "id": "k52_a", "render_cache_bytes": v}),
        )
    };
    // raised: nothing is evicted
    assert!(set(1 << 20).await.get("error").is_none());
    let d = pass(&a, "k52_a");
    assert_eq!(d.messages, want);
    assert!(a.stats().entries >= held, "raised: nothing evicted");
    assert_eq!(a.stats().entries, 24);
    // lowered: least recently used first, down to it
    assert!(set(16_384).await.get("error").is_none());
    let d = pass(&a, "k52_a");
    assert_eq!(d.messages, want);
    assert!(a.stats().bytes <= 16_384);
    assert_eq!(*ids_of(&a, "t").last().unwrap(), 24, "the newest stay");
    // the other chat's bound did not change
    assert_eq!(bound_on(&kernel, "k52_b"), 32_768);
    assert_eq!(pass(&b, "k52_b").messages, want);
    assert!(b.stats().bytes <= 32_768);
}

// K53
#[test]
fn change_misses_links_not_entries() {
    let rows = rows_n(1, 12, 3);
    for p in 1..=12 {
        let (c, r) = (cache(), Count::new("t"));
        let before = run(&c, &r, &conv(&refs(&rows), "go"), &Opts::default());
        let mut changed = rows.clone();
        changed[p - 1].1 = format!("changed {p}");
        let after = run(&c, &r, &conv(&refs(&changed), "go"), &Opts::default());
        assert_eq!(after.rows, 1, "p={p}: only the changed row renders");
        assert_eq!(after.kept, p, "p={p}");
        for i in 0..before.links.len() {
            if i < p {
                assert_eq!(before.links[i], after.links[i], "p={p} i={i}");
            } else {
                assert_ne!(before.links[i], after.links[i], "p={p} i={i}");
            }
            if i != p {
                assert_eq!(
                    before.messages[i], after.messages[i],
                    "p={p} i={i}: equal bytes"
                );
            }
        }
    }
}

#[test]
fn sha_is_the_cache_hasher() {
    use sha2::Digest as _;
    let want: Hash = sha2::Sha256::digest(b"x").into();
    assert_eq!(<Sha as Hasher>::digest(b"x"), want);
}

// ─── K34, K35: the cache under the pass path (W6) ───────────────────────────

/// A provider that answers every request with one canned text, reports what a test
/// sets, and logs what it was sent.
pub(super) struct Canned {
    answer: String,
    pub(super) report: std::sync::Mutex<crate::provider::ProviderReport>,
    asked: std::sync::Mutex<Vec<Vec<Value>>>,
}

impl Canned {
    pub(super) fn new(answer: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.to_string(),
            report: std::sync::Mutex::new(crate::provider::ProviderReport::default()),
            asked: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub(super) fn asked(&self) -> Vec<Vec<Value>> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl crate::provider::Provider for Canned {
    async fn chat(
        &self,
        messages: &[Value],
        _max_tokens: Option<i64>,
    ) -> Result<crate::provider::ProviderStream, crate::provider::ProviderError> {
        self.asked.lock().unwrap().push(messages.to_vec());
        let ev: Vec<Result<crate::provider::ProviderEvent, crate::provider::ProviderError>> =
            vec![Ok(crate::provider::ProviderEvent::Token(
                self.answer.clone(),
            ))];
        Ok(Box::pin(futures_util::stream::iter(ev)))
    }
    fn model(&self) -> String {
        "qwen3-8b".to_string()
    }
    fn report(&self) -> crate::provider::ProviderReport {
        self.report.lock().unwrap().clone()
    }
}

/// A hermes pass over `provider` on the chat `chat`'s own state.
pub(super) fn hermes_pass(
    provider: Arc<Canned>,
    chat: &str,
) -> (super::pass::Pass, Arc<crate::state::BackendState>) {
    let state = crate::state::state_for(&fantastic_kernel::AgentId::from(chat));
    let pass = super::pass::Pass::typed_for_test(
        provider,
        Arc::clone(&state),
        super::hermes::NAME,
        Some("qwen3-8b"),
    );
    (pass, state)
}

/// One turn's view on `pass`, built as `run_generation` builds it, run to its end.
pub(super) fn turn_of(pass: &super::pass::Pass, messages: &[Value]) -> Done {
    finish(pass.view(messages, &Map::new()))
}

/// A history with calls in it: what a fold summary of it sends holds them too.
fn called_rows(n: i64) -> Vec<(i64, String)> {
    let mut args = Map::new();
    args.insert("target_id".into(), json!("probe"));
    args.insert("payload".into(), json!({"type": "say", "text": "hi"}));
    let call = crate::tool_parse::render_call("send", &args);
    (1..=n)
        .map(|i| {
            let text = if i % 2 == 0 {
                format!("step {i}\n{call}")
            } else {
                format!("[reply to probe say]\nok {i}")
            };
            (i, text)
        })
        .collect()
}

// K34
/// A fold summary inside a turn is a task pass: it inserts, drops and evicts nothing,
/// and the turn after it renders what it would have with no task pass.
#[tokio::test]
async fn task_pass_leaves_cache() {
    let rows = called_rows(8);
    let first = conv(&refs(&rows), "go");
    let more = called_rows(9);
    let second = conv(&refs(&more), "next");

    let provider = Canned::new("TITLE\n\nthe summary");
    let (pass, state) = hermes_pass(provider.clone(), "k34_task");
    let a = turn_of(&pass, &first);
    assert!(a.cold && a.rows == rows.len());
    let before = state.render_cache.stats();
    let span: Vec<Value> = first[1..first.len() - 1].to_vec();
    let (summary, _) = crate::strategies::summarize(&pass, &span, None).await;
    assert_eq!(summary, "TITLE\n\nthe summary");
    assert_eq!(provider.asked().len(), 1, "one task request");
    assert_eq!(
        state.render_cache.stats(),
        before,
        "the task pass inserts, drops and evicts nothing"
    );
    let b = turn_of(&pass, &second);

    // the same two turns with no task pass between them
    let (control, _) = hermes_pass(Canned::new("x"), "k34_control");
    let _ = turn_of(&control, &first);
    let c = turn_of(&control, &second);
    assert_eq!(b.rows, 1, "the turn's warm count: its one new row");
    assert_eq!(
        (b.rows, b.kept, b.cold, &b.dropped),
        (c.rows, c.kept, c.cold, &c.dropped)
    );
    assert_eq!(b.messages, c.messages);
    for chat in ["k34_task", "k34_control"] {
        crate::state::drop_state(&fantastic_kernel::AgentId::from(chat));
    }
}

// K35
/// A chat runs one pass at a time (#795: the next pass starts once the one in flight
/// ended): the second pass's view is built only after the first pass ended (its pins
/// released, its chain kept), and both views equal the two passes run one after the
/// other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_turn_serial() {
    let first = conv(&refs(&called_rows(6)), "go");
    let second = conv(&refs(&called_rows(8)), "next");

    let (pass, state) = hermes_pass(Canned::new("ok"), "k35_queued");
    let pass = Arc::new(pass);
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    let (go_tx, go_rx) = tokio::sync::oneshot::channel::<()>();
    let (ended_tx, ended_rx) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn({
        let (pass, first) = (Arc::clone(&pass), first.clone());
        async move {
            let view = pass.view(&first, &Map::new());
            held_tx.send(()).unwrap();
            go_rx.await.unwrap();
            let out = finish(view);
            ended_tx.send(()).unwrap();
            out
        }
    });
    held_rx.await.unwrap();
    let queued = tokio::spawn({
        let (pass, state, second) = (Arc::clone(&pass), Arc::clone(&state), second.clone());
        async move {
            // the next pass starts once the one in flight ended
            ended_rx.await.unwrap();
            let at_start = state.render_cache.stats();
            (at_start, turn_of(&pass, &second))
        }
    });
    assert!(
        state.render_cache.stats().pins > 0,
        "the running pass holds its pins"
    );
    go_tx.send(()).unwrap();
    let a = running.await.unwrap();
    let (at_start, b) = queued.await.unwrap();
    assert_eq!(at_start.passes, 1, "the queued pass began after the first");
    assert_eq!(at_start.pins, 0, "the first pass had ended");
    assert_eq!(state.render_cache.stats().pins, 0);

    let (serial, _) = hermes_pass(Canned::new("ok"), "k35_serial");
    let sa = turn_of(&serial, &first);
    let sb = turn_of(&serial, &second);
    assert_eq!(a.messages, sa.messages);
    assert_eq!(b.messages, sb.messages);
    assert_eq!((b.rows, b.kept, b.cold), (sb.rows, sb.kept, sb.cold));
    assert_eq!((a.rows, a.kept, a.cold), (sa.rows, sa.kept, sa.cold));
    for chat in ["k35_queued", "k35_serial"] {
        crate::state::drop_state(&fantastic_kernel::AgentId::from(chat));
    }
}
