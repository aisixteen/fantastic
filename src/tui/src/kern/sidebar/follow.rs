//! THE SIDEBAR FOLLOWS THE KERNEL, as the agent: from its `boot` on it taps the kernel's
//! state stream (inside the kernel, as the agent — never through a view) and publishes what
//! its tree shows changed on ITS OWN inbox. Its view watches only it: one publisher, many
//! view-models.
//!
//! What it publishes (each `{type, …}` on the sidebar's id):
//! - `tree_changed {}` — an agent was created, deleted or updated (`update_agent`), or a
//!   kernel connected or left (a leg's `peer_connected` / `socket_closed`, a bridge's
//!   `bridge_up` / `bridge_down`);
//! - `status_changed {agent, phase}` — a chat's call moved (its own `status` event). The
//!   sidebar keeps each chat's LAST phase — a call's end (`done`, `failed`, `stalled`,
//!   `awaiting`) and `paused` stay until the next call moves the chat; `idle` drops it;
//! - `line_summary_changed {agent, text, ts}` — a chat wrote a new one-line summary of what
//!   it does now (its own `line_summary_changed {text, ts}`, #709). The sidebar keeps each
//!   chat's latest line;
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
pub const LINE_SUMMARY_CHANGED: &str = fantastic_ai_core::line_summary::EVENT;
pub const INQUIRIES_CHANGED: &str = "inquiries_changed";
pub const FILES_CHANGED: &str = "files_changed";

/// The agent whose folder the tree shows (the view's `files`), and the holder of the
/// person's questions (the view's `user`).
pub(super) const FILES: &str = "files";
const PERSON: &str = "user";

/// What one published kernel event is to the tree, or nothing.
#[derive(Debug, PartialEq)]
pub enum Change {
    Tree,
    Status { agent: String, phase: String },
    Line { agent: String, text: String, ts: f64 },
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
                t if t == LINE_SUMMARY_CHANGED => {
                    let text = s(p, "text")?;
                    let ts = p.get("ts").and_then(Value::as_f64)?;
                    (s(p, "source").as_deref() == Some(at.as_str())).then_some(Change::Line { agent: at, text, ts })
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

/// Each sidebar's tap on this kernel, the last phase of each chat it has seen moving
/// (a chat that never moved, or went `idle`, has none), and each chat's latest line.
#[derive(Default)]
pub struct Follow {
    taps: Mutex<HashMap<String, SubscriberToken>>,
    phases: Mutex<HashMap<String, BTreeMap<String, String>>>,
    lines: Mutex<HashMap<String, BTreeMap<String, Value>>>,
}

fn follow(kernel: &Kernel) -> Arc<Follow> {
    kernel.extension::<Follow>("sidebar.follow")
}

/// The phases sidebar `me` holds now: `{chat: phase}`, each chat's last phase.
#[must_use]
pub fn phases(kernel: &Kernel, me: &AgentId) -> Value {
    let f = follow(kernel);
    let all = f.phases.lock().expect("phases poisoned");
    json!(all.get(me.as_str()).cloned().unwrap_or_default())
}

/// The lines sidebar `me` holds now: `{chat: {text, ts}}`, each chat's latest.
#[must_use]
pub fn lines(kernel: &Kernel, me: &AgentId) -> Value {
    let f = follow(kernel);
    let all = f.lines.lock().expect("lines poisoned");
    json!(all.get(me.as_str()).cloned().unwrap_or_default())
}

/// A chat's `status` kept: its phase is its last until the next one; `idle` (a resume with
/// nothing held) leaves it with none.
fn keep_phase(mine: &mut BTreeMap<String, String>, agent: &str, phase: &str) {
    if phase == "idle" {
        mine.remove(agent);
    } else {
        mine.insert(agent.to_string(), phase.to_string());
    }
}

/// How many questions to the person are open now (0 when `user` holds none or is absent).
#[must_use]
pub fn open_questions(kernel: &Arc<Kernel>) -> u64 {
    if !kernel.exists(PERSON) {
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
    let me_key = me.as_str().to_string();
    let me = me.clone();
    let token = kernel.publish_changes(me.clone(), change_of, move |k, c| {
        Some(match c {
            Change::Tree => json!({"type": TREE_CHANGED}),
            Change::Status { agent, phase } => {
                let f = follow(k);
                let mut all = f.phases.lock().expect("phases poisoned");
                keep_phase(
                    all.entry(me.as_str().to_string()).or_default(),
                    &agent,
                    &phase,
                );
                drop(all);
                json!({"type": STATUS_CHANGED, "agent": agent, "phase": phase})
            }
            Change::Line { agent, text, ts } => {
                let f = follow(k);
                f.lines
                    .lock()
                    .expect("lines poisoned")
                    .entry(me.as_str().to_string())
                    .or_default()
                    .insert(agent.clone(), json!({"text": text, "ts": ts}));
                json!({"type": LINE_SUMMARY_CHANGED, "agent": agent, "text": text, "ts": ts})
            }
            Change::Inquiries => json!({"type": INQUIRIES_CHANGED, "open": open_questions(k)}),
            Change::File { path, change } => json!({"type": FILES_CHANGED, "path": path, "change": change}),
        })
    });
    taps.insert(me_key, token);
}

/// Stop following (the sidebar is deleted, or the kernel stops).
pub fn stop(kernel: &Arc<Kernel>, me: &AgentId) {
    let f = follow(kernel);
    if let Some(token) = f.taps.lock().expect("taps poisoned").remove(me.as_str()) {
        kernel.remove_state_subscriber(token);
    }
    f.phases.lock().expect("phases poisoned").remove(me.as_str());
    f.lines.lock().expect("lines poisoned").remove(me.as_str());
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
            change_of(&emit("c1", json!({"type": LINE_SUMMARY_CHANGED, "source": "c1", "text": "reading the spec", "ts": 12.5}))),
            Some(Change::Line { agent: "c1".into(), text: "reading the spec".into(), ts: 12.5 })
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
    fn each_chat_keeps_its_last_phase_until_idle() {
        let mut mine = BTreeMap::new();
        // a call's phases, each the chat's last
        for p in ["queued", "thinking", "streaming", "retrying"] {
            keep_phase(&mut mine, "c1", p);
            assert_eq!(mine.get("c1").map(String::as_str), Some(p));
        }
        // the ends and a pause stay: the tree shows them until the next call
        for p in ["done", "failed", "stalled", "awaiting", "paused"] {
            keep_phase(&mut mine, "c1", "thinking");
            keep_phase(&mut mine, "c1", p);
            assert_eq!(mine.get("c1").map(String::as_str), Some(p), "{p} is kept");
        }
        keep_phase(&mut mine, "c2", "done");
        // the next call moves the chat on
        keep_phase(&mut mine, "c1", "queued");
        assert_eq!(mine.get("c1").map(String::as_str), Some("queued"));
        // a resume with nothing held: the chat is at rest, another chat keeps its own
        keep_phase(&mut mine, "c1", "idle");
        assert_eq!(
            mine,
            BTreeMap::from([("c2".to_string(), "done".to_string())])
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
        assert_eq!(change_of(&emit("w", json!({"type": LINE_SUMMARY_CHANGED, "source": "c1", "text": "x", "ts": 1.0}))), None);
        assert_eq!(change_of(&emit("c1", json!({"type": "inquiry_opened", "re": 4, "asker": "c1", "holder": "user"}))), None);
        assert_eq!(change_of(&emit("mail", json!({"type": "inquiry_opened", "re": 4, "asker": "c1", "holder": "mail"}))), None, "another holder's");
        // the log the process writes on every call, in place
        assert_eq!(change_of(&emit("files", json!({"type": "changed", "path": "team.log", "kind": "modified"}))), None);
        assert_eq!(change_of(&emit("w", json!({"type": "changed", "path": "a", "kind": "added"}))), None, "another bridge's");
        // the sidebar's own publications
        for t in [TREE_CHANGED, STATUS_CHANGED, INQUIRIES_CHANGED, FILES_CHANGED, LINE_SUMMARY_CHANGED] {
            assert_eq!(change_of(&emit("sidebar", json!({"type": t, "agent": "c1", "phase": "x", "text": "x", "ts": 1.0}))), None, "{t}");
        }
    }
}
