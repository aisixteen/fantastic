//! The batch vocabulary — the wire spellings ARE the contract.

use super::*;

#[test]
fn the_status_spellings_are_the_wire_contract() {
    // Every runtime and every client branches on these exact strings.
    assert_eq!(Status::Completed.as_str(), "completed");
    assert_eq!(Status::Failed.as_str(), "failed");
    assert_eq!(Status::Cancelled.as_str(), "cancelled");
    assert_eq!(Status::TimedOut.as_str(), "timed_out");
}

#[test]
fn cancelled_is_absorbing_not_retryable() {
    // A cancel is a human decision, not a transient — the retry engine must
    // never re-arm work an operator killed.
    assert!(Status::Failed.is_retryable());
    assert!(Status::TimedOut.is_retryable());
    assert!(!Status::Cancelled.is_retryable());
    assert!(!Status::Completed.is_retryable());
}

#[test]
fn a_refusal_has_no_pid_and_carries_the_refusal_verbatim() {
    // INVARIANT: pid.is_none() <=> nothing was ever in the task table. The
    // refusal body is untouched so every caller's `reason` branch survives.
    let r = crate::refusal::missing("create_agent", "parent_id");
    let o = Outcome::refused(r.clone());
    assert!(o.pid.is_none());
    assert_eq!(o.status, Status::Failed);
    assert_eq!(o.result, r);
    assert_eq!(o.attempts, 1);
}

#[test]
fn an_item_defaults_nothing() {
    // The kernel NEVER defaults a timeout or a retry budget: absent means
    // "wait forever" and "one attempt", and that is the caller's call to make.
    let it = Item::new("store", serde_json::json!({"type": "read"}));
    assert!(it.id.is_none());
    assert!(it.timeout.is_none());
    assert!(it.retries.is_none());

    let named = Item::new("store", serde_json::json!({"type": "read"}))
        .with_id("job-1")
        .with_timeout(2.5)
        .with_retries(3);
    assert_eq!(named.id.as_deref(), Some("job-1"));
    assert_eq!(named.timeout, Some(2.5));
    assert_eq!(named.retries, Some(3));
}

#[test]
fn a_frame_id_becomes_the_execution_id() {
    // ONE id space: the correlation id and the execution id are the same thing,
    // so the same id arriving twice (two tabs, a reconnect) is ONE intent.
    let it = Item::from_frame("desk", serde_json::json!({"type": "buy"}), Some("click-7"));
    assert_eq!(it.id.as_deref(), Some("click-7"));

    // An absent or empty frame id names nothing — the execution is unnamed and
    // gets a minted pid instead.
    assert!(Item::from_frame("desk", serde_json::json!({}), None)
        .id
        .is_none());
    assert!(Item::from_frame("desk", serde_json::json!({}), Some(""))
        .id
        .is_none());
}

#[test]
fn the_wire_shape_is_pid_status_result_attempts() {
    let o = Outcome::new(
        Some(crate::tasks::TaskId::from("desk:0badf00d")),
        Status::Completed,
        serde_json::json!({"ok": true}),
    );
    let v = o.to_value();
    assert_eq!(v["pid"], "desk:0badf00d");
    assert_eq!(v["status"], "completed");
    assert_eq!(v["result"]["ok"], true);
    assert_eq!(v["attempts"], 1);

    // A refusal serializes with a null pid, never a missing key.
    let r = Outcome::refused(serde_json::json!({"reason": "unauthorized"}));
    assert!(r.to_value()["pid"].is_null());
}
