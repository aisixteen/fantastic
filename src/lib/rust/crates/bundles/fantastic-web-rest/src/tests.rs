//! Unit tests for this bundle crate.

use super::*;

/// A11.6 — the readme is python's file, byte for byte
/// (`PB/io/web_rest/src/web_rest/readme.md`).
#[test]
fn readme_is_python_verbatim() {
    assert_eq!(
        README,
        include_str!("../../../../../python/bundled_agents/io/web_rest/src/web_rest/readme.md")
    );
}

/// A11.5 — `get_routes` declares python's three specs: `{kind:"http", method,
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

/// A11.4 — an unknown verb is the shared `unhandled` refusal with python's
/// `{t!r}` text and the verb echoed back; `shutdown` is no longer a verb.
#[tokio::test]
async fn unknown_verb_is_unhandled_refusal() {
    let kernel = Arc::new(Kernel::new());
    let reply = WebRestBundle
        .handle(
            &AgentId::from("rest"),
            &json!({"type": "shutdown"}),
            &kernel,
        )
        .await
        .expect("handled")
        .expect("reply");
    assert_eq!(
        reply,
        json!({
            "error": "web_rest: unknown type 'shutdown'",
            "reason": "unhandled",
            "verb": "shutdown",
        })
    );
}

/// A11.15 — the reflect frame is python's: no `mounted_on`, the curl examples,
/// the `auth_header`, the leg posture from `describe`, and python's verb docs.
#[tokio::test]
async fn reflect_keys_match_python() {
    let kernel = Arc::new(Kernel::new());
    let reply = WebRestBundle
        .handle(&AgentId::from("rest"), &json!({"type": "reflect"}), &kernel)
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
            "method",
            "reflect_url",
            "reflect_pattern",
            "auth_header",
            "curl_post",
            "curl_reflect",
            "curl_reflect_target",
            "ingress_rule",
            "egress_rule",
            "sealed",
            "verbs",
        ]
    );
    assert_eq!(
        obj["curl_post"],
        json!("curl -X POST -H \"content-type: application/json\" -d '{\"type\":\"reflect\"}' http://<host>/rest/<target_id>")
    );
    assert_eq!(
        obj["curl_reflect"],
        json!("curl http://<host>/rest/_reflect           # root reflect (tree)")
    );
    assert_eq!(
        obj["curl_reflect_target"],
        json!("curl http://<host>/rest/_reflect/<target_id>  # any agent")
    );
    assert_eq!(obj["sealed"], json!(true));
}

/// STEP-4 PIN (2026-10-01): the unknown-type refusal's KEY ORDER is part of the
/// wire (`preserve_order`); `Value` equality ignores it, so the text is compared.
#[tokio::test]
async fn unknown_type_wire_shape_is_byte_exact() {
    let kernel = Arc::new(Kernel::new());
    for (payload, want) in [
        (
            json!({"type": "frob"}),
            r#"{"error":"web_rest: unknown type 'frob'","reason":"unhandled","verb":"frob"}"#,
        ),
        (
            json!({}),
            r#"{"error":"web_rest: unknown type None","reason":"unhandled","verb":null}"#,
        ),
        (
            json!({"type": 5}),
            r#"{"error":"web_rest: unknown type 5","reason":"unhandled","verb":5}"#,
        ),
    ] {
        let reply = WebRestBundle
            .handle(&AgentId::from("rest"), &payload, &kernel)
            .await
            .expect("handled")
            .expect("reply");
        assert_eq!(serde_json::to_string(&reply).unwrap(), want, "{payload}");
    }
}
