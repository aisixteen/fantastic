//! THE SIDEBAR FOLLOWS THE KERNEL, as the agent: from its `boot` on it taps the kernel's
//! state stream (inside the kernel, as the agent — never through a view) and publishes what
//! its tree shows changed on ITS OWN inbox. Its view watches only it: one publisher, many
//! view-models.
//!
//! What it publishes (each `{type, …}` on the sidebar's id):
//! - `tree_changed {}` — an agent was created, deleted or updated (`update_agent`), or a
//!   kernel connected or left (a leg's `peer_connected` / `socket_closed`, a bridge's
//!   `bridge_up` / `bridge_down`);
//! - `status_changed {agent, phase}` — a chat's call moved (its own `status` event; `done`
//!   is the end of the call);
//! - `inquiries_changed {open}` — a question to the person (`user`) opened or closed;
//!   `open` is how many are open now;
//! - `files_changed {path, change}` — an entry of a folder the files agent watches was
//!   `added` or `removed`. A file written in place (`modified`) is no change of the tree:
//!   the process's own log in the files root is written on every call.
//!
//! The tap reads only events the kernel publishes: a copy of a call made to an agent is a
//! `send`, never an `emit`, so none of the sidebar's (or its view's) own reads come back.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use fantastic_kernel::{AgentId, Kernel, SubscriberToken};
use serde_json::{json, Value};

/// The event types the sidebar publishes on its own inbox.
pub const TREE_CHANGED: &str = "tree_changed";
pub const STATUS_CHANGED: &str = "status_changed";
pub const INQUIRIES_CHANGED: &str = "inquiries_changed";
pub const FILES_CHANGED: &str = "files_changed";

/// The agent whose folder the tree shows (the view's `files`), and the holder of the
/// person's questions (the view's `user`).
const FILES: &str = "files";
const PERSON: &str = "user";

/// What one published kernel event is to the tree, or nothing.
#[derive(Debug, PartialEq)]
pub enum Change {
    Tree,
    Status { agent: String, phase: String },
    Inquiries,
    File { path: String, change: &'static str },
}

/// The events of other agents that change the tree: a record edited through `update_agent`,
/// a kernel that connected or left.
const TREE_EMITS: [&str; 5] = ["agent_updated", "peer_connected", "socket_closed", "bridge_up", "bridge_down"];

/// One state-stream event → the change it is to the tree. An `emit` mirrored to a watcher is
/// published again under the WATCHER's id: each rule names whose event it takes (a chat's own
/// status is its `source`, a question's holder is `user`, a folder change is the files
/// agent's), so a mirror is never taken for a second change.
#[must_use]
pub fn change_of(ev: &Value) -> Option<Change> {
    let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let at = s(ev, "agent_id")?;
    match ev.get("kind").and_then(Value::as_str)? {
        "added" | "removed" => Some(Change::Tree),
        "emit" => {
            let p = ev.get("payload")?;
            let ty = s(p, "type")?;
            match ty.as_str() {
                t if TREE_EMITS.contains(&t) => Some(Change::Tree),
                "status" => {
                    let phase = s(p, "phase").filter(|x| !x.is_empty())?;
                    (s(p, "source").as_deref() == Some(at.as_str())).then_some(Change::Status { agent: at, phase })
                }
                "inquiry_opened" | "inquiry_closed" => {
                    (at == PERSON && s(p, "holder").as_deref() == Some(PERSON)).then_some(Change::Inquiries)
                }
                "changed" if at == FILES => {
                    let path = s(p, "path")?;
                    let change = match s(p, "kind")?.as_str() {
                        "added" => "added",
                        "deleted" => "removed",
                        _ => return None,
                    };
                    Some(Change::File { path, change })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Each sidebar's tap on this kernel, and the call phase of each chat it has seen moving
/// (a chat at rest has none).
#[derive(Default)]
pub struct Follow {
    taps: Mutex<HashMap<String, SubscriberToken>>,
    phases: Mutex<HashMap<String, BTreeMap<String, String>>>,
}

fn follow(kernel: &Kernel) -> Arc<Follow> {
    kernel.extension::<Follow>("sidebar.follow")
}

/// The phases sidebar `me` holds now: `{chat: phase}` for every chat inside a call.
#[must_use]
pub fn phases(kernel: &Kernel, me: &AgentId) -> Value {
    let f = follow(kernel);
    let all = f.phases.lock().expect("phases poisoned");
    json!(all.get(me.as_str()).cloned().unwrap_or_default())
}

/// How many questions to the person are open now (0 when `user` holds none or is absent).
#[must_use]
pub fn open_questions(kernel: &Arc<Kernel>) -> u64 {
    if !kernel.agents.contains_key(&AgentId::from(PERSON)) {
        return 0;
    }
    fantastic_kernel::inquiry::open_count(kernel, &AgentId::from(PERSON))
        .as_u64()
        .unwrap_or(0)
}

/// Start following the kernel for sidebar `me`; a second call is a no-op.
pub fn start(kernel: &Arc<Kernel>, me: &AgentId) {
    let f = follow(kernel);
    let mut taps = f.taps.lock().expect("taps poisoned");
    if taps.contains_key(me.as_str()) {
        return;
    }
    // the kernel is not touched from inside its own publish: changes are queued
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Change>();
    let token = kernel.add_state_subscriber(Arc::new(move |ev: &Value, _body| {
        if let Some(c) = change_of(ev) {
            let _ = tx.send(c);
        }
    }));
    taps.insert(me.as_str().to_string(), token);
    drop(taps);
    let weak = Arc::downgrade(kernel);
    let me = me.clone();
    tokio::spawn(async move {
        // the subscriber dropped (`stop`) drops the sender: the loop ends
        while let Some(c) = rx.recv().await {
            let Some(k) = weak.upgrade() else { return };
            let ev = match c {
                Change::Tree => json!({"type": TREE_CHANGED}),
                Change::Status { agent, phase } => {
                    let f = follow(&k);
                    let mut all = f.phases.lock().expect("phases poisoned");
                    let mine = all.entry(me.as_str().to_string()).or_default();
                    if phase == "done" {
                        mine.remove(&agent);
                    } else {
                        mine.insert(agent.clone(), phase.clone());
                    }
                    drop(all);
                    json!({"type": STATUS_CHANGED, "agent": agent, "phase": phase})
                }
                Change::Inquiries => json!({"type": INQUIRIES_CHANGED, "open": open_questions(&k)}),
                Change::File { path, change } => json!({"type": FILES_CHANGED, "path": path, "change": change}),
            };
            k.emit(&me, ev).await;
        }
    });
}

/// Stop following (the sidebar is deleted, or the kernel stops).
pub fn stop(kernel: &Arc<Kernel>, me: &AgentId) {
    let f = follow(kernel);
    if let Some(token) = f.taps.lock().expect("taps poisoned").remove(me.as_str()) {
        kernel.remove_state_subscriber(token);
    }
    f.phases.lock().expect("phases poisoned").remove(me.as_str());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emit(at: &str, payload: Value) -> Value {
        json!({"agent_id": at, "kind": "emit", "backlog": 0, "sender": null, "summary": "", "payload": payload})
    }

    #[test]
    fn each_published_change_the_tree_shows_is_named_and_nothing_else() {
        assert_eq!(change_of(&json!({"agent_id": "c1", "kind": "added", "parent_id": "k"})), Some(Change::Tree));
        assert_eq!(change_of(&json!({"agent_id": "c1", "kind": "removed"})), Some(Change::Tree));
        assert_eq!(change_of(&json!({"agent_id": "c1", "kind": "updated", "changed": ["next_id"]})), None, "a record write by the kernel is no tree change");
        assert_eq!(change_of(&emit("kernel", json!({"type": "agent_updated", "id": "c1"}))), Some(Change::Tree));
        assert_eq!(change_of(&emit("web", json!({"type": "peer_connected", "guid": "g"}))), Some(Change::Tree));
        assert_eq!(change_of(&emit("web", json!({"type": "socket_closed", "client_id": "x"}))), Some(Change::Tree));
        assert_eq!(change_of(&emit("b", json!({"type": "bridge_down"}))), Some(Change::Tree));
        assert_eq!(
            change_of(&emit("c1", json!({"type": "status", "source": "c1", "phase": "thinking"}))),
            Some(Change::Status { agent: "c1".into(), phase: "thinking".into() })
        );
        assert_eq!(
            change_of(&emit("user", json!({"type": "inquiry_opened", "re": 4, "asker": "c1", "holder": "user"}))),
            Some(Change::Inquiries)
        );
        assert_eq!(
            change_of(&emit("files", json!({"type": "changed", "path": "src/a.rs", "kind": "added"}))),
            Some(Change::File { path: "src/a.rs".into(), change: "added" })
        );
        assert_eq!(
            change_of(&emit("files", json!({"type": "changed", "path": "src/a.rs", "kind": "deleted"}))),
            Some(Change::File { path: "src/a.rs".into(), change: "removed" })
        );
    }

    #[test]
    fn a_copy_a_mirror_or_a_write_in_place_is_no_change() {
        // a call is published as a `send`: the copy of a status read, a listing, a reflect
        for p in [json!({"type": "status"}), json!({"type": "list", "path": "."}), json!({"type": "reflect"})] {
            assert_eq!(change_of(&json!({"agent_id": "c1", "kind": "send", "payload": p})), None, "{p}");
        }
        // a status with no phase is a read's echo, not a moved call
        assert_eq!(change_of(&emit("c1", json!({"type": "status", "source": "c1"}))), None);
        // a mirror on a watcher: another agent's status, the asker's copy of a question
        assert_eq!(change_of(&emit("w", json!({"type": "status", "source": "c1", "phase": "thinking"}))), None);
        assert_eq!(change_of(&emit("c1", json!({"type": "inquiry_opened", "re": 4, "asker": "c1", "holder": "user"}))), None);
        assert_eq!(change_of(&emit("mail", json!({"type": "inquiry_opened", "re": 4, "asker": "c1", "holder": "mail"}))), None, "another holder's");
        // the log the process writes on every call, in place
        assert_eq!(change_of(&emit("files", json!({"type": "changed", "path": "team.log", "kind": "modified"}))), None);
        assert_eq!(change_of(&emit("w", json!({"type": "changed", "path": "a", "kind": "added"}))), None, "another bridge's");
        // the sidebar's own publications
        for t in [TREE_CHANGED, STATUS_CHANGED, INQUIRIES_CHANGED, FILES_CHANGED] {
            assert_eq!(change_of(&emit("sidebar", json!({"type": t, "agent": "c1", "phase": "x"}))), None, "{t}");
        }
    }
}
