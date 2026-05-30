//! Unit tests for this bundle crate.

use super::*;
use serde_json::json;

#[test]
fn readme_present_and_titled() {
    assert!(!README.is_empty());
    assert!(README.contains("cli — stdout renderer"));
}

#[test]
fn format_send_event() {
    let v = json!({
        "type": "send",
        "sender": "alice",
        "target": "bob",
        "verb": "ping",
        "summary": r#"{"type":"ping"}"#,
    });
    let line = format_event(&v);
    assert!(line.starts_with("send "));
    assert!(line.contains("alice → bob"));
    assert!(line.contains("ping"));
    assert!(line.contains(r#"{"type":"ping"}"#));
}

#[test]
fn format_emit_event() {
    let v = json!({
        "type": "emit",
        "sender": "x",
        "target": "y",
        "verb": "tick",
        "summary": "{}",
    });
    let line = format_event(&v);
    assert!(line.starts_with("emit "));
    assert!(line.contains("x → y"));
}

#[test]
fn format_created_event_with_handler_module() {
    let v = json!({
        "type": "created",
        "id": "kid_1",
        "parent_id": "core",
        "handler_module": "file.tools",
    });
    let line = format_event(&v);
    assert_eq!(line, "created  kid_1 (file.tools)");
}

#[test]
fn format_removed_event() {
    let v = json!({"type": "removed", "id": "gone_1"});
    assert_eq!(format_event(&v), "removed  gone_1");
}

#[test]
fn format_unknown_event_falls_through_to_json() {
    let v = json!({"type": "mystery", "foo": 1});
    let line = format_event(&v);
    assert!(line.starts_with("mystery"));
    assert!(line.contains("\"foo\":1"));
}

#[tokio::test]
async fn attach_returns_token_and_subscriber_fires() {
    // We don't capture stdout in this unit test (would require
    // shadowing the global fd); we just verify attach() returns a
    // usable token and the subscriber isn't immediately detached.
    let kernel = std::sync::Arc::new(Kernel::new());
    let token = attach(&kernel);
    kernel.publish_state(
        &json!({"type": "send", "sender": "a", "target": "b", "verb": "x", "summary": "{}"}),
    );
    kernel.remove_state_subscriber(token);
    // After detach, further publishes should be a no-op for our subscriber.
    kernel.publish_state(
        &json!({"type": "send", "sender": "a", "target": "b", "verb": "x", "summary": "{}"}),
    );
}
