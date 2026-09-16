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
fn register_mints_no_inbox() {
    // AN AGENT DOES NOT GET A QUEUE MERELY BY EXISTING. `register` used to
    // auto-vivify one, so every agent looked like a reader to `watch` and a
    // mirror piled up in a queue nobody drained. `ensure_inbox` IS the reader
    // registration, and only then does an inbox exist.
    let k = Kernel::new();
    let a = make_agent("agent_x");
    k.register(Arc::clone(&a));
    assert!(k.agents.contains_key(&AgentId::from("agent_x")));
    assert!(
        !k.inboxes.contains_key(&AgentId::from("agent_x")),
        "register minted an inbox nobody asked for"
    );
    let _rx = k.ensure_inbox(&AgentId::from("agent_x"));
    assert!(k.inboxes.contains_key(&AgentId::from("agent_x")));
}

#[tokio::test]
async fn ensure_inbox_is_idempotent_and_shares_one_queue() {
    // A reader may call it defensively before every read; a second call must
    // hand back the SAME queue, not a fresh one that silently swallows the mail
    // the first receiver is waiting on.
    let k = Kernel::new();
    let a = make_agent("twice");
    k.register(Arc::clone(&a));
    let id = AgentId::from("twice");
    let _first = k.ensure_inbox(&id);
    let mut second = k.ensure_inbox(&id);
    assert_eq!(k.inboxes.len(), 1, "a second call minted a second inbox");
    k.inboxes
        .get(&id)
        .expect("inbox registered")
        .send(crate::inbox::InboxItem::text(json!({"type": "ping"})));
    assert_eq!(
        second.try_recv().expect("the one queue holds it").payload["type"],
        "ping"
    );
}

#[test]
fn unregister_drops_both_indexes() {
    let k = Kernel::new();
    let a = make_agent("zap");
    k.register(Arc::clone(&a));
    let _rx = k.ensure_inbox(&AgentId::from("zap"));
    k.unregister(&AgentId::from("zap"));
    assert!(!k.agents.contains_key(&AgentId::from("zap")));
    assert!(!k.inboxes.contains_key(&AgentId::from("zap")));
}

#[test]
fn remove_inbox_drops_the_reader_only() {
    // The twin of swift `Kernel.removeInbox`: a browser disconnects, its
    // synthetic queue goes, and the agent it was watching is untouched.
    let k = Kernel::new();
    let a = make_agent("keeper");
    k.register(Arc::clone(&a));
    let id = AgentId::from("keeper");
    let _rx = k.ensure_inbox(&id);
    k.remove_inbox(&id);
    assert!(!k.inboxes.contains_key(&id));
    assert!(k.agents.contains_key(&id), "the agent went with its inbox");
    k.remove_inbox(&id); // idempotent
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
    let _t1 = k.add_state_subscriber(Arc::new(move |_ev, _body| {
        c1.fetch_add(1, Ordering::SeqCst);
    }));
    let _t2 = k.add_state_subscriber(Arc::new(move |_ev, _body| {
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
    let token = k.add_state_subscriber(Arc::new(move |_ev, _body| {
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
    k.register(Arc::clone(&a));
    let mut rx = k.ensure_inbox(&AgentId::from("bp"));
    let tx = k.inboxes.get(&AgentId::from("bp")).unwrap().clone();
    for t in ["one", "two", "three"] {
        tx.send(crate::inbox::InboxItem::text(json!({ "type": t })));
    }

    assert_eq!(rx.try_recv().expect("held").payload["type"], "two");
    assert_eq!(rx.try_recv().expect("held").payload["type"], "three");
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
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let cli = Agent::new(
        AgentId::from("cli"),
        None,
        Some(root.id.clone()),
        Map::new(),
        std::path::PathBuf::new(),
        true, // EPHEMERAL
    );
    kernel.register(Arc::clone(&cli));
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
        kernel.register(a);
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
    kernel.register(Arc::clone(&root));
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

// ─── C77 — the state stream contract ────────────────────────────────

#[test]
fn state_snapshot_is_registration_order_with_in_flight() {
    // Python walks `self.agents.values()` — the registry's own order — and
    // reports `_in_flight`, the handler dispatches running now. Rust sorted by
    // id and answered `backlog: 0` for everyone, so it described a registry and
    // a workload python never describes.
    let k = Kernel::new();
    for id in ["zeta", "alpha", "middle"] {
        k.register(make_agent(id));
    }
    let mut meta = Map::new();
    meta.insert("display_name".into(), json!("Zed"));
    let named = Agent::new(
        AgentId::from("named"),
        None,
        None,
        meta,
        std::path::PathBuf::new(),
        true,
    );
    k.register(Arc::clone(&named));
    named.in_flight_enter();
    named.in_flight_enter();

    let snap = k.state_snapshot();
    let ids: Vec<&str> = snap
        .iter()
        .map(|e| e["agent_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(ids, vec!["zeta", "alpha", "middle", "named"]);
    assert_eq!(
        snap[0],
        json!({"agent_id": "zeta", "name": "zeta", "backlog": 0})
    );
    assert_eq!(
        snap[3],
        json!({"agent_id": "named", "name": "Zed", "backlog": 2})
    );
}

#[tokio::test]
async fn state_events_are_python_shape() {
    // `{agent_id, kind, backlog, sender, summary, payload}` + a `ts` stamped
    // last, then `{agent_id, kind:"drain", backlog}` once the handler returned.
    // The port sent `{type, sender, target, verb, summary}` — a different shape
    // with a different vocabulary, and `sender` was the TARGET's own id whenever
    // an operator sent.
    let kernel = Arc::new(Kernel::new());
    let a = make_agent("worker");
    kernel.register(Arc::clone(&a));
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let sink = Arc::clone(&seen);
    let _t = kernel.add_state_subscriber(Arc::new(move |ev: &Value, _b| {
        sink.lock().expect("sink poisoned").push(ev.clone());
    }));
    let _ = kernel
        .send(vec![crate::outcome::Item::new(
            a.id.clone(),
            json!({"type": "nope"}),
        )])
        .await;

    let events = seen.lock().expect("sink poisoned").clone();
    let kinds: Vec<&str> = events
        .iter()
        .map(|e| e["kind"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(kinds, vec!["send", "drain"], "{events:?}");

    let send = &events[0];
    let keys: Vec<&str> = send
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec!["agent_id", "kind", "backlog", "sender", "summary", "payload", "ts"]
    );
    assert_eq!(send["agent_id"], json!("worker"));
    assert_eq!(
        send["backlog"],
        json!(1),
        "in flight while the handler runs"
    );
    assert_eq!(
        send["sender"],
        Value::Null,
        "an operator send carries JSON null, never the target's own id"
    );
    assert_eq!(send["summary"], json!(r#"{"type": "nope"}"#));
    assert_eq!(send["payload"], json!({"type": "nope"}));
    assert!(send["ts"].is_f64());

    let drain = &events[1];
    let dkeys: Vec<&str> = drain
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(dkeys, vec!["agent_id", "kind", "backlog", "ts"]);
    assert_eq!(drain["backlog"], json!(0));
    assert_eq!(a.in_flight(), 0, "the counter came back down");
}
