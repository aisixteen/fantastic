//! Model switches (#766, W11): M6-M13, M15, M17, M18 of spec 2.1, and H1-H5, H14 of 2.4.
//!
//! A view is driven through the chat's render cache as the turn drives it
//! ([`super::tests_cache`]), with the real adapters; a case that reads a trace row or the
//! model id of a pass runs the real pass path (`run_generation`) over the scripted
//! endpoint of [`super::tests_trace`].

use serde_json::{json, Map, Value};
use tempfile::TempDir;

use super::cache::{epoch_of, Hash, RenderCache};
use super::default::{self, Tags};
use super::hermes::{self, Hermes};
use super::rules::{load, model_of_with, note_seen, shipped, type_of, RULES_JSON};
use super::tests_cache::{plain, run, Opts};
use super::tests_trace::{
    answered, chat_on, names, set_model, traces_of, Scripted, Step, PLAIN, QWEN,
};
use super::{Out, Reader, Renderer, DEFAULT};
use crate::tool_parse::render_call;

/// A model id of the `hermes` type (Qwen 2.5).
const QWEN25: &str = "qwen2.5-7b-instruct";
/// The hermes `done` call, as a hermes model writes it.
const H_DONE: &str = "<tool_call>\n{\"name\": \"done\", \"arguments\": {}}\n</tool_call>";

// ─── the history ─────────────────────────────────────────────────────────────

/// A call in the internal form.
fn call(name: &str, args: Value) -> String {
    let map: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
    render_call(name, &map)
}

/// The nudge and escalation text of `res/test/history.json` (row 5).
fn nudge() -> String {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("res/test/history.json")).expect("history.json");
    rows.iter()
        .find(|r| r["kind"] == json!("notice"))
        .and_then(|r| r["content"].as_str())
        .expect("the nudge row")
        .to_string()
}

/// The `stop` call with `reason`, as the router stores it.
fn stop(reason: &str) -> String {
    call("stop", json!({"reason": reason}))
}

/// The harness's reason after `n` attempts, for `kind`.
fn harness_reason(kind: &str, n: usize) -> String {
    super::fill(super::of(kind).stop_reason, &[("n", &n.to_string())])
}

/// Stored rows in the internal form: calls, a reply batch with a refusal, the nudge and
/// an escalation, a keep, a stop call, an old correction block. `(id, role, content)`.
fn stored() -> Vec<(i64, &'static str, String)> {
    let send = call(
        "send",
        json!({"target_id": "notes", "title": "Огляд",
               "payload": {"type": "write", "path": "plan.md", "bytes": "fn main() {}\n"}}),
    );
    let refusal = format!(
        "[reply to 11 · notes write · ↑2 · 1 of 2 in batch 2]\n{{\"ok\": true}}\n\n\
         [reply to 12 · kernel grep · ↑2 · 2 of 2 in batch 2]\n\
         call NOT dispatched: `kernel` does not answer `grep`. {}",
        crate::route::NO_TARGET_ERROR
    );
    let keep = format!(
        "{}\n{}",
        call("keep", json!({"of": "11", "summary": "План записано."})),
        call("await", json!({}))
    );
    let correction = format!(
        "[reply to - · - -]\n[malformed call — nothing ran] line 1, col 1: not a call tag\n\
         write it as:\n{}\n{}",
        call(
            "send",
            json!({"target_id": "notes", "payload": {"type": "reflect"}})
        ),
        default::FORM
    );
    vec![
        (1, "user", "Привіт! Склади план.".to_string()),
        (
            2,
            "assistant",
            format!("Почну.\n{send}\n{}", call("done", json!({}))),
        ),
        (3, "user", refusal),
        (4, "assistant", keep),
        (5, "user", nudge()),
        (6, "assistant", stop("Немає доступу до kernel.")),
        (7, "user", correction),
    ]
}

/// The arriving message: the `keep` template the turn puts on a folding reply.
fn arriving() -> String {
    format!(
        "Далі.\n{}",
        call("keep", json!({"of": "13", "summary": "…"}))
    )
}

/// The view of a pass over `rows`: the system message, the rows, the arriving message.
fn view(rows: &[(i64, &str, String)], arriving: &str) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": "SYS"})];
    out.extend(
        rows.iter()
            .map(|(id, role, text)| json!({"id": id, "role": role, "content": text})),
    );
    out.push(json!({"role": "user", "content": arriving}));
    out
}

fn contents(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|m| m["content"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// The pass options for the model `model`.
fn model(model: &str) -> Opts {
    Opts {
        model: Some(model.into()),
        ..Opts::default()
    }
}

/// The keys `(type, id)` a cache holds.
fn keys(c: &RenderCache) -> Vec<(String, i64)> {
    c.stats().keys
}

/// The internal text the reader gives back for a rendered answer: its texts and calls,
/// in order; no `Bad`, no `Open`.
fn read_back(reader: &dyn Reader, rendered: &str) -> String {
    let mut o = reader.outbound();
    let mut outs = o.feed(rendered);
    outs.extend(o.finish());
    outs.into_iter()
        .map(|x| match x {
            Out::Text(t) | Out::Call { text: t } => t,
            other => panic!("{other:?} reading {rendered:?}"),
        })
        .collect()
}

// ─── M6-M13 ──────────────────────────────────────────────────────────────────

// M6
/// A switch inside one type: no row rendered, every entry and link kept, the same bytes,
/// `render_warm {rows: 0}`; at the turn, no `model_switched`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_type_keeps_all() {
    assert_eq!(type_of(shipped(), QWEN25), hermes::NAME);
    assert_eq!(type_of(shipped(), QWEN), hermes::NAME);
    let c = <RenderCache>::new();
    let m = view(&stored(), &arriving());
    let first = run(&c, &Hermes, &m, &model(QWEN25));
    let entries = keys(&c);
    let next = run(&c, &Hermes, &m, &model(QWEN));
    assert_eq!(next.rows, 0);
    assert_eq!(next.messages, first.messages);
    assert_eq!(next.links, first.links);
    assert_eq!(
        next.kept,
        m.len(),
        "every link, the same arriving message's included"
    );
    assert_eq!(keys(&c), entries);
    assert_eq!(names(&next.events), ["render_warm"]);
    assert_eq!(next.events[0]["rows"], json!(0));

    // the turn: the record's model moves inside the type
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "sw_m6_base").await;
    let a = chat_on(
        &kernel,
        "sw_m6",
        fantastic_ram_store::test_support::MEMORY,
        Some(QWEN25),
    )
    .await;
    let p = Scripted::new(QWEN25, vec![]);
    answered(&kernel, &a, &p, "m6_a").await;
    answered(&kernel, &a, &p, "m6_b").await;
    set_model(&kernel, &a, QWEN).await;
    p.set_model(QWEN);
    answered(&kernel, &a, &p, "m6_c").await;
    let rows = traces_of(&kernel, &a, "m6_c").await;
    assert!(
        !names(&rows).contains(&"model_switched".to_string()),
        "{rows:?}"
    );
    assert!(
        rows.iter().all(|r| r["type"] == json!("hermes")),
        "{rows:?}"
    );
    assert_eq!(names(&rows).last().map(String::as_str), Some("render_warm"));
    crate::state::drop_state(&a);
}

// M7
/// D -> H: every row rendered once for H; a second H pass renders none.
#[test]
fn other_type_renders_once() {
    let c = <RenderCache>::new();
    let m = view(&stored(), &arriving());
    let rows = m.len() - 2;
    let d = run(&c, &Tags, &m, &model(PLAIN));
    assert_eq!(
        (d.rows, c.stats().entries),
        (0, 0),
        "default stores nothing"
    );
    let h1 = run(&c, &Hermes, &m, &model(QWEN));
    assert!(h1.cold);
    assert_eq!(h1.rows, rows);
    assert_eq!(c.stats().entries, rows);
    let h2 = run(&c, &Hermes, &m, &model(QWEN));
    assert_eq!(h2.rows, 0);
    assert_eq!(h2.messages, h1.messages);
    assert_eq!(h1.messages, plain(&Hermes, &m));
}

// M8
/// D -> H -> D and H -> D -> H: the third view's bytes equal the first's, and the return
/// renders nothing.
#[test]
fn back_gives_identical_bytes() {
    let m = view(&stored(), &arriving());
    let c = <RenderCache>::new();
    let d1 = run(&c, &Tags, &m, &model(PLAIN));
    run(&c, &Hermes, &m, &model(QWEN));
    let d2 = run(&c, &Tags, &m, &model(PLAIN));
    assert_eq!(d2.messages, d1.messages);
    assert_eq!(d2.rows, 0);

    let c = <RenderCache>::new();
    let h1 = run(&c, &Hermes, &m, &model(QWEN));
    run(&c, &Tags, &m, &model(PLAIN));
    let h2 = run(&c, &Hermes, &m, &model(QWEN));
    assert_eq!(h2.messages, h1.messages);
    assert_eq!(h2.rows, 0, "the return renders nothing");
    assert_eq!(h2.kept, m.len(), "H's chain was kept across the D pass");
}

// M9
/// Nothing of the old type is left: the H view holds no `<call name=`, the D view no
/// `<tool_call>`, over calls, replies, a refusal, a nudge, an escalation, a keep template,
/// a stop call and an old correction block.
#[test]
fn no_foreign_form_left() {
    let m = view(&stored(), &arriving());
    for text in contents(&m) {
        assert!(
            !text.contains("<tool_call>"),
            "a fixture holds H's form: {text}"
        );
    }
    let c = <RenderCache>::new();
    let h = run(&c, &Hermes, &m, &model(QWEN));
    for (i, text) in contents(&h.messages).iter().enumerate() {
        assert!(!text.contains("<call name="), "H message {i}: {text}");
    }
    let d = run(&c, &Tags, &m, &model(PLAIN));
    for (i, text) in contents(&d.messages).iter().enumerate() {
        assert!(!text.contains("<tool_call>"), "D message {i}: {text}");
    }
    assert_eq!(d.messages, crate::agent_loop::render_for_model(&m));
}

// M10
/// An id that holds an earlier rule's substring and a later one's: the first rule's type
/// is used, and the turn's trace rows name it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unexpected_match_is_traced() {
    let id = "NousResearch/Hermes-Qwen3-Coder-7B";
    let lower = id.to_lowercase();
    assert!(lower.contains("hermes") && lower.contains("qwen3-coder"));
    assert_eq!(type_of(shipped(), id), DEFAULT, "`qwen3-coder` comes first");
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "sw_m10_base").await;
    let a = chat_on(
        &kernel,
        "sw_m10",
        fantastic_ram_store::test_support::MEMORY,
        Some(id),
    )
    .await;
    let p = Scripted::new(id, vec![]);
    answered(&kernel, &a, &p, "m10_a").await;
    let rows = traces_of(&kernel, &a, "m10_a").await;
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r["type"] == json!(DEFAULT)), "{rows:?}");
    // the system block carries the default how-to, not the hermes one
    let sys = p.asked()[0][0]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(!sys.contains("<tools>"), "{sys}");
    crate::state::drop_state(&a);
}

/// The shipped rule list with `{"contains": "qwen3-8b", "type": "default"}` put first.
fn moved_rules() -> String {
    let mut v: Vec<Value> = serde_json::from_str(RULES_JSON).expect("rules");
    v.insert(0, json!({"contains": "qwen3-8b", "type": "default"}));
    serde_json::to_string_pretty(&v).expect("rules text")
}

/// The epoch of a build with `rules`.
fn epoch_with(rules: &str) -> Hash {
    let renderers: Vec<&dyn Renderer> = super::REGISTRY.iter().map(|e| e.renderer).collect();
    epoch_of(rules.as_bytes(), &renderers)
}

// M11, M12
/// The rule list changes, the same id moves to another type (unit-only): the epoch
/// differs, everything is cleared, the render is cold; back on the first list it is cold
/// again (the epoch changed again) and the bytes equal the ones rendered before.
#[test]
fn rule_change_resets() {
    let moved = load(&moved_rules()).expect("the moved list loads");
    assert_eq!(type_of(shipped(), QWEN), hermes::NAME);
    assert_eq!(type_of(&moved, QWEN), DEFAULT);
    let (e1, e2) = (epoch_with(RULES_JSON), epoch_with(&moved.source));
    assert_ne!(e1, e2);
    assert_eq!(e1, super::cache::epoch(), "the shipped epoch");
    let m = view(&stored(), &arriving());
    let c = <RenderCache>::new();
    let at = |e: Hash| Opts {
        epoch: e,
        ..model(QWEN)
    };
    run(&c, &Hermes, &m, &at(e1));
    let before = run(&c, &Hermes, &m, &at(e1));
    assert_eq!(before.rows, 0);
    assert!(c.stats().entries > 0);
    // the new build: the id is `default`
    let after = run(&c, super::of(type_of(&moved, QWEN)).renderer, &m, &at(e2));
    assert!(after.cold);
    assert_eq!(
        (after.kept, c.stats().entries),
        (0, 0),
        "everything cleared"
    );
    assert_eq!(after.messages, crate::agent_loop::render_for_model(&m));
    assert_eq!(c.stats().chains, 1, "only the new build's chain");
}

// M12
#[test]
fn back_after_rule_change() {
    let moved = load(&moved_rules()).expect("the moved list loads");
    let (e1, e2) = (epoch_with(RULES_JSON), epoch_with(&moved.source));
    let m = view(&stored(), &arriving());
    let c = <RenderCache>::new();
    let at = |e: Hash| Opts {
        epoch: e,
        ..model(QWEN)
    };
    let recorded = run(&c, &Hermes, &m, &at(e1));
    run(&c, &Tags, &m, &at(e2));
    let back = run(
        &c,
        super::of(type_of(shipped(), QWEN)).renderer,
        &m,
        &at(e1),
    );
    assert!(back.cold, "cold again: the epoch changed again");
    assert_eq!((back.rows, back.kept), (m.len() - 2, 0));
    assert_eq!(back.messages, recorded.messages);
    assert_eq!(back.messages, plain(&Hermes, &m));
}

// M13
/// No model known (no record `model`, no reply naming one, no `models` default, no row):
/// the pass runs as `default`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_model_is_default() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "sw_m13_base").await;
    let a = chat_on(
        &kernel,
        "sw_m13",
        fantastic_ram_store::test_support::MEMORY,
        None,
    )
    .await;
    let state = crate::state::state_for(&a);
    let meta = crate::helpers::agent_meta(&a, &kernel);
    let got = model_of_with(
        &meta,
        &state,
        |_| async { None::<Option<String>> },
        || async { None::<String> },
    )
    .await;
    assert_eq!(got, None);
    // the endpoint names no model either
    let p = Scripted::new("", vec![]);
    answered(&kernel, &a, &p, "m13_a").await;
    let rows = traces_of(&kernel, &a, "m13_a").await;
    assert!(!rows.is_empty());
    assert!(rows.iter().all(|r| r["type"] == json!(DEFAULT)), "{rows:?}");
    crate::state::drop_state(&a);
}

// M15
/// The first reply names a hermes model after a default pass: that pass stands; the next
/// pass is H, cold, after `model_switched {from: default, to: hermes}`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reply_names_the_type() {
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "sw_m15_base").await;
    let a = chat_on(
        &kernel,
        "sw_m15",
        fantastic_ram_store::test_support::MEMORY,
        None,
    )
    .await;
    let p = Scripted::new(PLAIN, vec![Step::says("ok").named_model(QWEN)]);
    let (text, _) = answered(&kernel, &a, &p, "m15_a").await;
    assert_eq!(text, "ok", "the default pass stands");
    let first = traces_of(&kernel, &a, "m15_a").await;
    assert!(
        first.iter().all(|r| r["type"] == json!(DEFAULT)),
        "{first:?}"
    );
    answered(&kernel, &a, &p, "m15_b").await;
    let rows = traces_of(&kernel, &a, "m15_b").await;
    assert_eq!(names(&rows), ["model_switched", "render_cold"], "{rows:?}");
    assert_eq!(
        (&rows[0]["from"], &rows[0]["to"], &rows[0]["model"]),
        (&json!("default"), &json!("hermes"), &json!(QWEN))
    );
    // the second request is in H's form: its system block carries the tools block
    let sys = p.asked()[1][0]["content"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(sys.contains("<tools>"), "{sys}");
    crate::state::drop_state(&a);
}

// M17
/// `provider_id` changes: what the old provider named is forgotten; the type comes from
/// what the new provider names in its first reply.
#[tokio::test]
async fn provider_change_forgets_model() {
    let state = crate::state::state_for(&fantastic_kernel::AgentId::from("sw_m17"));
    let meta = |p: &str| -> Map<String, Value> {
        let mut m = Map::new();
        m.insert(crate::remote_provider::PROVIDER_FIELD.to_string(), json!(p));
        m
    };
    let none = || async { None::<String> };
    let _ = model_of_with(&meta("p1"), &state, |_| async { None::<Option<String>> }, none).await;
    note_seen(&state, QWEN);
    let got = model_of_with(&meta("p1"), &state, |_| async { None::<Option<String>> }, none).await;
    assert_eq!(got.as_deref(), Some(QWEN));
    assert_eq!(type_of(shipped(), got.as_deref().unwrap()), hermes::NAME);
    // the record names another provider
    let got = model_of_with(&meta("p2"), &state, |_| async { None::<Option<String>> }, none).await;
    assert_eq!(got, None, "model_seen was the old provider's");
    assert!(state.model_seen.lock().unwrap().is_none());
    assert_eq!(type_of(shipped(), got.as_deref().unwrap_or("")), DEFAULT);
    // the new provider's first reply names its model
    note_seen(&state, "llama-3.1-8b-instruct");
    let got = model_of_with(&meta("p2"), &state, |_| async { None::<Option<String>> }, none).await;
    assert_eq!(got.as_deref(), Some("llama-3.1-8b-instruct"));
    assert_eq!(type_of(shipped(), got.as_deref().unwrap()), DEFAULT);
    crate::state::drop_state(&fantastic_kernel::AgentId::from("sw_m17"));
}

// M18
/// A switch with a record `system_prompt`: no how-to is inserted, and the stored calls
/// are rendered in the new type.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn custom_system_prompt_no_howto() {
    const PROMPT: &str = "Ти — помічник нотаток.";
    let tmp = TempDir::new().unwrap();
    let (kernel, _) = crate::tests::mk_kernel(&tmp, "sw_m18_base").await;
    let a = chat_on(
        &kernel,
        "sw_m18",
        fantastic_ram_store::test_support::MEMORY,
        Some(PLAIN),
    )
    .await;
    let r = super::tests_trace::operator(
        &kernel,
        "kernel_state",
        json!({"type": "update_agent", "id": a.as_str(), "system_prompt": PROMPT}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let p = Scripted::new(
        PLAIN,
        vec![Step::says(&format!("ok\n{}", call("done", json!({}))))],
    );
    answered(&kernel, &a, &p, "m18_a").await;
    set_model(&kernel, &a, QWEN).await;
    p.set_model(QWEN);
    p.push(vec![Step::says(&format!("ok\n{H_DONE}"))]);
    answered(&kernel, &a, &p, "m18_b").await;
    answered(&kernel, &a, &p, "m18_c").await;
    for (i, req) in p.asked().iter().enumerate() {
        let sys = req[0]["content"].as_str().unwrap_or("");
        assert!(sys.contains(PROMPT), "request {i}: {sys}");
        assert!(!sys.contains("## Reaching"), "request {i}: a how-to: {sys}");
        assert!(!sys.contains(hermes::HOWTO_SEND), "request {i}");
        assert!(!sys.contains(default::HOWTO_SEND), "request {i}");
    }
    // the H requests show both stored calls in H's form
    for req in &p.asked()[1..] {
        let body: Vec<String> = contents(&req[1..]);
        assert!(body.iter().any(|t| t.contains(H_DONE)), "{body:?}");
        assert!(body.iter().all(|t| !t.contains("<call name=")), "{body:?}");
    }
    crate::state::drop_state(&a);
}

// ─── H1-H5, H14 ──────────────────────────────────────────────────────────────

/// The rows of [`stored`] with row `id` shown as `text`.
fn with_row(id: i64, text: &str) -> Vec<(i64, &'static str, String)> {
    stored()
        .into_iter()
        .map(|(i, role, t)| (i, role, if i == id { text.to_string() } else { t }))
        .collect()
}

// H1
/// A switch during a fold: pass 1 unfolded on D, pass 2 folded on H. H renders the folded
/// source; no D bytes are left.
#[test]
fn during_fold() {
    let c = <RenderCache>::new();
    let m1 = view(&stored(), &arriving());
    run(&c, &Tags, &m1, &model(PLAIN));
    let fold = crate::folds::make_fold(&[2, 3], "План", "План записано.", false, 20, 2, 1, None);
    let folded_text = fold["content"].as_str().unwrap().to_string();
    let mut rows: Vec<(i64, &str, String)> = stored()
        .into_iter()
        .filter(|(i, ..)| *i != 2 && *i != 3)
        .collect();
    rows.insert(1, (20, "user", folded_text.clone()));
    let m2 = view(&rows, &arriving());
    let h = run(&c, &Hermes, &m2, &model(QWEN));
    assert_eq!(h.messages, plain(&Hermes, &m2));
    assert_eq!(h.messages[2]["content"], json!(Hermes.render(&folded_text)));
    for text in contents(&h.messages) {
        assert!(!text.contains("<call name="), "{text}");
    }
}

// H2
/// A switch after a recall: the recalled row and an anchor both in the new type.
#[test]
fn after_recall() {
    let c = <RenderCache>::new();
    let anchor = format!(
        "[reply to 11 · notes write · ↑2] (folded: 1 call)\n{}",
        call(
            "send",
            json!({"target_id": "ai", "payload": {"type": "recall", "ids": ["3"]}})
        )
    );
    let rows = with_row(7, &anchor);
    let m = view(&rows, &arriving());
    run(
        &c,
        &Tags,
        &m,
        &model(PLAIN).form(3, "recalled").form(7, "anchor"),
    );
    let h = run(
        &c,
        &Hermes,
        &m,
        &model(QWEN).form(3, "recalled").form(7, "anchor"),
    );
    assert_eq!(h.messages, plain(&Hermes, &m));
    let shown = contents(&h.messages);
    assert!(shown[3].contains("<tool_response>"), "{}", shown[3]);
    assert!(shown[7].contains("<tool_call>"), "{}", shown[7]);
    assert!(!shown[7].contains("<call name="), "{}", shown[7]);
}

// H3
/// A switch after a cut (`context_clear`): only the rows in view render for the new type.
#[test]
fn after_cut() {
    let c = <RenderCache>::new();
    let all = stored();
    run(&c, &Tags, &view(&all, &arriving()), &model(PLAIN));
    let kept: Vec<(i64, &str, String)> = all.into_iter().filter(|(i, ..)| *i >= 5).collect();
    let m = view(&kept, &arriving());
    let h = run(&c, &Hermes, &m, &model(QWEN));
    assert_eq!(h.rows, kept.len());
    let ids: Vec<i64> = keys(&c).into_iter().map(|(_, id)| id).collect();
    assert_eq!(ids, vec![5, 6, 7], "entries only for the rows in view");
    assert_eq!(h.messages, plain(&Hermes, &m));
}

// H4
/// Replies in flight: calls sent on D, the reply batch arrives on H; the reply blocks are
/// in H's reply form and their pids unchanged.
#[test]
fn replies_in_flight() {
    let c = <RenderCache>::new();
    let sent: Vec<(i64, &str, String)> = stored().into_iter().take(2).collect();
    run(&c, &Tags, &view(&sent, "go"), &model(PLAIN));
    let mut rows = sent;
    rows.push(stored().remove(2));
    let m = view(&rows, "go");
    let h = run(&c, &Hermes, &m, &model(QWEN));
    let batch = h.messages[3]["content"].as_str().unwrap();
    assert!(
        batch
            .starts_with("[reply to 11 · notes write · ↑2 · 1 of 2 in batch 2]\n<tool_response>\n"),
        "{batch}"
    );
    assert!(
        batch.contains("\n</tool_response>\n\n[reply to 12 · kernel grep · ↑2 · 2 of 2 in batch 2]\n<tool_response>\n"),
        "{batch}"
    );
    assert_eq!(batch.matches("<tool_response>").count(), 2, "{batch}");
}

// H5
/// Rows written under D, H, D: one uniform view in the current type; the stored rows stay
/// byte for byte.
#[test]
fn mixed_rows_uniform() {
    // rows are stored in the internal form whichever type wrote them
    let h_written = read_back(&Hermes, &format!("Читаю.\n{H_DONE}"));
    assert_eq!(h_written, format!("Читаю.\n{}", call("done", json!({}))));
    let rows: Vec<(i64, &str, String)> = vec![
        (1, "user", "a".into()),
        (
            2,
            "assistant",
            format!(
                "D. {}",
                call(
                    "send",
                    json!({"target_id": "notes", "payload": {"type": "read"}})
                )
            ),
        ),
        (3, "user", "[reply to 9 · notes read]\nx".into()),
        (4, "assistant", h_written),
        (5, "user", "b".into()),
        (
            6,
            "assistant",
            format!("D again. {}", call("await", json!({}))),
        ),
    ];
    let m = view(&rows, "go");
    let before = m.clone();
    let c = <RenderCache>::new();
    let h = run(&c, &Hermes, &m, &model(QWEN));
    assert_eq!(m, before, "the stored rows are unchanged");
    for text in contents(&h.messages) {
        assert!(!text.contains("<call name="), "{text}");
    }
    let d = run(&c, &Tags, &m, &model(PLAIN));
    for text in contents(&d.messages) {
        assert!(!text.contains("<tool_call>"), "{text}");
    }
}

// H14
/// A stored stop call, one written by a model and one by the harness: verbatim for D; for
/// H the call form `{"name": "stop", "arguments": {"reason": ..}}`; both round-trip and
/// are treated alike.
#[test]
fn stop_call_uniform() {
    let harness = stop(&harness_reason(DEFAULT, 3));
    // the model's, as the hermes reader stores it
    let reason = "Немає доступу до kernel.";
    let model_written = read_back(
        &Hermes,
        &format!("<tool_call>\n{{\"name\": \"stop\", \"arguments\": {{\"reason\": \"{reason}\"}}}}\n</tool_call>"),
    );
    assert_eq!(model_written, stop(reason));
    for (who, text, why) in [
        ("harness", harness.clone(), harness_reason(DEFAULT, 3)),
        ("model", model_written.clone(), reason.to_string()),
    ] {
        assert_eq!(Tags.render(&text), text, "{who}: D verbatim");
        let h = Hermes.render(&text);
        let mut args = Map::new();
        args.insert("reason".into(), json!(why));
        let mut want = String::new();
        hermes::write_json(&json!({"name": "stop", "arguments": args}), &mut want);
        assert_eq!(h, format!("<tool_call>\n{want}\n</tool_call>"), "{who}");
        assert_eq!(read_back(&Hermes, &h), text, "{who}: H round trip");
        assert_eq!(read_back(&Tags, &text), text, "{who}: D round trip");
        let parsed = crate::tool_parse::extract_tool_calls(&text);
        assert_eq!(parsed.len(), 1, "{who}");
        assert_eq!(parsed[0].name, "stop", "{who}");
        assert_eq!(parsed[0].arguments["reason"], json!(why), "{who}");
    }
    // alike: the same reason gives the same bytes, whoever wrote it
    let same = stop(&harness_reason(DEFAULT, 3));
    assert_eq!(Hermes.render(&same), Hermes.render(&harness));
}
