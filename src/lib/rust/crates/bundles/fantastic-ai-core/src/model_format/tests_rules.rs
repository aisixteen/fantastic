//! The model type of a pass and the model id it is chosen by (#766, M1-M5, M14, M20-M22;
//! the qwen rules of 2026-11-25).

use super::rules::{default_in, first_set, load, model_of_with, note_seen, shipped, type_of};
use super::{declared, DEFAULT, REGISTRY};
use fantastic_kernel::AgentId;
use serde_json::{json, Map, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// The real-id table written from the rule file: `[{id, type}]`.
pub(super) const REAL_IDS: &str = include_str!("res/test/real_ids.json");

pub(super) fn real_ids() -> Vec<(String, String)> {
    let v: Value = serde_json::from_str(REAL_IDS).expect("real_ids.json");
    v.as_array()
        .expect("an array")
        .iter()
        .map(|r| {
            (
                r["id"].as_str().expect("id").to_string(),
                r["type"].as_str().expect("type").to_string(),
            )
        })
        .collect()
}

fn meta(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), json!(v)))
        .collect()
}

/// A fresh chat state of its own for one test.
fn state(tag: &str) -> Arc<crate::state::BackendState> {
    let id = AgentId::from(format!("mf_rules_{tag}").as_str());
    crate::state::drop_state(&id);
    crate::state::state_for(&id)
}

/// An `ask` that counts its calls and answers `answer`.
fn asker(
    count: &Arc<AtomicUsize>,
    answer: Option<&str>,
) -> impl FnOnce(String) -> std::future::Ready<Option<Option<String>>> {
    let count = Arc::clone(count);
    let answer = answer.map(str::to_string);
    move |_provider| {
        count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Some(answer))
    }
}

/// An `ask` that counts its calls and gets no answer (the provider is not up, the call
/// failed).
fn unanswered(
    count: &Arc<AtomicUsize>,
) -> impl FnOnce(String) -> std::future::Ready<Option<Option<String>>> {
    let count = Arc::clone(count);
    move |_provider| {
        count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(None)
    }
}

/// A `newest` that counts its calls and answers `answer`.
fn newest(
    count: &Arc<AtomicUsize>,
    answer: Option<&str>,
) -> impl FnOnce() -> std::future::Ready<Option<String>> {
    let count = Arc::clone(count);
    let answer = answer.map(str::to_string);
    move || {
        count.fetch_add(1, Ordering::SeqCst);
        std::future::ready(answer)
    }
}

// M1
#[test]
fn no_match_is_default() {
    let rules = shipped();
    for id in [
        "llama-3.1-8b-instruct",
        "gpt-oss-20b",
        "",
        "stub",
        "мовна-модель",
    ] {
        assert_eq!(type_of(rules, id), DEFAULT, "{id:?}");
    }
}

// M2
#[test]
fn first_rule_wins() {
    let rules = shipped();
    assert_eq!(type_of(rules, "qwen3-coder-30b"), "default");
    assert_eq!(type_of(rules, "qwen3-8b"), "hermes");
    // the same id under a hand-written list: the earlier rule wins either way round
    let a = load(
        r#"[{"contains":"qwen","type":"hermes"},{"contains":"qwen3-coder","type":"default"}]"#,
    )
    .unwrap();
    assert_eq!(type_of(&a, "qwen3-coder-30b"), "hermes");
    let b = load(
        r#"[{"contains":"qwen3-coder","type":"default"},{"contains":"qwen","type":"hermes"}]"#,
    )
    .unwrap();
    assert_eq!(type_of(&b, "qwen3-coder-30b"), "default");
}

// M3
#[test]
fn case_insensitive() {
    let rules = shipped();
    for (id, want) in [
        ("QWEN3-8B", "hermes"),
        ("Qwen/Qwen3-8B", "hermes"),
        ("Qwen3-Coder-30B", "default"),
        ("HERMES-3-LLAMA-3.1-8B", "hermes"),
    ] {
        assert_eq!(type_of(rules, id), want, "{id}");
    }
}

// M4
#[test]
fn unknown_type_is_error() {
    let err = load(r#"[{"contains":"qwen","type":"nope"}]"#).unwrap_err();
    assert!(err.contains("unknown type `nope`"), "{err}");
    for bad in [
        "not json",
        r#"{"contains":"qwen","type":"hermes"}"#,
        r#"["qwen"]"#,
        r#"[{"contains":"qwen"}]"#,
        r#"[{"type":"hermes"}]"#,
        r#"[{"contains":"qwen","type":"hermes","extra":1}]"#,
        r#"[{"contains":"","type":"hermes"}]"#,
        r#"[{"contains":"Qwen","type":"hermes"}]"#,
        r#"[{"contains":7,"type":"hermes"}]"#,
    ] {
        assert!(load(bad).is_err(), "{bad} loaded");
    }
}

// M5
#[test]
fn shipped_rules_load() {
    let rules = shipped();
    assert!(!rules.rules.is_empty());
    assert_eq!(rules.source, super::rules::RULES_JSON);
    for r in &rules.rules {
        assert!(declared(r.kind).is_some(), "{} is not registered", r.kind);
    }
    let names: Vec<&str> = REGISTRY.iter().map(|e| e.name).collect();
    assert_eq!(names, ["default", "hermes"]);
    // the owner's qwen list (2026-11-25), then `hermes`: exactly these rules, in this
    // order; no catch-all `qwen` rule
    let got: Vec<(&str, &str)> = rules
        .rules
        .iter()
        .map(|r| (r.contains.as_str(), r.kind))
        .collect();
    assert_eq!(
        got,
        [
            ("qwen3-coder", "default"),
            ("embedding", "default"),
            ("reranker", "default"),
            ("qwen2.5-vl", "default"),
            ("qwen2.5-omni", "default"),
            ("qwen2.5-math", "default"),
            ("deepseek-r1-distill", "default"),
            ("qwen3", "hermes"),
            ("qwq", "hermes"),
            ("qwen2.5", "hermes"),
            ("hermes", "hermes"),
        ]
    );
}

// M5, the qwen rules
/// Each qwen rule decides its ids, and its place matters: an id that also holds a later
/// rule's substring takes the earlier rule's type; an id no rule names is `default`.
#[test]
fn qwen_rules_each_and_order() {
    let rules = shipped();
    for (id, want, why) in [
        // 1 before 5: qwen3-coder holds `qwen3`
        (
            "Qwen/Qwen3-Coder-30B-A3B-Instruct",
            "default",
            "qwen3-coder before qwen3",
        ),
        // 2 before 5: embedding / reranker ids hold `qwen3`
        (
            "Qwen/Qwen3-Embedding-0.6B",
            "default",
            "embedding before qwen3",
        ),
        (
            "Qwen/Qwen3-Reranker-0.6B",
            "default",
            "reranker before qwen3",
        ),
        // 2 for an id of another family
        ("bge-reranker-v2-m3", "default", "reranker"),
        // 3 before 7: each holds `qwen2.5`
        (
            "Qwen/Qwen2.5-VL-7B-Instruct",
            "default",
            "qwen2.5-vl before qwen2.5",
        ),
        (
            "Qwen/Qwen2.5-Omni-7B",
            "default",
            "qwen2.5-omni before qwen2.5",
        ),
        (
            "Qwen/Qwen2.5-Math-7B-Instruct",
            "default",
            "qwen2.5-math before qwen2.5",
        ),
        // 4: the id holds `qwen`, which no later rule matches alone
        (
            "deepseek-ai/DeepSeek-R1-Distill-Qwen-7B",
            "default",
            "deepseek-r1-distill",
        ),
        // 4 before 7: a distill of a qwen2.5 base
        (
            "deepseek-r1-distill-qwen2.5-7b",
            "default",
            "deepseek-r1-distill before qwen2.5",
        ),
        // 5
        ("Qwen/Qwen3-8B", "hermes", "qwen3"),
        ("Qwen/Qwen3-30B-A3B-Instruct-2507", "hermes", "qwen3"),
        ("Qwen/Qwen3-VL-8B-Instruct", "hermes", "qwen3"),
        ("Qwen/Qwen3-Omni-30B-A3B-Instruct", "hermes", "qwen3"),
        ("qwen3:8b", "hermes", "qwen3"),
        // 6
        ("Qwen/QwQ-32B", "hermes", "qwq"),
        ("qwq:32b", "hermes", "qwq"),
        // 7
        ("Qwen/Qwen2.5-7B-Instruct", "hermes", "qwen2.5"),
        ("Qwen/Qwen2.5-Coder-7B-Instruct", "hermes", "qwen2.5"),
        ("qwen2.5:7b", "hermes", "qwen2.5"),
        // 8: anything else
        ("Qwen/Qwen2-7B-Instruct", "default", "qwen2 has no rule"),
        ("qwen2:7b", "default", "qwen2 has no rule"),
        ("Qwen/Qwen1.5-7B-Chat", "default", "qwen1.5 has no rule"),
        ("qwen1.5:7b", "default", "qwen1.5 has no rule"),
        ("Qwen/Qwen-7B-Chat", "default", "qwen has no rule"),
        ("qwen:7b", "default", "qwen has no rule"),
    ] {
        assert_eq!(type_of(rules, id), want, "{id}: {why}");
    }
    // the order is load-bearing: with each pair swapped, the later rule would win
    for (first, then, id) in [
        ("qwen3-coder", "qwen3", "qwen3-coder-30b"),
        ("embedding", "qwen3", "qwen3-embedding-8b"),
        ("reranker", "qwen3", "qwen3-reranker-4b"),
        ("qwen2.5-vl", "qwen2.5", "qwen2.5-vl-7b"),
        ("qwen2.5-omni", "qwen2.5", "qwen2.5-omni-7b"),
        ("qwen2.5-math", "qwen2.5", "qwen2.5-math-7b"),
        (
            "deepseek-r1-distill",
            "qwen2.5",
            "deepseek-r1-distill-qwen2.5-7b",
        ),
    ] {
        let pos = |c: &str| rules.rules.iter().position(|r| r.contains == c).unwrap();
        assert!(pos(first) < pos(then), "{first} before {then}");
        let hit = |r: &super::rules::Rule| id.contains(r.contains.as_str());
        assert!(
            hit(&rules.rules[pos(first)]) && hit(&rules.rules[pos(then)]),
            "{id}"
        );
        assert_ne!(
            rules.rules[pos(first)].kind,
            rules.rules[pos(then)].kind,
            "{id}"
        );
    }
}

// M14
#[tokio::test]
async fn model_of_order() {
    assert_eq!(
        first_set([Some("r"), Some("s"), Some("d"), Some("n")]),
        Some("r")
    );
    assert_eq!(
        first_set([None, Some("s"), Some("d"), Some("n")]),
        Some("s")
    );
    assert_eq!(first_set([Some(""), None, Some("d"), Some("n")]), Some("d"));
    assert_eq!(first_set([None, None, None, Some("n")]), Some("n"));
    assert_eq!(first_set([None, Some(""), None, None]), None);

    let (asks, rows) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    // the record's model beats everything; nothing else is asked or read
    let st = state("order");
    note_seen(&st, "seen-model");
    let m = meta(&[("model", "record-model"), ("provider_id", "p")]);
    let got = model_of_with(
        &m,
        &st,
        asker(&asks, Some("default-model")),
        newest(&rows, Some("row-model")),
    )
    .await;
    assert_eq!(got.as_deref(), Some("record-model"));
    assert_eq!(
        (asks.load(Ordering::SeqCst), rows.load(Ordering::SeqCst)),
        (0, 0)
    );

    // no record model: what the provider named beats the `models` default
    let st = state("order_seen");
    let m = meta(&[("provider_id", "p")]);
    let _ = model_of_with(&m, &st, asker(&asks, None), newest(&rows, None)).await;
    note_seen(&st, "seen-model");
    let got = model_of_with(
        &m,
        &st,
        asker(&asks, Some("default-model")),
        newest(&rows, Some("row-model")),
    )
    .await;
    assert_eq!(got.as_deref(), Some("seen-model"));

    // nothing seen: the `models` default beats the newest row
    let st = state("order_default");
    let (asks, rows) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let got = model_of_with(
        &m,
        &st,
        asker(&asks, Some("default-model")),
        newest(&rows, Some("row-model")),
    )
    .await;
    assert_eq!(got.as_deref(), Some("default-model"));
    assert_eq!(
        (asks.load(Ordering::SeqCst), rows.load(Ordering::SeqCst)),
        (1, 0)
    );

    // no default: the newest row's model
    let st = state("order_row");
    let got = model_of_with(
        &m,
        &st,
        asker(&asks, None),
        newest(&rows, Some("row-model")),
    )
    .await;
    assert_eq!(got.as_deref(), Some("row-model"));
    assert_eq!(rows.load(Ordering::SeqCst), 1);

    // nothing anywhere: none, so the type is the default one
    let st = state("order_none");
    let got = model_of_with(
        &Map::new(),
        &st,
        asker(&asks, Some("x")),
        newest(&rows, None),
    )
    .await;
    assert_eq!(got, None);
    assert_eq!(type_of(shipped(), got.as_deref().unwrap_or("")), DEFAULT);

    // a changed pair forgets what the provider named before
    let st = state("order_pair");
    let a = meta(&[("provider_id", "p1")]);
    let _ = model_of_with(&a, &st, asker(&asks, None), newest(&rows, None)).await;
    note_seen(&st, "qwen3-8b");
    let got = model_of_with(&a, &st, asker(&asks, None), newest(&rows, None)).await;
    assert_eq!(got.as_deref(), Some("qwen3-8b"));
    let b = meta(&[("provider_id", "p2")]);
    let got = model_of_with(&b, &st, asker(&asks, None), newest(&rows, None)).await;
    assert_eq!(got, None, "model_seen was for the other provider");
    assert!(st.model_seen.lock().unwrap().is_none());
}

// M20
#[test]
fn real_id_table() {
    let rules = shipped();
    let table = real_ids();
    for (id, want) in &table {
        assert_eq!(type_of(rules, id), want.as_str(), "{id}");
        // the type the file states: its first rule in file order, read off the file itself
        let file: Value = serde_json::from_str(super::rules::RULES_JSON).unwrap();
        let first = file
            .as_array()
            .unwrap()
            .iter()
            .find(|r| id.to_lowercase().contains(r["contains"].as_str().unwrap()))
            .map_or("default", |r| r["type"].as_str().unwrap());
        assert_eq!(first, want.as_str(), "{id}");
    }
    // every rule has an id of its own in the table: one whose first match is that rule
    for (i, rule) in rules.rules.iter().enumerate() {
        let hit = table.iter().any(|(id, _)| {
            let id = id.to_lowercase();
            rules
                .rules
                .iter()
                .position(|r| id.contains(r.contains.as_str()))
                == Some(i)
        });
        assert!(
            hit,
            "no id in real_ids.json for the rule `{}`",
            rule.contains
        );
    }
    // the 14 templates read (res/hermes/source.txt), and the ids the tests use
    for must in [
        "Qwen/Qwen3-8B",
        "Qwen/Qwen2.5-7B-Instruct",
        "Qwen/Qwen2.5-Coder-7B-Instruct",
        "Qwen/Qwen2.5-Math-7B-Instruct",
        "Qwen/Qwen2.5-VL-7B-Instruct",
        "Qwen/Qwen2.5-Omni-7B",
        "Qwen/Qwen3-30B-A3B-Instruct-2507",
        "Qwen/Qwen3-VL-8B-Instruct",
        "Qwen/Qwen3-Omni-30B-A3B-Instruct",
        "Qwen/Qwen3-Coder-30B-A3B-Instruct",
        "Qwen/Qwen3-Embedding-0.6B",
        "Qwen/Qwen3-Reranker-0.6B",
        "Qwen/QwQ-32B",
        "deepseek-ai/DeepSeek-R1-Distill-Qwen-7B",
        "qwen2.5-7b-instruct",
        "qwen3-coder-30b",
        "hermes-3-llama-3.1-8b",
    ] {
        assert!(table.iter().any(|(id, _)| id == must), "{must}");
    }
}

// M21
#[tokio::test]
async fn models_default_before_first_pass() {
    let st = state("models_default");
    let (asks, rows) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let m = meta(&[("provider_id", "p")]);
    let got = model_of_with(
        &m,
        &st,
        asker(&asks, Some("Qwen/Qwen3-8B")),
        newest(&rows, None),
    )
    .await;
    assert_eq!(got.as_deref(), Some("Qwen/Qwen3-8B"));
    assert_eq!(type_of(shipped(), got.as_deref().unwrap()), "hermes");
    // asked once per pair: the next pass keeps the answer
    let got = model_of_with(&m, &st, asker(&asks, Some("other")), newest(&rows, None)).await;
    assert_eq!(got.as_deref(), Some("Qwen/Qwen3-8B"));
    assert_eq!(asks.load(Ordering::SeqCst), 1);
    // another pair is asked again
    let m2 = meta(&[("provider_id", "p2")]);
    let got = model_of_with(
        &m2,
        &st,
        asker(&asks, Some("llama-3.1-8b")),
        newest(&rows, None),
    )
    .await;
    assert_eq!(got.as_deref(), Some("llama-3.1-8b"));
    assert_eq!(asks.load(Ordering::SeqCst), 2);
    assert_eq!(rows.load(Ordering::SeqCst), 0);
    // a record with no provider asks nobody
    let st = state("models_no_provider");
    let got = model_of_with(
        &Map::new(),
        &st,
        asker(&asks, Some("x")),
        newest(&rows, None),
    )
    .await;
    assert_eq!(got, None);
    assert_eq!(asks.load(Ordering::SeqCst), 2);
}

// M22
#[tokio::test]
async fn models_unanswered_is_default() {
    // what counts as no default in an answer
    assert_eq!(
        default_in(&json!({"models": ["a"], "default": "a"})).as_deref(),
        Some("a")
    );
    assert_eq!(
        default_in(&json!({"error": "refused", "default": "a"})),
        None
    );
    assert_eq!(default_in(&json!({"models": [], "default": null})), None);
    assert_eq!(default_in(&json!({"models": []})), None);
    assert_eq!(default_in(&json!({"models": [], "default": ""})), None);
    assert_eq!(default_in(&json!({"models": [], "default": 7})), None);
    // answered with no default (a refusal, none named): the pass runs as default, and
    // the answer is kept for the pair, so the ask is not repeated
    let st = state("models_unanswered");
    let (asks, rows) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let m = meta(&[("provider_id", "p")]);
    for _ in 0..3 {
        let got = model_of_with(&m, &st, asker(&asks, None), newest(&rows, None)).await;
        assert_eq!(got, None);
        assert_eq!(type_of(shipped(), got.as_deref().unwrap_or("")), DEFAULT);
    }
    assert_eq!(asks.load(Ordering::SeqCst), 1);
    // no answer at all (the provider not up yet, the call failed): that pass runs as
    // default with one ask; nothing is kept, so the next pass asks again and its type
    // follows the answer
    let st = state("models_no_answer_then_answer");
    let asks = Arc::new(AtomicUsize::new(0));
    let got = model_of_with(&m, &st, unanswered(&asks), newest(&rows, None)).await;
    assert_eq!(got, None);
    assert_eq!(type_of(shipped(), got.as_deref().unwrap_or("")), DEFAULT);
    assert_eq!(asks.load(Ordering::SeqCst), 1);
    let got = model_of_with(&m, &st, unanswered(&asks), newest(&rows, None)).await;
    assert_eq!(got, None);
    assert_eq!(asks.load(Ordering::SeqCst), 2);
    let got = model_of_with(
        &m,
        &st,
        asker(&asks, Some("Qwen/Qwen3-8B")),
        newest(&rows, None),
    )
    .await;
    assert_eq!(got.as_deref(), Some("Qwen/Qwen3-8B"));
    assert_eq!(type_of(shipped(), got.as_deref().unwrap()), "hermes");
    assert_eq!(asks.load(Ordering::SeqCst), 3);
    // the answer is now kept
    let got = model_of_with(&m, &st, unanswered(&asks), newest(&rows, None)).await;
    assert_eq!(got.as_deref(), Some("Qwen/Qwen3-8B"));
    assert_eq!(asks.load(Ordering::SeqCst), 3);
}
