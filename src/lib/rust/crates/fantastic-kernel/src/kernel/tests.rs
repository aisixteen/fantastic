//! Unit tests for [`crate::kernel`].

use super::*;
use crate::agent::Agent;
use serde_json::{json, Map};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

fn make_agent(id: &str) -> Arc<Agent> {
    Agent::new(
        id.into(),
        None,
        None,
        Map::new(),
        Path::new("/tmp/nowhere").join(id),
        true,
    )
}

#[test]
fn empty_kernel_has_no_root() {
    let k = Kernel::new();
    assert!(k.root().is_none());
}

#[test]
fn register_creates_inbox_and_indexes_agent() {
    let k = Kernel::new();
    let a = make_agent("agent_x");
    let _rx = k.register(Arc::clone(&a));
    assert!(k.agents.contains_key(&AgentId::from("agent_x")));
    assert!(k.inboxes.contains_key(&AgentId::from("agent_x")));
}

#[test]
fn unregister_drops_both_indexes() {
    let k = Kernel::new();
    let a = make_agent("zap");
    let _rx = k.register(Arc::clone(&a));
    k.unregister(&AgentId::from("zap"));
    assert!(!k.agents.contains_key(&AgentId::from("zap")));
    assert!(!k.inboxes.contains_key(&AgentId::from("zap")));
}

#[test]
fn root_swap_visible_to_all_readers() {
    let k = Arc::new(Kernel::new());
    let root = make_agent("core");
    k.set_root(Arc::clone(&root));
    let seen = k.root().expect("root set");
    assert_eq!(seen.id, AgentId::from("core"));
}

#[test]
fn publish_state_reaches_every_subscriber() {
    let k = Kernel::new();
    let count1 = Arc::new(AtomicUsize::new(0));
    let count2 = Arc::new(AtomicUsize::new(0));
    let c1 = Arc::clone(&count1);
    let c2 = Arc::clone(&count2);
    let _t1 = k.add_state_subscriber(Arc::new(move |_ev| {
        c1.fetch_add(1, Ordering::SeqCst);
    }));
    let _t2 = k.add_state_subscriber(Arc::new(move |_ev| {
        c2.fetch_add(1, Ordering::SeqCst);
    }));
    k.publish_state(&json!({"type": "test"}));
    k.publish_state(&json!({"type": "test"}));
    assert_eq!(count1.load(Ordering::SeqCst), 2);
    assert_eq!(count2.load(Ordering::SeqCst), 2);
}

#[test]
fn remove_state_subscriber_detaches() {
    let k = Kernel::new();
    let count = Arc::new(AtomicUsize::new(0));
    let c = Arc::clone(&count);
    let token = k.add_state_subscriber(Arc::new(move |_ev| {
        c.fetch_add(1, Ordering::SeqCst);
    }));
    k.publish_state(&json!({"type": "tick"}));
    k.remove_state_subscriber(token);
    k.publish_state(&json!({"type": "tick"}));
    // Subscriber fired exactly once, before detach.
    assert_eq!(count.load(Ordering::SeqCst), 1);
    // Re-removing the same token is a no-op.
    k.remove_state_subscriber(token);
}

#[tokio::test]
async fn inbox_channel_back_pressure_at_bound() {
    // Sender to a bound=2 inbox accepts 2 then blocks on the 3rd
    // (try_send returns Err). Verifies the bound is actually wired.
    let k = Kernel::new_with_inbox_bound(2);
    let a = make_agent("bp");
    let _rx = k.register(Arc::clone(&a));
    let tx = k.inboxes.get(&AgentId::from("bp")).unwrap().clone();
    tx.send(json!({"type": "one"})).await.unwrap();
    tx.send(json!({"type": "two"})).await.unwrap();
    // The 3rd push would block — try_send must report full.
    match tx.try_send(json!({"type": "three"})) {
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
        other => panic!("expected Full, got {other:?}"),
    }
}
