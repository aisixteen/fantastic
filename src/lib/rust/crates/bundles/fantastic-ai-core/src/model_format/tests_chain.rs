//! The render cache's chain (#766, W5, W6): C1-C15 of spec 2.3.
//!
//! `kept` is the number of leading messages whose links equal the kept chain's (the
//! type's last finished pass); `kept_endpoint` is `kept`, or 0 when the endpoint's own
//! cache cannot hold it. C15 reads the endpoint's report through the pass path (W6).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use super::cache::RenderCache;
use super::default::Tags;
use super::hermes::Hermes;
use super::tests_cache::{cache, conv, finish, open, refs, rows_n, run, Count, Opts};
use super::Renderer;

fn model(m: &str) -> Opts {
    Opts {
        model: Some(m.into()),
        ..Opts::default()
    }
}

// C1
#[test]
fn append_keeps_prefix() {
    let (c, r) = (cache(), Count::new("t"));
    let first = conv(&[(1, "a"), (2, "b"), (3, "c")], "d");
    let a = run(&c, &r, &first, &Opts::default());
    assert_eq!(a.kept, 0, "nothing kept before the first pass");
    let b = run(
        &c,
        &r,
        &conv(&[(1, "a"), (2, "b"), (3, "c"), (4, "d")], "e"),
        &Opts::default(),
    );
    assert_eq!(
        b.kept,
        first.len() - 1,
        "the previous pass's length minus its arriving message"
    );
}

// C2
#[test]
fn change_at_any_position() {
    let base = conv(&refs(&rows_n(1, 10, 2)), "go");
    assert_eq!(base.len(), 12);
    for p in 0..12 {
        let (c, r) = (cache(), Count::new("t"));
        let before = run(&c, &r, &base, &Opts::default());
        let mut changed = base.clone();
        changed[p]["content"] = serde_json::json!(format!("changed at {p}"));
        let after = run(&c, &r, &changed, &Opts::default());
        assert_eq!(after.kept, p, "p={p}");
        for i in 0..12 {
            assert_eq!(before.links[i] == after.links[i], i < p, "p={p} i={i}");
        }
    }
}

// C3
#[test]
fn same_text_different_links() {
    let (c, r) = (cache(), Count::new("t"));
    let d = run(
        &c,
        &r,
        &conv(&[(1, "same"), (2, "same")], "same"),
        &Opts::default(),
    );
    assert_eq!(d.messages[1]["content"], d.messages[2]["content"]);
    assert_ne!(d.links[1], d.links[2]);
    assert_ne!(d.links[2], d.links[3]);
}

// C4
#[test]
fn types_do_not_share_links() {
    let m = conv(&refs(&rows_n(1, 6, 3)), "go");
    let c = cache();
    let pairs: [(&dyn Renderer, &dyn Renderer); 2] =
        [(&Count::new("a"), &Count::new("b")), (&Tags, &Hermes)];
    for (x, y) in pairs {
        let (dx, dy) = (
            run(&c, x, &m, &Opts::default()),
            run(&c, y, &m, &Opts::default()),
        );
        for (i, (a, b)) in dx.links.iter().zip(&dy.links).enumerate() {
            assert_ne!(a, b, "{} vs {}: link {i}", x.name(), y.name());
        }
    }
}

// C5
#[test]
fn same_type_same_links() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&refs(&rows_n(1, 5, 0)), "go");
    let a = run(&c, &r, &m, &model("qwen2.5-7b"));
    let b = run(&c, &r, &m, &model("qwen3-8b"));
    assert_eq!(a.links, b.links);
    assert_eq!(b.kept, m.len());
    assert_eq!(b.rows, 0);
}

// C6
#[test]
fn system_change_breaks_all() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&refs(&rows_n(1, 5, 0)), "go");
    run(&c, &r, &m, &Opts::default());
    let mut other = m.clone();
    other[0]["content"] = serde_json::json!("another system block");
    let d = run(&c, &r, &other, &Opts::default());
    assert_eq!(d.kept, 0);
    assert_eq!(d.rows, 0, "the rows are still render hits");
}

// C7
#[test]
fn fold_recall_cut_break_there() {
    let rows = rows_n(1, 6, 0);
    let all = refs(&rows);
    let folded: Vec<(i64, &str)> = all
        .iter()
        .map(|&(i, t)| (i, if i == 3 { "[folded]" } else { t }))
        .collect();
    let cut: Vec<(i64, &str)> = all.iter().copied().filter(|(i, _)| *i != 3).collect();
    // fold at position 3; then the recall back at position 3; a cut at position 3
    let (c, r) = (cache(), Count::new("t"));
    run(&c, &r, &conv(&all, "go"), &Opts::default());
    assert_eq!(
        run(
            &c,
            &r,
            &conv(&folded, "go"),
            &Opts::default().form(3, "anchor")
        )
        .kept,
        3,
        "fold"
    );
    assert_eq!(
        run(
            &c,
            &r,
            &conv(&all, "go"),
            &Opts::default().form(3, "recalled")
        )
        .kept,
        3,
        "recall"
    );
    assert_eq!(
        run(&c, &r, &conv(&cut, "go"), &Opts::default()).kept,
        3,
        "cut"
    );
}

// C8
#[test]
fn chain_kept_per_type() {
    let c = cache();
    let (a, b) = (Count::new("a"), Count::new("b"));
    let first = conv(&[(1, "x"), (2, "y")], "z");
    run(&c, &a, &first, &Opts::default());
    let mid = run(
        &c,
        &b,
        &conv(&[(1, "x"), (2, "y"), (3, "z")], "w"),
        &Opts::default(),
    );
    assert_eq!(mid.kept, 0, "b has no chain of its own yet");
    let back = run(
        &c,
        &a,
        &conv(&[(1, "x"), (2, "y"), (3, "z")], "w"),
        &Opts::default(),
    );
    assert_eq!(
        back.kept,
        first.len() - 1,
        "a's chain is still a's last pass"
    );
}

// C9
#[test]
fn restart_links_identical() {
    let m = conv(&refs(&rows_n(1, 5, 1)), "go");
    let before = cache();
    run(&before, &Hermes, &m, &Opts::default());
    let pre = run(&before, &Hermes, &m, &Opts::default());
    drop(before);
    let after = run(&cache(), &Hermes, &m, &Opts::default());
    assert_eq!(after.kept, 0, "no kept chain after a restart");
    assert_eq!(after.links, pre.links);
}

// C10
#[test]
fn only_finished_pass_kept() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&refs(&rows_n(1, 5, 0)), "go");
    let first = run(&c, &r, &m, &Opts::default());
    let mut other = m.clone();
    other[2]["content"] = serde_json::json!("changed");
    // a cancelled pass, and a failed one: dropped unfinished
    drop(open(&c, &r, &other, &Opts::default()));
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _v = open(&c, &r, &other, &Opts::default());
        panic!("the pass failed");
    }));
    assert_eq!(c.chain("t"), Some(first.links.clone()), "not replaced");
    assert_eq!(run(&c, &r, &m, &Opts::default()).kept, m.len());
}

// C11
#[test]
fn kept_means_equal_bytes() {
    let rows = rows_n(1, 8, 2);
    for p in 1..9 {
        let (c, r) = (cache(), Count::new("t"));
        let before = run(&c, &r, &conv(&refs(&rows), "go"), &Opts::default());
        let mut changed = rows.clone();
        changed[p - 1].1 = "other".into();
        let after = run(&c, &r, &conv(&refs(&changed), "go"), &Opts::default());
        assert_eq!(after.kept, p);
        assert_eq!(after.messages[..p], before.messages[..p], "p={p}");
        assert_ne!(after.messages[p], before.messages[p], "p={p}");
    }
}

// C12
#[test]
fn endpoint_cold_on_model_switch() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&refs(&rows_n(1, 4, 0)), "go");
    let first = run(&c, &r, &m, &model("qwen2.5-7b"));
    assert_eq!(first.kept_endpoint, 0, "no pass before it");
    assert_eq!(run(&c, &r, &m, &model("qwen2.5-7b")).kept_endpoint, m.len());
    let switched = run(&c, &r, &m, &model("qwen3-8b"));
    assert_eq!(switched.kept, m.len(), "the whole prefix");
    assert_eq!(
        switched.kept_endpoint, 0,
        "other weights: the endpoint is cold"
    );
    let next = run(&c, &r, &m, &model("qwen3-8b"));
    assert_eq!(next.kept_endpoint, next.kept);
}

// C13
#[test]
fn endpoint_cold_on_provider_switch() {
    let (c, r) = (cache(), Count::new("t"));
    let m = conv(&refs(&rows_n(1, 4, 0)), "go");
    run(&c, &r, &m, &Opts::default());
    let other = Opts {
        provider: Some("another provider".into()),
        ..Opts::default()
    };
    let d = run(&c, &r, &m, &other);
    assert_eq!(d.kept, m.len());
    assert_eq!(d.kept_endpoint, 0);
}

// C14
#[test]
fn endpoint_cold_after_lifetime() {
    let now = Arc::new(AtomicU64::new(1_000f64.to_bits()));
    let clock = Arc::clone(&now);
    let c: RenderCache =
        RenderCache::with_clock(move || f64::from_bits(clock.load(Ordering::SeqCst)));
    let r = Count::new("t");
    let m = conv(&refs(&rows_n(1, 4, 0)), "go");
    let at = |sent: f64| Opts {
        last_send: Some(sent),
        ..Opts::default()
    };
    run(&c, &r, &m, &at(1_000.0));
    let lifetime = crate::constants::CACHE_LIFETIME_SECS;
    now.store((1_000.0 + lifetime - 1.0).to_bits(), Ordering::SeqCst);
    assert_eq!(
        run(&c, &r, &m, &at(1_000.0)).kept_endpoint,
        m.len(),
        "still warm"
    );
    now.store((1_000.0 + lifetime + 1.0).to_bits(), Ordering::SeqCst);
    let d = run(&c, &r, &m, &at(1_000.0));
    assert_eq!(d.kept, m.len(), "kept unchanged");
    assert_eq!(d.kept_endpoint, 0, "past the lifetime");
    // the clock at 0 and far in the future changes no byte
    let far = finish(open(&c, &r, &m, &at(0.0)));
    now.store(0f64.to_bits(), Ordering::SeqCst);
    assert_eq!(run(&c, &r, &m, &at(0.0)).messages, far.messages);
}

// C15
/// The endpoint's `cache_read` is a measurement, `kept` a statement about the chat's
/// own bytes: a report smaller than the predicted prefix (a block-rounded count) or
/// larger than it is carried as reported in `prefix_cache`, and changes neither `kept`
/// nor `kept_endpoint`, of this pass or of the next.
#[tokio::test]
async fn reported_read_is_not_kept() {
    use super::pass::Mode;
    use super::tests_cache::{hermes_pass, pass_view, Canned};
    use crate::provider::ProviderReport;
    use serde_json::{json, Map};

    let views = [
        conv(&refs(&rows_n(1, 6, 40)), "go"),
        conv(&refs(&rows_n(1, 8, 40)), "next"),
        conv(&refs(&rows_n(1, 10, 40)), "then"),
    ];
    // the same passes with no report: what the chat predicts from its bytes alone
    let (control, control_state) = hermes_pass(Canned::new("ok"), "c15_control");
    *control_state.last_send_secs.lock().unwrap() = Some(crate::helpers::now_secs());
    let want: Vec<(usize, usize)> = views
        .iter()
        .map(|v| {
            let d = pass_view(&control, v);
            (d.kept, d.kept_endpoint)
        })
        .collect();
    assert!(want[1].0 > 0 && want[1].1 == want[1].0, "{want:?}");

    for reported in [1_i64, 1_000_000] {
        let chat = format!("c15_{reported}");
        let provider = Canned::new("ok");
        let (pass, state) = hermes_pass(provider.clone(), &chat);
        *state.last_send_secs.lock().unwrap() = Some(crate::helpers::now_secs());
        assert_eq!(pass_view(&pass, &views[0]).kept, want[0].0);

        let view = pass.view(&views[1], &Map::new());
        let predicted = (view.kept, view.kept_endpoint);
        assert_eq!(predicted, want[1]);
        *provider.report.lock().unwrap() = ProviderReport {
            prompt_tokens: Some(2_000_000),
            completion_tokens: Some(3),
            cache_read: Some(reported),
            cache_write: Some(0),
            ..ProviderReport::default()
        };
        let report = pass.report(Mode::Pass);
        assert_eq!(
            report.cache_read,
            Some(reported),
            "the report as the endpoint gave it"
        );
        assert_eq!(
            (view.kept, view.kept_endpoint),
            predicted,
            "the report changes neither"
        );
        let _ = view.finish(&mut pass.events(), Mode::Pass.name());

        let rows = pass.take_events().into_rows();
        let measured: Vec<&serde_json::Value> = rows
            .iter()
            .filter(|r| r["event"] == json!("prefix_cache"))
            .collect();
        assert_eq!(measured.len(), 1, "{rows:?}");
        assert_eq!(measured[0]["cache_read"], json!(reported));
        assert_eq!(measured[0]["cache_write"], json!(0));
        assert_eq!(measured[0]["mode"], json!("pass"));
        assert_eq!(measured[0]["type"], json!(super::hermes::NAME));
        let render = rows
            .iter()
            .find(|r| r["event"] == json!("render_warm"))
            .expect("a warm pass");
        assert_eq!(render["kept"], json!(predicted.0));
        assert_eq!(render["kept_endpoint"], json!(predicted.1));

        // the next pass predicts from the links alone, as with no report
        let next = pass_view(&pass, &views[2]);
        assert_eq!((next.kept, next.kept_endpoint), want[2]);
        crate::state::drop_state(&fantastic_kernel::AgentId::from(chat.as_str()));
    }
    crate::state::drop_state(&fantastic_kernel::AgentId::from("c15_control"));
}
