//! Unit tests for this bundle crate.

use super::*;
use fantastic_kernel::Agent;

/// The readme is python's file, byte for byte
/// (`PB/io/web_ws/src/web_ws/readme.md`).
#[test]
fn readme_is_python_verbatim() {
    assert_eq!(
        README,
        include_str!("../../../../../python/bundled_agents/io/web_ws/src/web_ws/readme.md")
    );
}

/// `get_routes` declares python's one websocket spec, with NO `method`
/// key (python also carries a live `endpoint` callable, which has no wire
/// spelling).
#[tokio::test]
async fn get_routes_match_python() {
    let kernel = Arc::new(Kernel::new());
    let reply = WebWsBundle
        .handle(
            &AgentId::from("ws"),
            &json!({"type": "get_routes"}),
            &kernel,
        )
        .await
        .expect("handled")
        .expect("reply");
    assert_eq!(
        reply,
        json!({"routes": [{"kind": "websocket", "path": "/{host_id}/ws"}]})
    );
}

/// An unknown verb is the shared `unhandled` refusal with python's
/// `{t!r}` text and the verb echoed back; `shutdown` is no longer a verb.
#[tokio::test]
async fn unknown_verb_is_unhandled_refusal() {
    let kernel = Arc::new(Kernel::new());
    let reply = WebWsBundle
        .handle(&AgentId::from("ws"), &json!({"type": "shutdown"}), &kernel)
        .await
        .expect("handled")
        .expect("reply");
    assert_eq!(
        reply,
        json!({
            "error": "web_ws: unknown type 'shutdown'",
            "reason": "unhandled",
            "verb": "shutdown",
        })
    );
}

/// The reflect frame is python's: no `kind`, no `mounted_on`, the
/// leg posture from `describe`, and python's verb docs.
#[tokio::test]
async fn reflect_keys_match_python() {
    let kernel = Arc::new(Kernel::new());
    let reply = WebWsBundle
        .handle(&AgentId::from("ws"), &json!({"type": "reflect"}), &kernel)
        .await
        .expect("handled")
        .expect("reply");
    let obj = reply.as_object().expect("object");
    let keys: Vec<&str> = obj.keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        vec![
            "id",
            "sentence",
            "path_pattern",
            "ingress_rule",
            "egress_rule",
            "sealed",
        ]
    );
    assert_eq!(
        obj["sentence"],
        json!("WS verb-invocation surface; mounts /<host_id>/ws on the parent web.")
    );
    assert_eq!(obj["path_pattern"], json!("/{host_id}/ws"));
}

/// The unknown-type refusal's KEY ORDER is part of the
/// wire (`preserve_order`); `Value` equality ignores it, so the text is compared.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let kernel = Arc::new(Kernel::new());
    for (payload, want) in [
        (
            json!({"type": "frob"}),
            r#"{"error":"web_ws: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#,
        ),
        (
            json!({}),
            r#"{"error":"web_ws: unknown type None","reason":"unhandled","verb":null}"#,
        ),
        (
            json!({"type": 5}),
            r#"{"error":"web_ws: unknown type 5","reason":"unhandled","verb":5}"#,
        ),
    ] {
        let reply = WebWsBundle
            .handle(&AgentId::from("ws"), &payload, &kernel)
            .await
            .expect("handled")
            .expect("reply");
        assert_eq!(serde_json::to_string(&reply).unwrap(), want, "{payload}");
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

async fn ask(kernel: &Arc<Kernel>, verb: &str) -> Value {
    WebWsBundle
        .handle(&AgentId::from("ws"), &json!({"type": verb}), kernel)
        .await
        .expect("handled")
        .expect("reply")
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
        assert_eq!(
            ask(&kernel, "reflect").await["path_pattern"],
            json!("/{host_id}/ws")
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
    assert_eq!(
        ask(&kernel, "reflect").await["path_pattern"],
        json!("/page_ws/ws")
    );
}

/// A `route` that is not a literal path is refused at the write.
#[test]
fn validate_meta_refuses_a_route_that_is_not_a_literal_path() {
    for bad in [json!("/{x}/ws"), json!("page/ws"), json!(""), json!(7)] {
        let mut meta = Map::new();
        meta.insert("route".into(), bad.clone());
        let text = WebWsBundle
            .validate_meta(&meta, &Map::new())
            .unwrap_or_else(|| panic!("{bad} must be refused"));
        assert!(text.starts_with("route="), "{text}");
        assert!(text.contains("is not a literal mount path"), "{text}");
    }
    let mut meta = Map::new();
    meta.insert("route".into(), json!(7));
    assert_eq!(
        WebWsBundle.validate_meta(&meta, &Map::new()),
        Some(
            "route=7 is not a literal mount path — a string starting with \"/\", with no \"{\" or \"}\""
                .to_string()
        )
    );
    for ok in [json!("/page_ws/ws"), Value::Null] {
        let mut meta = Map::new();
        meta.insert("ingress_rule".into(), json!("allow_all_ingress"));
        meta.insert("route".into(), ok);
        assert_eq!(WebWsBundle.validate_meta(&meta, &Map::new()), None);
    }
}
