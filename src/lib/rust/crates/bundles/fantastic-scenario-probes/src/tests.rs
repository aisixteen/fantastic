//! Unit tests for the scenario fixtures. Every call goes through the kernel, so each
//! probe's own entry (`Call::parse`) judges it the way it does in a scenario: a
//! fixture that refuses differently from python makes a scenario measure the
//! FIXTURE instead of the kernel.

use super::*;
use fantastic_kernel::{Agent, Item};
use serde_json::Map;

/// A kernel with every probe registered under its module, and a root.
fn kernel() -> Arc<Kernel> {
    let mut k = Kernel::new();
    k.bundles
        .register(DISPENSER_MODULE, DispenserProbe::default());
    k.bundles.register(PLANT_MODULE, PlantProbe::default());
    k.bundles
        .register(PURCHASE_MODULE, PurchaseProbe::default());
    k.bundles.register(USER_MODULE, UserProbe::default());
    k.bundles.register(ORACLE_MODULE, OracleProbe::default());
    k.bundles.register(SLOW_MODULE, SlowProbe::default());
    k.bundles.register(VAULT_MODULE, VaultProbe::default());
    let k = Arc::new(k);
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        json!({"callable_by": {"user_send": "*", "user_granting": "*"}})
            .as_object()
            .cloned()
            .unwrap(),
        std::env::temp_dir().join("fantastic-probe-tests"),
        true,
    );
    k.register(Arc::clone(&root));
    k.set_root(root);
    k
}

/// Every caller may reach the agent, and the agent may reach the root.
fn wide_open(mut meta: Map<String, Value>) -> Map<String, Value> {
    meta.insert("callable_by".to_string(), json!({"*": "*"}));
    meta.insert("can_call".to_string(), json!({"kernel_state": "*"}));
    meta
}

/// A registered agent of `module` carrying `meta`, so a probe can read its own record.
fn agent(kernel: &Arc<Kernel>, id: &str, module: &str, meta: Value) -> AgentId {
    let map = match meta {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let a = Agent::new(
        AgentId::from(id),
        Some(module.to_string()),
        Some(AgentId::from("kernel_state")),
        wide_open(map),
        std::env::temp_dir().join("fantastic-probe-tests"),
        true,
    );
    kernel.register(Arc::clone(&a));
    a.id.clone()
}

async fn call(kernel: &Arc<Kernel>, id: &AgentId, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(id.clone(), payload)])
        .await
        .remove(0)
        .result
}

// ── reflect ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn plant_reflect_names_the_plant() {
    let k = kernel();
    let id = agent(&k, "flaky_plant", PLANT_MODULE, json!({}));
    let r = call(&k, &id, json!({"type": "reflect"})).await;
    assert_eq!(r["id"], "flaky_plant", "{r}");
    assert_eq!(
        r["sentence"], "Industrial plant. Its pump is unreliable; its valve has seized.",
        "{r}"
    );
}

#[tokio::test]
async fn purchase_reflect_names_the_desk() {
    let k = kernel();
    let id = agent(&k, "purchase_desk", PURCHASE_MODULE, json!({}));
    let r = call(&k, &id, json!({"type": "reflect"})).await;
    assert_eq!(
        r["sentence"], "Purchase desk. Spends real money — only a person may press the button.",
        "{r}"
    );
}

#[tokio::test]
async fn slow_reflect_names_the_work() {
    let k = kernel();
    let id = agent(&k, "slow", SLOW_MODULE, json!({}));
    let r = call(&k, &id, json!({"type": "reflect"})).await;
    assert_eq!(
        r["sentence"],
        "Probe work: `slow` (never finishes by itself), `work` (counts), \
         `fail` (raises), `sleep` (takes `seconds`).",
        "{r}"
    );
}

/// Every verb of every probe is rendered by `reflect` from its declaration.
#[tokio::test]
async fn reflect_renders_every_declared_verb() {
    let k = kernel();
    let rows: [(&str, &str, &[&str]); 7] = [
        ("d", DISPENSER_MODULE, &["dispense", "count"]),
        ("p", PLANT_MODULE, &["pump", "valve", "count"]),
        ("b", PURCHASE_MODULE, &["buy", "count"]),
        ("u", USER_MODULE, &["inquiry", "inquiries", "send"]),
        ("o", ORACLE_MODULE, &["ask"]),
        (
            "s",
            SLOW_MODULE,
            &["slow", "work", "fail", "started", "sleep"],
        ),
        ("v", VAULT_MODULE, &["open", "count"]),
    ];
    for (id, module, verbs) in rows {
        let id = agent(&k, id, module, json!({}));
        let r = call(&k, &id, json!({"type": "reflect"})).await;
        for verb in verbs {
            assert!(r["verbs"].get(verb).is_some(), "{module} {verb}: {r}");
        }
    }
}

/// The vault's detailed docs teach the code; its one-liner does not.
#[tokio::test]
async fn vault_docs_teach_the_code_only_in_detail() {
    let k = kernel();
    let id = agent(
        &k,
        "vault",
        VAULT_MODULE,
        json!({"serial": "1", "secret": "s"}),
    );
    let r = call(&k, &id, json!({"type": "reflect"})).await;
    let line = r["verbs"]["open"].as_str().unwrap();
    assert!(line.contains("code:string (req)"), "{line}");
    assert!(!line.contains("BACKWARDS"), "{line}");
    let d = call(
        &k,
        &AgentId::from("kernel_state"),
        json!({"type": "docs", "of": "vault", "verb": "open", "tier": "detailed"}),
    )
    .await;
    assert!(d.to_string().contains("BACKWARDS"), "{d}");
}

// ── refusals and raises ─────────────────────────────────────────────────────

/// The plant RAISES, and the kernel renders the exception class.
#[tokio::test]
async fn plant_failures_render_the_error() {
    let k = kernel();
    let id = agent(&k, "flaky_plant", PLANT_MODULE, json!({}));
    for _ in 0..2 {
        let r = call(&k, &id, json!({"type": "pump"})).await;
        assert!(
            r["error"]
                .as_str()
                .unwrap()
                .contains("RuntimeError: pump stalled"),
            "the first two pumps raise: {r}"
        );
    }
    // FLAKY, not broken: the third attempt works.
    let r = call(&k, &id, json!({"type": "pump"})).await;
    assert_eq!(r["pumped"], json!(true), "{r}");
    let r = call(&k, &id, json!({"type": "valve"})).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("RuntimeError: valve is rusted shut"),
        "{r}"
    );
    // pump x3 + valve
    assert_eq!(call(&k, &id, json!({"type": "count"})).await["count"], 4);
}

/// `secrets.token_hex(3)`: six hex characters, not eight.
#[tokio::test]
async fn plant_serial_is_six_hex() {
    let k = kernel();
    let id = agent(&k, "flaky_plant", PLANT_MODULE, json!({}));
    for _ in 0..2 {
        let _ = call(&k, &id, json!({"type": "pump"})).await;
    }
    let r = call(&k, &id, json!({"type": "pump"})).await;
    let serial = r["serial"].as_str().expect("a serial").to_string();
    assert_eq!(serial.len(), 6, "{serial}");
    assert!(
        serial
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{serial}"
    );
}

/// The dispenser and the desk count their side effects and echo their argument.
#[tokio::test]
async fn dispenser_and_desk_count_what_they_do() {
    let k = kernel();
    let d = agent(&k, "dispenser", DISPENSER_MODULE, json!({}));
    let r = call(&k, &d, json!({"type": "dispense", "job": "j1"})).await;
    assert_eq!(r["coin"], 1, "{r}");
    assert_eq!(r["job"], "j1", "{r}");
    assert_eq!(r["receipt"].as_str().unwrap().len(), 8, "{r}");
    let _ = call(&k, &d, json!({"type": "dispense"})).await;
    assert_eq!(call(&k, &d, json!({"type": "count"})).await["count"], 2);
    let b = agent(&k, "purchase_desk", PURCHASE_MODULE, json!({}));
    let r = call(&k, &b, json!({"type": "buy", "item": "tea"})).await;
    assert_eq!(r["bought"], "tea", "{r}");
    assert_eq!(r["n"], 1, "{r}");
    assert_eq!(call(&k, &b, json!({"type": "count"})).await["count"], 1);
}

/// An unknown verb is refused by each probe's own entry, naming what it answers; an
/// argument it does not take, or one of the wrong type, is refused by name.
#[tokio::test]
async fn every_probe_refuses_by_its_own_entry() {
    let k = kernel();
    let d = agent(&k, "dispenser", DISPENSER_MODULE, json!({}));
    let r = call(&k, &d, json!({"type": "zap"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    assert_eq!(r["verb"], "zap", "{r}");
    assert!(r["error"].as_str().unwrap().contains("dispense"), "{r}");
    let r = call(&k, &d, json!({"type": "dispense", "jobs": "x"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "jobs", "{r}");
    let r = call(&k, &d, json!({"type": "dispense", "job": 5})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "job", "{r}");
    for (id, module, listed) in [
        ("p", PLANT_MODULE, "pump"),
        ("b", PURCHASE_MODULE, "buy"),
        ("u", USER_MODULE, "inquiry"),
        ("o", ORACLE_MODULE, "ask"),
        ("s", SLOW_MODULE, "sleep"),
        ("v", VAULT_MODULE, "open"),
    ] {
        let id = agent(&k, id, module, json!({}));
        let r = call(&k, &id, json!({"type": "zap"})).await;
        assert_eq!(r["reason"], "unhandled", "{module}: {r}");
        assert_eq!(r["verb"], "zap", "{module}: {r}");
        assert!(
            r["error"].as_str().unwrap().contains(listed),
            "{module}: {r}"
        );
    }
}

/// `kind` is one of the four shapes, refused by name otherwise; an absent one passes.
#[tokio::test]
async fn inquiry_kind_is_one_of_the_four_shapes() {
    let k = kernel();
    let id = agent(&k, "user_kind", USER_MODULE, json!({"answer": "yes"}));
    for kind in [json!("essay"), json!(5), json!("Text")] {
        let r = call(
            &k,
            &id,
            json!({"type": "inquiry", "text": "q", "reply_client_id": "cli", "kind": kind.clone()}),
        )
        .await;
        assert_eq!(r["reason"], "bad_arg", "{kind}: {r}");
        assert_eq!(r["arg"], "kind", "{kind}: {r}");
    }
    for kind in ["text", "single", "multi", "confirm"] {
        let r = call(
            &k,
            &id,
            json!({"type": "inquiry", "text": "q", "reply_client_id": "cli", "kind": kind}),
        )
        .await;
        assert_eq!(r["accepted"], json!(true), "{kind}: {r}");
    }
    let ok = call(&k, &id, json!({"type": "inquiry", "text": "q", "reply_client_id": "cli"})).await;
    assert_eq!(ok["accepted"], json!(true), "{ok}");
}

/// An inquiry needs text; a blank one is refused naming `text`.
#[tokio::test]
async fn inquiry_text_is_required_and_not_blank() {
    let k = kernel();
    let id = agent(&k, "user_text", USER_MODULE, json!({"answer": "yes"}));
    let r = call(&k, &id, json!({"type": "inquiry", "reply_client_id": "cli"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
    assert_eq!(r["arg"], "text", "{r}");
    // the reply client defaults to `cli`
    let r = call(&k, &id, json!({"type": "inquiry", "text": "q"})).await;
    assert_eq!(r["accepted"], true, "{r}");
    for blank in ["", "  \n\t "] {
        let r = call(&k, &id, json!({"type": "inquiry", "text": blank, "reply_client_id": "cli"})).await;
        assert_eq!(r["arg"], "text", "{blank:?}: {r}");
    }
    // an unwired human says so
    let bare = agent(&k, "user_bare", USER_MODULE, json!({}));
    let r = call(&k, &bare, json!({"type": "inquiry", "text": "q", "reply_client_id": "cli"})).await;
    assert_eq!(r["reason"], "not_connected", "{r}");
}

/// `send` needs a target and a payload that names a `type`.
#[tokio::test]
async fn user_send_needs_a_target_and_a_typed_payload() {
    let k = kernel();
    let id = agent(&k, "user_send", USER_MODULE, json!({}));
    for payload in [
        json!({}),
        json!({"type": 0}),
        json!({"type": null}),
        json!({"type": false}),
        json!({"type": ""}),
    ] {
        let r = call(
            &k,
            &id,
            json!({"type": "send", "target": "x", "payload": payload.clone()}),
        )
        .await;
        assert!(
            r["reason"] == "bad_arg" || r["reason"] == "missing_arg",
            "{payload}: {r}"
        );
        assert!(
            r["arg"].as_str().unwrap().starts_with("payload"),
            "{payload}: {r}"
        );
    }
    let r = call(
        &k,
        &id,
        json!({"type": "send", "target": "", "payload": {"type": "reflect"}}),
    )
    .await;
    assert_eq!(r["arg"], "target", "{r}");
    // a good one is dispatched as the human
    let r = call(
        &k,
        &id,
        json!({"type": "send", "target": "kernel_state", "payload": {"type": "reflect", "tree": "none"}}),
    )
    .await;
    assert_eq!(r["sent"], "kernel_state", "{r}");
    assert_eq!(r["result"]["id"], "kernel_state", "{r}");
}

// ── tallies ─────────────────────────────────────────────────────────────────

/// The inquiry reply carries `granted`, the count of grants APPLIED. The reply is
/// the ACCEPTANCE (fully async); the answer itself arrives later as the human's own
/// message under the inquiry's pid.
#[tokio::test]
async fn inquiry_reply_carries_granted() {
    let k = kernel();
    let plain = agent(&k, "user_plain", USER_MODULE, json!({"answer": "yes"}));
    let r = call(&k, &plain, json!({"type": "inquiry", "text": "ok?", "reply_client_id": "cli"})).await;
    assert_eq!(r["accepted"], json!(true), "{r}");
    assert_eq!(r["inquiries"], json!(1), "{r}");
    assert_eq!(r["granted"], json!(0), "{r}");
    assert_eq!(r["request_id"], Value::Null, "{r}");
    // One grant on the record, applied before the answer.
    let granting = agent(
        &k,
        "user_granting",
        USER_MODULE,
        json!({"answer": "yes", "grants": [{"type": "reflect", "tree": "none"}]}),
    );
    let r = call(&k, &granting, json!({"type": "inquiry", "text": "ok?", "reply_client_id": "cli"})).await;
    assert_eq!(r["granted"], json!(1), "{r}");
    assert_eq!(r["accepted"], json!(true), "{r}");
}

/// Two humans keep two tallies, and their proposals stay apart.
#[tokio::test]
async fn user_tallies_are_per_agent() {
    let k = kernel();
    let u1 = agent(&k, "u1", USER_MODULE, json!({"answer": "a1"}));
    let u2 = agent(&k, "u2", USER_MODULE, json!({"answer": "a2"}));
    let act1 = json!({"target": "purchase_desk", "payload": {"type": "buy"}});
    let r1 = call(
        &k,
        &u1,
        json!({"type": "inquiry", "text": "q", "reply_client_id": "cli", "action": act1}),
    )
    .await;
    let r2 = call(&k, &u2, json!({"type": "inquiry", "text": "q", "reply_client_id": "cli"})).await;
    assert_eq!(r1["inquiries"], json!(1), "{r1}");
    assert_eq!(r2["inquiries"], json!(1), "{r2}");
    assert_eq!(
        call(&k, &u1, json!({"type": "inquiries"})).await,
        json!({"inquiries": 1, "proposals": [act1]})
    );
    assert_eq!(
        call(&k, &u2, json!({"type": "inquiries"})).await,
        json!({"inquiries": 1, "proposals": []})
    );
    // …and the reflect tally is the same per-agent count.
    assert_eq!(
        call(&k, &u2, json!({"type": "reflect"})).await["inquiries"],
        json!(1)
    );
}

/// Two oracles keep two tallies.
#[tokio::test]
async fn oracle_tallies_per_agent() {
    let k = kernel();
    let o1 = agent(
        &k,
        "o1",
        ORACLE_MODULE,
        json!({"lines": ["Dana is in room 4"]}),
    );
    let o2 = agent(
        &k,
        "o2",
        ORACLE_MODULE,
        json!({"lines": ["Dana is in room 4"]}),
    );
    for _ in 0..2 {
        let _ = call(
            &k,
            &o1,
            json!({"type": "ask", "question": "which room Dana", "limit": 6}),
        )
        .await;
    }
    let _ = call(
        &k,
        &o2,
        json!({"type": "ask", "question": "which room Dana", "limit": 6}),
    )
    .await;
    assert_eq!(
        call(&k, &o1, json!({"type": "reflect"})).await["asked"],
        json!(2)
    );
    assert_eq!(
        call(&k, &o2, json!({"type": "reflect"})).await["asked"],
        json!(1)
    );
}

// ── oracle ──────────────────────────────────────────────────────────────────

/// A non-string ledger line is rendered with python's `str()`: a dict reads
/// `{'a': 'alpha'}`, not JSON's `{"a":"alpha"}`.
#[tokio::test]
async fn oracle_renders_non_string_lines_with_python_str() {
    let k = kernel();
    let id = agent(
        &k,
        "oracle_str",
        ORACLE_MODULE,
        json!({"lines": [777, true, null, {"a": "alpha"}]}),
    );
    let r = call(
        &k,
        &id,
        json!({"type": "ask", "question": "true none 777 alpha", "limit": 6}),
    )
    .await;
    // One hit each, so python's `(-hits, line)` sort orders them by the RENDERED
    // line — which is the rendering this test is about.
    assert_eq!(
        r,
        json!({
            "answers": ["777", "None", "True", "{'a': 'alpha'}"],
            "matched": 4,
        })
    );
}

/// `limit` is an integer, clamped to 1..=20, six when absent or 0; a value of another
/// type is refused by name, never coerced.
#[tokio::test]
async fn oracle_limit_is_clamped_and_typed() {
    let k = kernel();
    let lines: Vec<String> = (0..25).map(|i| format!("fact number {i} here")).collect();
    let id = agent(&k, "oracle_limit", ORACLE_MODULE, json!({"lines": lines}));
    for (limit, want) in [
        (json!(3), 3),
        (json!(0), 6),
        (json!(99), 20),
        (json!(-5), 1),
    ] {
        let r = call(
            &k,
            &id,
            json!({"type": "ask", "question": "fact here", "limit": limit}),
        )
        .await;
        assert_eq!(
            r["answers"].as_array().expect("answers").len(),
            want,
            "limit {limit} must page {want}: {r}"
        );
        assert_eq!(r["matched"], json!(25));
    }
    for wrong in [json!("3"), json!(2.9), json!(true), json!("x")] {
        let r = call(
            &k,
            &id,
            json!({"type": "ask", "question": "fact here", "limit": wrong.clone()}),
        )
        .await;
        assert_eq!(r["reason"], "bad_arg", "{wrong}: {r}");
        assert_eq!(r["arg"], "limit", "{wrong}: {r}");
    }
    let r = call(&k, &id, json!({"type": "ask", "question": "fact here"})).await;
    assert_eq!(r["answers"].as_array().expect("answers").len(), 6, "{r}");
    let r = call(&k, &id, json!({"type": "ask", "question": "fact here", "limit": 6})).await;
    assert_eq!(r["answers"].as_array().expect("answers").len(), 6);
    // a question is text; a blank one is refused, an unwired oracle says so
    for blank in ["", " \t\n"] {
        let r = call(&k, &id, json!({"type": "ask", "question": blank, "limit": 6})).await;
        assert_eq!(r["arg"], "question", "{blank:?}: {r}");
    }
    let r = call(&k, &id, json!({"type": "ask", "question": 5, "limit": 6})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let bare = agent(&k, "oracle_bare", ORACLE_MODULE, json!({}));
    let r = call(&k, &bare, json!({"type": "ask", "question": "q q q", "limit": 6})).await;
    assert_eq!(r["reason"], "not_connected", "{r}");
}

// ── vault ───────────────────────────────────────────────────────────────────

/// The vault opens with `V-` and its serial reversed, and counts its openings.
#[tokio::test]
async fn vault_opens_only_with_the_right_code() {
    let k = kernel();
    let id = agent(
        &k,
        "vault",
        VAULT_MODULE,
        json!({"serial": "123", "secret": "gold"}),
    );
    let r = call(&k, &id, json!({"type": "open", "code": "nope"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "code", "{r}");
    assert_eq!(call(&k, &id, json!({"type": "count"})).await["opened"], 0);
    let r = call(&k, &id, json!({"type": "open", "code": " V-321 "})).await;
    assert_eq!(r, json!({"secret": "gold"}));
    assert_eq!(call(&k, &id, json!({"type": "count"})).await["opened"], 1);
    let r = call(&k, &id, json!({"type": "reflect"})).await;
    assert_eq!(r["serial"], "123", "{r}");
    assert_eq!(r["opened"], 1, "{r}");
    let r = call(&k, &id, json!({"type": "open"})).await;
    assert_eq!(r["reason"], "missing_arg", "{r}");
}

// ── slow ────────────────────────────────────────────────────────────────────

/// `sleep` takes the stated time and echoes the value; a missing, boolean, text or
/// negative `seconds` is refused naming the arg; `fail` raises python's class.
#[tokio::test]
async fn slow_sleep_work_and_fail_are_pythons() {
    let k = kernel();
    let id = agent(&k, "slow", SLOW_MODULE, json!({}));
    let t0 = std::time::Instant::now();
    let r = call(&k, &id, json!({"type": "sleep", "seconds": 0.2})).await;
    assert_eq!(r, json!({"slept": 0.2}));
    assert!(t0.elapsed() >= std::time::Duration::from_millis(200));
    assert_eq!(
        call(&k, &id, json!({"type": "sleep", "seconds": 0})).await,
        json!({"slept": 0})
    );
    for bad in [json!(null), json!(true), json!("2")] {
        let r = call(&k, &id, json!({"type": "sleep", "seconds": bad.clone()})).await;
        assert!(
            r["reason"] == "bad_arg" || r["reason"] == "missing_arg",
            "{bad}: {r}"
        );
        assert_eq!(r["arg"], "seconds", "{bad}: {r}");
    }
    let r = call(&k, &id, json!({"type": "sleep", "seconds": -1})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    assert_eq!(r["arg"], "seconds", "{r}");
    assert_eq!(
        call(&k, &id, json!({"type": "work"})).await,
        json!({"runs": 1})
    );
    assert_eq!(
        call(&k, &id, json!({"type": "started"})).await,
        json!({"started": 0})
    );
    let r = call(&k, &id, json!({"type": "fail"})).await;
    assert!(
        r["error"]
            .as_str()
            .unwrap()
            .contains("RuntimeError: slow_probe: failed on purpose"),
        "{r}"
    );
}
