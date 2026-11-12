//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;

/// A kernel holding one `web_ws` leg "ws" with the given record fields.
fn kernel_with_leg(meta: Value) -> Arc<Kernel> {
    let kernel = Arc::new(Kernel::new());
    kernel.register(Agent::new(
        AgentId::from("ws"),
        Some("web_ws.tools".into()),
        None,
        meta.as_object().cloned().unwrap_or_default(),
        std::path::PathBuf::new(),
        false,
    ));
    kernel
}

/// One payload to the leg `ws`, through the bundle's entry (the call is checked before
/// the handler runs).
async fn send(kernel: &Arc<Kernel>, payload: &Value) -> Value {
    <WebWsBundle as fantastic_kernel::Bundle>::handle_binary(
        &WebWsBundle,
        &AgentId::from("ws"),
        payload.clone(),
        None,
        kernel,
    )
    .await
    .expect("handled")
    .0
    .expect("reply")
}

/// `get_routes` declares python's one websocket spec, with NO `method`
/// key (python also carries a live `endpoint` callable, which has no wire
/// spelling).
#[tokio::test]
async fn get_routes_match_python() {
    let kernel = kernel_with_leg(json!({}));
    assert_eq!(
        ask(&kernel, "get_routes").await,
        json!({"routes": [{"kind": "websocket", "path": "/{host_id}/ws"}]})
    );
}

/// An unknown verb is refused by the leg's own entry, naming what the leg answers; the
/// raw `type` is echoed back (`null` when absent, `5`, `""`).
#[tokio::test]
async fn unknown_verb_is_unhandled_refusal() {
    let kernel = kernel_with_leg(json!({}));
    for (payload, verb) in [
        (json!({"type": "frob"}), json!("frob")),
        (json!({}), Value::Null),
        (json!({"type": 5}), json!(5)),
        (json!({"type": ""}), json!("")),
    ] {
        let reply = send(&kernel, &payload).await;
        assert_eq!(reply["reason"], json!("unhandled"), "{payload}");
        assert_eq!(reply["verb"], verb, "{payload}");
        let error = reply["error"].as_str().expect("error text");
        assert!(error.starts_with("unknown verb "), "{error}");
        assert!(
            error.contains(
                "ws answers: get_routes, inbound_call, inbound_emit, inbound_watch, \
                 inbound_state_subscribe"
            ),
            "{error}"
        );
    }
}

/// The reflect frame: no `kind`, no `mounted_on`, the leg posture from `describe`.
#[tokio::test]
async fn reflect_keys_match_python() {
    let kernel = kernel_with_leg(json!({}));
    let reply = ask(&kernel, "reflect").await;
    let obj = reply.as_object().expect("object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "ingress_rule",
            "egress_rule",
            "sealed",
        ]
    );
    assert_eq!(
        obj["sentence"],
        json!(crate::MAN)
    );
}

/// The inbound verbs refuse what their declaration does not take, by name: an unknown
/// argument, a missing one, a wrongly typed one — before any handler runs.
#[tokio::test]
async fn the_inbound_verbs_are_checked_by_their_declaration() {
    let kernel = kernel_with_leg(json!({}));
    for (payload, want) in [
        (
            json!({"type": "inbound_emit", "target": "x", "payload": {}, "extra": 1}),
            "inbound_emit: unknown argument 'extra'",
        ),
        (
            json!({"type": "inbound_emit", "payload": {}}),
            "inbound_emit: missing argument 'target'",
        ),
        (
            json!({"type": "inbound_emit", "target": "x", "payload": "s"}),
            "inbound_emit: argument 'payload' must be",
        ),
        (
            json!({"type": "inbound_watch", "src": "a"}),
            "inbound_watch: missing argument 'reader'",
        ),
        (
            json!({"type": "inbound_call", "item": {"payload": {}}}),
            "inbound_call: missing argument 'item.target'",
        ),
        (
            json!({"type": "inbound_state_subscribe", "x": 1}),
            "inbound_state_subscribe: unknown argument 'x'",
        ),
    ] {
        let reply = send(&kernel, &payload).await;
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(error.starts_with(want), "{payload} -> {reply}");
    }
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
            WebWsBundle.validate_meta(&meta, &Map::new()),
            Some(format!(
                "ingress_rule={shown} is not a rule this bundle implements — legal \
                 values: allow_all_ingress, deny_ingress, password_ingress"
            ))
        );
    }
    let mut meta = Map::new();
    meta.insert("ingress_rule".into(), json!("password_ingress"));
    assert_eq!(WebWsBundle.validate_meta(&meta, &Map::new()), None);
}

async fn ask(kernel: &Arc<Kernel>, verb: &str) -> Value {
    send(kernel, &json!({"type": verb})).await
}

/// No `route` (absent or null): the leg keeps the catch-all.
#[tokio::test]
async fn a_leg_without_route_keeps_the_catch_all() {
    for meta in [json!({}), json!({"route": null})] {
        let kernel = kernel_with_leg(meta);
        assert_eq!(
            ask(&kernel, "get_routes").await,
            json!({"routes": [{"kind": "websocket", "path": "/{host_id}/ws"}]})
        );
    }
}

/// A `route` mounts only that literal.
#[tokio::test]
async fn a_route_mounts_only_its_literal() {
    let kernel = kernel_with_leg(json!({"route": "/page_ws/ws"}));
    assert_eq!(
        ask(&kernel, "get_routes").await,
        json!({"routes": [{"kind": "websocket", "path": "/page_ws/ws"}]})
    );
}

/// A `route` of the wrong type, empty, not starting with `/`, or holding `{`/`}`, is refused by the record declaration.
#[test]
fn the_record_declaration_refuses_a_route_of_the_wrong_type() {
    for bad in [json!(5), json!(true), json!(["/a"]), json!(""),
        json!("page_ws"), json!("/{host_id}/ws"), json!("/a/{x}"), json!("/a}")] {
        let mut meta = Map::new();
        meta.insert("route".into(), bad.clone());
        let refused = <WebWsBundle as fantastic_kernel::Bundle>::check_record(
            &WebWsBundle,
            "create_agent",
            &meta,
        )
        .expect_err(&format!("{bad} must be refused"));
        assert_eq!(refused["arg"], "route", "{refused}");
    }
}

