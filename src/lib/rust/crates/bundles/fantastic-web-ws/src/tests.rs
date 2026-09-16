//! Unit tests for this bundle crate.

use super::*;

/// A11.6 — the readme is python's file, byte for byte
/// (`PB/io/web_ws/src/web_ws/readme.md`).
#[test]
fn readme_is_python_verbatim() {
    assert_eq!(
        README,
        include_str!("../../../../../python/bundled_agents/io/web_ws/src/web_ws/readme.md")
    );
}

/// A11.5 — `get_routes` declares python's one websocket spec, with NO `method`
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

/// A11.4 — an unknown verb is the shared `unhandled` refusal with python's
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

/// A11.16 — the reflect frame is python's: no `kind`, no `mounted_on`, the
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
            "verbs",
        ]
    );
    assert_eq!(
        obj["sentence"],
        json!("WS verb-invocation surface; mounts /<host_id>/ws on the parent web.")
    );
    assert_eq!(obj["path_pattern"], json!("/{host_id}/ws"));
}
