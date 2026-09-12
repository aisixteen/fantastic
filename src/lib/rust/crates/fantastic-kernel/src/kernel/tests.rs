//! Unit tests for [`crate::kernel`].
use crate::agent::{AgentRecord, RESERVED_RECORD_KEYS};

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
async fn an_inbox_at_its_bound_drops_the_oldest() {
    // THE BOUND IS WIRED, AND THE TIP SURVIVES. An emitter never blocks and
    // never fails; at the bound the OLDEST payload is evicted, so a consumer
    // that fell behind catches up on the freshest events rather than freezing on
    // stale ones. Python's `_put_drop_oldest` and swift's `.bufferingNewest`
    // rule; rust used to invert it (`try_send` discards what is being sent)
    // under a comment claiming they matched.
    let k = Kernel::new_with_inbox_bound(2);
    let a = make_agent("bp");
    let mut rx = k.register(Arc::clone(&a));
    let tx = k.inboxes.get(&AgentId::from("bp")).unwrap().clone();
    tx.send(json!({"type": "one"}));
    tx.send(json!({"type": "two"}));
    tx.send(json!({"type": "three"}));

    assert_eq!(rx.try_recv().expect("held")["type"], "two");
    assert_eq!(rx.try_recv().expect("held")["type"], "three");
    assert!(rx.try_recv().is_none(), "the bound holds exactly two");
}

// ─── #3 / #4 / #5 — record hygiene ──────────────────────────────────

#[test]
fn save_drops_the_ephemeral_subtree_transitively() {
    // Dropping only the ephemeral agent ITSELF left its children behind as
    // orphans — records whose `parent_id` names an agent not in the document —
    // and `load()` rejects exactly that. `save()` emitted a snapshot its own
    // loader refused.
    let kernel = Kernel::new();
    // A PERSISTENT root — `make_agent` builds ephemeral ones.
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    let _r = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let cli = Agent::new(
        AgentId::from("cli"),
        None,
        Some(root.id.clone()),
        Map::new(),
        std::path::PathBuf::new(),
        true, // EPHEMERAL
    );
    let _c = kernel.register(Arc::clone(&cli));
    // A child OF the ephemeral agent, and a grandchild.
    for (id, parent) in [("cli_kid", "cli"), ("cli_grandkid", "cli_kid")] {
        let a = Agent::new(
            AgentId::from(id),
            None,
            Some(AgentId::from(parent)),
            Map::new(),
            std::path::PathBuf::new(),
            false, // NOT ephemeral itself — it is orphaned by its parent
        );
        let _x = kernel.register(a);
    }

    let snap = kernel.save();
    let ids: Vec<&str> = snap.agents.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, vec!["core"], "the ephemeral subtree survived: {ids:?}");

    // The real property: what save() emits, load() accepts.
    let fresh = Kernel::new();
    fresh
        .load(snap)
        .expect("save() must emit a loadable document");
}

#[test]
fn a_reserved_key_is_refused_before_anything_is_torn_down() {
    // A hand-authored `root_path:` once cleared the whole tree and THEN raised.
    // Validation BEFORE teardown is what makes a bad document a no-op.
    let kernel = Kernel::new();
    let root = Agent::new(
        AgentId::from("core"),
        None,
        None,
        Map::new(),
        std::path::PathBuf::new(),
        false,
    );
    let _r = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    for key in RESERVED_RECORD_KEYS {
        let mut meta = Map::new();
        meta.insert(key.to_string(), json!("anything"));
        let state = KernelState {
            version: CURRENT_VERSION,
            agents: vec![AgentRecord {
                id: "core".into(),
                handler_module: None,
                parent_id: None,
                meta,
            }],
        };
        let err = kernel.load(state).expect_err("{key} must be refused");
        assert!(format!("{err}").contains(key), "{err}");
        // ...and the live tree is UNTOUCHED.
        assert!(
            kernel.agents.contains_key(&AgentId::from("core")),
            "a refused load tore the tree down anyway"
        );
    }
}
