//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;

/// A kernel holding one `web_rest` leg "rest".
fn kernel_with_leg() -> Arc<Kernel> {
    let kernel = Arc::new(Kernel::new());
    kernel.register(Agent::new(
        AgentId::from("rest"),
        Some("web_rest.tools".into()),
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    ));
    kernel
}

/// One payload to the leg `rest`, through the bundle's entry (the call is checked
/// before the handler runs).
async fn send(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    <WebRestBundle as fantastic_kernel::Bundle>::handle_binary(
        &WebRestBundle,
        &AgentId::from("rest"),
        payload.clone(),
        None,
        kernel,
    )
    .await
    .expect("handled")
    .0
    .expect("reply")
}

/// `get_routes` declares python's three specs: `{kind:"http", method,
/// path}` (python also carries a live `endpoint` callable, which has no wire
/// spelling).
#[test]
fn get_routes_match_python() {
    let got = get_routes(&AgentId::from("rest"));
    assert_eq!(
        got,
        json!({"routes": [
            {"kind": "http", "method": "POST", "path": "/rest/{target_id}"},
            {"kind": "http", "method": "GET", "path": "/rest/_reflect"},
            {"kind": "http", "method": "GET", "path": "/rest/_reflect/{target_id}"},
        ]})
    );
}

/// An unknown verb is refused by the leg's own entry, naming what the leg answers; the
/// raw `type` is echoed back (`null` when absent, `5`, `""`). A REST leg carries calls
/// only: the other inbound verbs are not its verbs.
#[tokio::test]
async fn unknown_verb_is_unhandled_refusal() {
    let kernel = kernel_with_leg();
    for (payload, verb) in [
        (json!({"type": "frob"}), json!("frob")),
        (json!({"type": "inbound_emit"}), json!("inbound_emit")),
        (json!({}), Value::Null),
        (json!({"type": 5}), json!(5)),
        (json!({"type": ""}), json!("")),
    ] {
        let reply = send(&kernel, &payload).await;
        assert_eq!(reply["reason"], json!("unhandled"), "{payload}");
        assert_eq!(reply["verb"], verb, "{payload}");
        let error = reply["error"].as_str().expect("error text");
        assert!(
            error.starts_with("unknown verb ") && error.contains("rest answers: get_routes, inbound_call"),
            "{error}"
        );
    }
}

/// `inbound_call` is checked by its declaration: the item's `target` and `payload` are
/// required, an unknown argument is refused by name.
#[tokio::test]
async fn inbound_call_is_checked_by_its_declaration() {
    let kernel = kernel_with_leg();
    for (payload, want) in [
        (
            json!({"type": "inbound_call"}),
            "inbound_call: missing argument 'item'",
        ),
        (
            json!({"type": "inbound_call", "item": {"target": "x"}}),
            "inbound_call: missing argument 'item.payload'",
        ),
        (
            json!({"type": "inbound_call", "item": {"target": "x", "payload": {}}, "extra": 1}),
            "inbound_call: unknown argument 'extra'",
        ),
    ] {
        let reply = send(&kernel, &payload).await;
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(error.starts_with(want), "{payload} -> {reply}");
    }
}

/// The reflect frame: no `mounted_on`, no example strings,
/// the `auth_header` and the leg posture from `describe`.
#[tokio::test]
async fn reflect_keys_match_python() {
    let kernel = kernel_with_leg();
    let reply = send(&kernel, &json!({"type": "reflect"})).await;
    let obj = reply.as_object().expect("object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "path_pattern",
            "method",
            "reflect_url",
            "reflect_pattern",
            "auth_header",
            "ingress_rule",
            "egress_rule",
            "sealed",
        ]
    );
    assert_eq!(obj["sealed"], json!(true));
}

/// The one write-time check every leg makes (python
/// `validate_meta` → `io_bridge.unknown_ingress_refusal`): an unknown name or a
/// malformed spec is refused with the wire legal values; a rule it implements is
/// not.
#[test]
fn validate_meta_refuses_an_unknown_or_malformed_ingress_rule() {
    for (spec, shown) in [
        (json!("open_sesame"), "'open_sesame'"),
        (json!(7), "7"),
        (json!({"type": ["x"]}), "{'type': ['x']}"),
        (
            json!({"type": "password_ingress", "env": 7}),
            "{'type': 'password_ingress', 'env': 7}",
        ),
    ] {
        let mut meta = Map::new();
        meta.insert("ingress_rule".into(), spec);
        assert_eq!(
            WebRestBundle.validate_meta(&meta, &Map::new()),
            Some(format!(
                "ingress_rule={shown} is not a rule this bundle implements — legal \
                 values: allow_all_ingress, deny_ingress, password_ingress"
            ))
        );
    }
    let mut meta = Map::new();
    meta.insert("ingress_rule".into(), json!("password_ingress"));
    assert_eq!(WebRestBundle.validate_meta(&meta, &Map::new()), None);
}
