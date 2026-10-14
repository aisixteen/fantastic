//! Unit tests for the scenario fixtures. Every text pinned here is the python
//! probe's (`e2e/probes/*/tools.py`) — a fixture that refuses differently from
//! python makes a scenario measure the FIXTURE instead of the kernel.
//!
//! Each test has a camelCase swift twin with identical pins
//! (`ST/FantasticBundlesTests/ScenarioProbesTests.swift`).

use super::*;
use fantastic_kernel::Agent;
use serde_json::Map;

fn kernel() -> Arc<Kernel> {
    Arc::new(Kernel::new())
}

/// A registered agent carrying `meta`, so a probe can read its own record.
fn agent(kernel: &Arc<Kernel>, id: &str, meta: Value) -> AgentId {
    let map = match meta {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    let a = Agent::new(
        AgentId::from(id),
        None,
        None,
        map,
        std::env::temp_dir().join("fantastic-probe-tests"),
        true,
    );
    kernel.register(Arc::clone(&a));
    a.id.clone()
}

/// A root, so `user_probe`'s grants have somewhere to go.
fn with_root(kernel: &Arc<Kernel>) {
    let root = Agent::new(
        AgentId::from("kernel_state"),
        None,
        None,
        Map::new(),
        std::env::temp_dir().join("fantastic-probe-tests"),
        true,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
}

async fn call<B: Bundle>(b: &B, kernel: &Arc<Kernel>, id: &AgentId, payload: Value) -> Value {
    b.handle(id, &payload, kernel)
        .await
        .expect("no raise")
        .expect("a reply")
}

// ── reflect bytes ───────────────────────────────────────────────────────────

/// The plant's `count` doc is python's.
#[tokio::test]
async fn plant_reflect_is_pythons_bytes() {
    let k = kernel();
    let id = AgentId::from("flaky_plant");
    let r = call(&PlantProbe::default(), &k, &id, json!({"type": "reflect"})).await;
    assert_eq!(
        r,
        json!({
            "id": "flaky_plant",
            "sentence": "Industrial plant. Its pump is unreliable; its valve has seized.",
        })
    );
}

/// The desk's `count` doc is python's, "ever" included.
#[tokio::test]
async fn purchase_reflect_is_pythons_bytes() {
    let k = kernel();
    let id = AgentId::from("purchase_desk");
    let r = call(
        &PurchaseProbe::default(),
        &k,
        &id,
        json!({"type": "reflect"}),
    )
    .await;
    assert_eq!(
        r,
        json!({
            "id": "purchase_desk",
            "sentence": "Purchase desk. Spends real money — only a person may press the button.",
        })
    );
}

// ── refusals and raises ─────────────────────────────────────────────────────

/// The plant RAISES, and the kernel renders the exception class.
#[tokio::test]
async fn plant_failures_render_pythons_error() {
    let k = kernel();
    let id = AgentId::from("flaky_plant");
    let p = PlantProbe::default();
    for _ in 0..2 {
        let e = p
            .handle(&id, &json!({"type": "pump"}), &k)
            .await
            .expect_err("the first two pumps raise");
        assert_eq!(e.to_string(), "RuntimeError: pump stalled");
    }
    // FLAKY, not broken: the third attempt works.
    let r = call(&p, &k, &id, json!({"type": "pump"})).await;
    assert_eq!(r["pumped"], json!(true));
    let e = p
        .handle(&id, &json!({"type": "valve"}), &k)
        .await
        .expect_err("the valve never opens");
    assert_eq!(e.to_string(), "RuntimeError: valve is rusted shut");
}

/// `secrets.token_hex(3)`: six hex characters, not eight.
#[tokio::test]
async fn plant_serial_is_six_hex() {
    let k = kernel();
    let id = AgentId::from("flaky_plant");
    let p = PlantProbe::default();
    for _ in 0..2 {
        let _ = p.handle(&id, &json!({"type": "pump"}), &k).await;
    }
    let r = call(&p, &k, &id, json!({"type": "pump"})).await;
    let serial = r["serial"].as_str().expect("a serial").to_string();
    assert_eq!(serial.len(), 6, "{serial}");
    assert!(
        serial
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "{serial}"
    );
}

/// `unknown verb 'zap'`, python's repr: a quote inside the verb switches
/// the quoting, which rust's `{:?}` never does.
#[tokio::test]
async fn unknown_verb_is_python_repr() {
    let k = kernel();
    let d = AgentId::from("dispenser");
    let p = AgentId::from("flaky_plant");
    let b = AgentId::from("purchase_desk");
    assert_eq!(
        call(&DispenserProbe::default(), &k, &d, json!({"type": "zap"})).await,
        json!({"error": "dispenser_probe: unknown verb 'zap'"})
    );
    assert_eq!(
        call(&PlantProbe::default(), &k, &p, json!({"type": "zap"})).await,
        json!({"error": "plant_probe: unknown verb 'zap'"})
    );
    assert_eq!(
        call(&PurchaseProbe::default(), &k, &b, json!({"type": "zap"})).await,
        json!({"error": "purchase_probe: unknown verb 'zap'"})
    );
    assert_eq!(
        call(&DispenserProbe::default(), &k, &d, json!({"type": "it's"})).await,
        json!({"error": "dispenser_probe: unknown verb \"it's\""})
    );
}

/// The human's unknown-verb refusal, python repr and all.
#[tokio::test]
async fn user_unknown_verb_is_python_repr() {
    let k = kernel();
    let id = agent(&k, "user_unknown", json!({}));
    assert_eq!(
        call(&UserProbe::default(), &k, &id, json!({"type": "zap"})).await,
        json!({"error": "user_probe: unknown verb 'zap'", "reason": "unhandled", "verb": "zap"})
    );
}

/// The oracle names its verbs as python's `sorted(VERBS)` prints them.
#[tokio::test]
async fn oracle_unknown_verb_names_its_verbs_like_python() {
    let k = kernel();
    let id = agent(&k, "oracle_unknown", json!({}));
    assert_eq!(
        call(&OracleProbe::default(), &k, &id, json!({"type": "zap"})).await,
        json!({
            "error": "oracle_probe: unknown verb 'zap'; verbs: ['ask']",
            "reason": "unhandled",
        })
    );
}

/// The `kind` refusal lists python's list, and a present
/// non-string `kind` is refused rather than silently defaulted.
#[tokio::test]
async fn inquiry_kind_refusal_lists_python_list() {
    let k = kernel();
    let id = agent(&k, "user_kind", json!({"answer": "yes"}));
    let p = UserProbe::default();
    let refusal = json!({
        "error": "user_probe: inquiry: `kind` must be one of ['text', 'single', 'multi', 'confirm']",
        "reason": "bad_arg",
        "arg": "kind",
    });
    assert_eq!(
        call(
            &p,
            &k,
            &id,
            json!({"type": "inquiry", "text": "q", "kind": "essay"})
        )
        .await,
        refusal
    );
    // A present non-string is not in KINDS either.
    assert_eq!(
        call(
            &p,
            &k,
            &id,
            json!({"type": "inquiry", "text": "q", "kind": 5})
        )
        .await,
        refusal
    );
    assert_eq!(
        call(
            &p,
            &k,
            &id,
            json!({"type": "inquiry", "text": "q", "kind": null})
        )
        .await,
        refusal
    );
    // An ABSENT kind takes python's `"text"` default and passes.
    let ok = call(&p, &k, &id, json!({"type": "inquiry", "text": "q"})).await;
    assert_eq!(ok["accepted"], json!(true), "{ok}");
}

/// `send` refuses a payload whose `type` is falsy, not just absent.
#[tokio::test]
async fn user_send_refuses_a_falsy_type() {
    let k = kernel();
    let id = agent(&k, "user_send", json!({}));
    let p = UserProbe::default();
    let refusal = json!({
        "error": "user_probe: send: `payload` must be a dict naming a `type`",
        "reason": "bad_arg",
        "arg": "payload",
    });
    for t in [json!(""), json!(0), json!(null), json!(false)] {
        assert_eq!(
            call(
                &p,
                &k,
                &id,
                json!({"type": "send", "target": "x", "payload": {"type": t}})
            )
            .await,
            refusal,
            "a falsy type must be refused: {t}"
        );
    }
}

// ── tallies ─────────────────────────────────────────────────────────────────

/// The inquiry reply carries `granted`, the count of grants APPLIED. The
/// reply is the ACCEPTANCE (fully async); the answer itself
/// arrives later as the human's own message under the inquiry's pid.
#[tokio::test]
async fn inquiry_reply_carries_granted() {
    let k = kernel();
    with_root(&k);
    let p = UserProbe::default();
    let plain = agent(&k, "user_plain", json!({"answer": "yes"}));
    assert_eq!(
        call(&p, &k, &plain, json!({"type": "inquiry", "text": "ok?"})).await,
        json!({"accepted": true, "re": null, "request_id": null, "inquiries": 1, "granted": 0})
    );
    // One grant on the record, applied before the answer.
    let granting = agent(
        &k,
        "user_granting",
        json!({"answer": "yes", "grants": [{"type": "reflect", "tree": "none"}]}),
    );
    let r = call(&p, &k, &granting, json!({"type": "inquiry", "text": "ok?"})).await;
    assert_eq!(r["granted"], json!(1));
    assert_eq!(r["accepted"], json!(true));
}

/// Two humans keep two tallies, and their proposals stay apart.
#[tokio::test]
async fn user_tallies_are_per_agent() {
    let k = kernel();
    with_root(&k);
    let p = UserProbe::default();
    let u1 = agent(&k, "u1", json!({"answer": "a1"}));
    let u2 = agent(&k, "u2", json!({"answer": "a2"}));
    let act1 = json!({"target": "purchase_desk", "payload": {"type": "buy"}});
    let r1 = call(
        &p,
        &k,
        &u1,
        json!({"type": "inquiry", "text": "q", "action": act1}),
    )
    .await;
    let r2 = call(&p, &k, &u2, json!({"type": "inquiry", "text": "q"})).await;
    assert_eq!(r1["inquiries"], json!(1));
    assert_eq!(r2["inquiries"], json!(1));
    assert_eq!(
        call(&p, &k, &u1, json!({"type": "inquiries"})).await,
        json!({"inquiries": 1, "proposals": [act1]})
    );
    assert_eq!(
        call(&p, &k, &u2, json!({"type": "inquiries"})).await,
        json!({"inquiries": 1, "proposals": []})
    );
    // …and the reflect tally is the same per-agent count.
    assert_eq!(
        call(&p, &k, &u2, json!({"type": "reflect"})).await["inquiries"],
        json!(1)
    );
}

/// Two oracles keep two tallies.
#[tokio::test]
async fn oracle_tallies_per_agent() {
    let k = kernel();
    let p = OracleProbe::default();
    let o1 = agent(&k, "o1", json!({"lines": ["Dana is in room 4"]}));
    let o2 = agent(&k, "o2", json!({"lines": ["Dana is in room 4"]}));
    for _ in 0..2 {
        let _ = call(
            &p,
            &k,
            &o1,
            json!({"type": "ask", "question": "which room Dana"}),
        )
        .await;
    }
    let _ = call(
        &p,
        &k,
        &o2,
        json!({"type": "ask", "question": "which room Dana"}),
    )
    .await;
    assert_eq!(
        call(&p, &k, &o1, json!({"type": "reflect"})).await["asked"],
        json!(2)
    );
    assert_eq!(
        call(&p, &k, &o2, json!({"type": "reflect"})).await["asked"],
        json!(1)
    );
}

// ── oracle rendering and coercion ───────────────────────────────────────────

/// A non-string ledger line is rendered with python's `str()`: a dict
/// reads `{\'a\': \'alpha\'}`, not JSON's `{"a":"alpha"}`.
#[tokio::test]
async fn oracle_renders_non_string_lines_with_python_str() {
    let k = kernel();
    let id = agent(
        &k,
        "oracle_str",
        json!({"lines": [777, true, null, {"a": "alpha"}]}),
    );
    let r = call(
        &OracleProbe::default(),
        &k,
        &id,
        json!({"type": "ask", "question": "true none 777 alpha"}),
    )
    .await;
    // One hit each, so python's `(-hits, line)` sort orders them by the RENDERED
    // line — which is the rendering this test is about.
    assert_eq!(
        r,
        json!({
            "answers": ["777", "None", "True", "{\'a\': \'alpha\'}"],
            "matched": 4,
        })
    );
}

/// `limit` is python's `int(x or 6)`, clamped to 1..=20, and an
/// unparsable value falls back to 6 rather than raising.
#[tokio::test]
async fn oracle_limit_coerces_like_python() {
    let k = kernel();
    let lines: Vec<String> = (0..25).map(|i| format!("fact number {i} here")).collect();
    let id = agent(&k, "oracle_limit", json!({"lines": lines}));
    let p = OracleProbe::default();
    for (limit, want) in [
        (json!("3"), 3),
        (json!(0), 6),
        (json!("x"), 6),
        (json!(99), 20),
        (json!(2.9), 2),
        (json!(true), 1),
        (json!(-5), 1),
        (json!(null), 6),
    ] {
        let r = call(
            &p,
            &k,
            &id,
            json!({"type": "ask", "question": "fact here", "limit": limit}),
        )
        .await;
        assert_eq!(
            r["answers"].as_array().expect("answers").len(),
            want,
            "limit {limit} must page {want}"
        );
        assert_eq!(r["matched"], json!(25));
    }
    // Absent is python's `or 6` too.
    let r = call(&p, &k, &id, json!({"type": "ask", "question": "fact here"})).await;
    assert_eq!(r["answers"].as_array().expect("answers").len(), 6);
}

// ── slow ────────────────────────────────────────────────────────────────────

/// The slow probe's reflect is python's, verb order included.
#[tokio::test]
async fn slow_reflect_is_pythons_bytes() {
    let k = kernel();
    let id = AgentId::from("slow");
    let r = call(&SlowProbe::default(), &k, &id, json!({"type": "reflect"})).await;
    assert_eq!(
        serde_json::to_string(&r).unwrap(),
        serde_json::to_string(&json!({
            "id": "slow",
            "sentence": "Probe work: `slow` (never finishes by itself), `work` (counts), \
                         `fail` (raises), `sleep` (takes `seconds`).",
        }))
        .unwrap()
    );
}

/// `sleep` takes the stated time and echoes the value; a missing, boolean or
/// negative `seconds` is refused naming the arg; `fail` raises python's class.
#[tokio::test]
async fn slow_sleep_work_and_fail_are_pythons() {
    let k = kernel();
    let id = AgentId::from("slow");
    let p = SlowProbe::default();
    let t0 = std::time::Instant::now();
    let r = call(&p, &k, &id, json!({"type": "sleep", "seconds": 0.2})).await;
    assert_eq!(r, json!({"slept": 0.2}));
    assert!(t0.elapsed() >= std::time::Duration::from_millis(200));
    assert_eq!(
        call(&p, &k, &id, json!({"type": "sleep", "seconds": 0})).await,
        json!({"slept": 0})
    );
    for bad in [json!(null), json!(true), json!(-1), json!("2")] {
        let r = call(&p, &k, &id, json!({"type": "sleep", "seconds": bad})).await;
        assert_eq!(
            r,
            json!({
                "error": "slow_probe: sleep needs seconds, a non-negative number",
                "reason": "bad_arg",
                "arg": "seconds",
            }),
            "{bad}"
        );
    }
    assert_eq!(
        call(&p, &k, &id, json!({"type": "work"})).await,
        json!({"runs": 1})
    );
    assert_eq!(
        call(&p, &k, &id, json!({"type": "started"})).await,
        json!({"started": 0})
    );
    let err = p
        .handle(&id, &json!({"type": "fail"}), &k)
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "RuntimeError: slow_probe: failed on purpose"
    );
    assert_eq!(
        call(&p, &k, &id, json!({"type": "zap"})).await,
        json!({"error": "slow_probe: unknown verb 'zap'", "reason": "unhandled"})
    );
}
