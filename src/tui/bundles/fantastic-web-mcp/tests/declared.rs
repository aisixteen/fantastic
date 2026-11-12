//! THE LEG'S DECLARED VERBS (issue #642), against a real kernel: every call is checked
//! by the agent's own entry before any handler runs — an unknown verb is refused naming
//! what the leg answers, an unknown, missing or wrongly typed argument by name — and
//! `reflect` and `docs` render the declarations.

use std::sync::Arc;

use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_web_mcp::{verbs, WebMcpBundle, HANDLER_MODULE};
use serde_json::{json, Value};

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

/// A kernel with the leg `mcp` (it may call only itself).
async fn setup() -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(HANDLER_MODULE, WebMcpBundle);
    let k = Arc::clone(&bootstrap(reg, BootstrapOptions::in_memory()).expect("boots").kernel);
    let root = k.root().expect("root").id.0.clone();
    let r = op(
        &k,
        &root,
        json!({"type": "create_agent", "parent_id": root, "id": "mcp", "handler_module": HANDLER_MODULE,
               "can_call": {}}),
    )
    .await;
    assert!(r.get("error").is_none(), "create mcp: {r}");
    k
}

fn why(r: &Value) -> (Option<&str>, Option<&str>) {
    (r["reason"].as_str(), r["arg"].as_str())
}

#[tokio::test(flavor = "multi_thread")]
async fn every_verb_is_checked_by_its_declaration() {
    let k = setup().await;
    let r = op(&k, "mcp", json!({"type": "zzz_nope"})).await;
    assert_eq!(r["reason"], "unhandled", "{r}");
    for v in verbs::VERBS.iter().map(|v| v.name).chain(["reflect"]) {
        assert!(r["error"].as_str().unwrap().contains(v), "{v} in {r}");
    }
    for v in verbs::VERBS {
        let r = op(&k, "mcp", json!({"type": v.name, "zzz_unknown": 1})).await;
        assert_eq!(why(&r), (Some("bad_arg"), Some("zzz_unknown")), "{}: {r}", v.name);
    }
    for (verb, arg) in [("inbound_send", "items"), ("inquiry", "text"), ("answer_inquiry", "re")] {
        let r = op(&k, "mcp", json!({"type": verb})).await;
        assert_eq!(why(&r), (Some("missing_arg"), Some(arg)), "{verb}: {r}");
    }
    // wrongly typed, never coerced
    for (p, arg) in [
        (json!({"type": "inbound_send", "items": "x"}), "items"),
        (json!({"type": "inquiry", "text": 3}), "text"),
        (json!({"type": "inquiry", "text": "  "}), "text"),
        (json!({"type": "inquiry", "text": "q", "kind": "poll"}), "kind"),
        (json!({"type": "inquiry", "text": "q", "options": []}), "options"),
        (json!({"type": "inquiry", "text": "q", "reply_client_id": ""}), "reply_client_id"),
        (json!({"type": "inquiries", "asker": 5}), "asker"),
        (json!({"type": "answer_inquiry", "re": "7", "answer": "yes"}), "re"),
        (json!({"type": "answer_inquiry", "re": 7, "answer": 4}), "answer"),
        (json!({"type": "answer_inquiry", "re": 7, "answer": ""}), "answer"),
    ] {
        let r = op(&k, "mcp", p.clone()).await;
        assert_eq!(why(&r), (Some("bad_arg"), Some(arg)), "{p}: {r}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn inbound_send_sends_the_batch_as_the_leg() {
    let k = setup().await;
    let r = op(
        &k,
        "mcp",
        json!({"type": "inbound_send", "items": [{"target": "mcp", "payload": {"type": "inquiries"}, "timeout": 5}]}),
    )
    .await;
    assert_eq!(r["outcomes"][0]["status"], "completed", "{r}");
    assert_eq!(r["outcomes"][0]["result"], json!({"inquiries": []}), "{r}");
    // an item is a declared shape too
    let r = op(&k, "mcp", json!({"type": "inbound_send", "items": [{"target": "kernel"}]})).await;
    assert_eq!(why(&r), (Some("bad_arg"), None), "{r}");
    assert!(r["error"].as_str().unwrap().starts_with("inbound_send: the arguments do not parse"), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_choice_question_keeps_its_options() {
    let k = setup().await;
    let r = op(&k, "mcp", json!({"type": "inquiry", "text": "which?", "kind": "single", "options": ["a", "b"]})).await;
    assert_eq!(r["accepted"], true, "{r}");
    let r = op(&k, "mcp", json!({"type": "inquiries"})).await;
    assert_eq!(r["inquiries"][0]["kind"], "single", "{r}");
    assert_eq!(r["inquiries"][0]["reply_client_id"], "cli", "the declared default: {r}");
    assert_eq!(r["inquiries"][0]["options"], json!(["a", "b"]), "{r}");
}

#[tokio::test(flavor = "multi_thread")]
async fn reflect_and_docs_render_the_declarations_and_the_record_is_declared() {
    let k = setup().await;
    let r = op(&k, "mcp", json!({"type": "reflect"})).await;
    assert_eq!(r["id"], "mcp", "{r}");
    for v in ["inbound_send", "inquiry", "inquiries", "answer_inquiry", "boot", "reflect"] {
        assert!(r["verbs"].get(v).is_some(), "{v} in {r}");
    }
    let d = op(&k, "kernel", json!({"type": "docs", "of": "mcp", "tier": "detailed"})).await;
    assert!(d["verbs"]["answer_inquiry"].as_str().unwrap().contains("re:int (req)"), "{d}");
    assert!(d["man"].as_str().is_some_and(|m| !m.is_empty()), "{d}");
    // the record: its three fields and no other
    let root = k.root().unwrap().id.0.clone();
    let ok = op(&k, &root, json!({"type": "update_agent", "id": "mcp", "file_bridge_id": "cfg", "mcp_config_file": ".mcp.json"})).await;
    assert!(ok.get("error").is_none(), "{ok}");
    for p in [
        json!({"type": "update_agent", "id": "mcp", "stray": 1}),
        json!({"type": "update_agent", "id": "mcp", "file_bridge_id": 3}),
    ] {
        let r = op(&k, &root, p.clone()).await;
        assert_eq!(r["reason"], "bad_arg", "{p}: {r}");
    }
}
