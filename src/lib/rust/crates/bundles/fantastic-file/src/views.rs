//! The VIEW AGENTS over the file bridge — python `file_bridge/views.py`,
//! `file_agent.py` and `folder_agent.py`, ported verb for verb. Read those first.
//!
//! A view is a record of this bundle carrying `view: file|folder`, a child of its
//! bridge. This bundle's handler answers it before the bridge's own gate: a view is
//! not sealed, its gate is its grant on the bridge (`GRANTS`, and the scope token its
//! bridge entry carries). Every disk operation is a send to the bridge AS THE VIEW, for
//! the view's own path; a verb naming another path is refused here, before the bridge
//! is asked. The bridge pushes a view its watch changes (`watch::relay_to_views`), so
//! the view is never a kernel watcher of its bridge.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use fantastic_bundle::py_repr;
use fantastic_kernel::inbox::{InboxItem, InboxReceiver};
use fantastic_kernel::lifecycle::create_from_payload;
use fantastic_kernel::refusal as rf;
use fantastic_kernel::{Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};

use super::{clamp_root, gate_path, norm, path_arg};

pub(crate) const FILE: &str = "file";
pub(crate) const FOLDER: &str = "folder";

/// python `views.GRANTS` — the verbs a view may send its bridge, nothing else.
const FILE_GRANTS: [&str; 4] = ["read", "write", "watch_path", "unwatch_path"];
const FOLDER_GRANTS: [&str; 3] = ["list", "watch_path", "unwatch_path"];
/// python `views.ANSWERS` — the verbs each view answers: what a caller may be granted on it.
const FILE_ANSWERS: [&str; 4] = ["read", "write", "watch", "reflect"];
const FOLDER_ANSWERS: [&str; 4] = ["list", "reveal", "watch", "reflect"];

/// python `file_agent.VERBS` docs — what `docs of=<file view>` serves: the view's own
/// table, never the bridge's (a view refuses a delete it is not granted).
const FILE_VERB_DOCS: [(&str, &str); 4] = [
    ("reflect", "Identity, the file it views, its bridge, and whether its change relay is on. No args."),
    ("read", "args: path? (this file only), offset:int=0 (line), limit:int=200 — the file's text page {path, lines:[{n,text}], total, eof, version}; refuses binary."),
    ("write", "args: bytes (req — the whole document), if_match:str? (the version you read; a changed document is refused) — atomic whole-document write; {path, size, version}."),
    ("watch", "args: none — start relaying this file's changes onto this agent: `{type: changed, path, kind: added|modified|deleted}`; WATCH this agent to receive them. Idempotent."),
];

/// python `folder_agent.VERBS` docs — the folder view's own table, as `FILE_VERB_DOCS`.
const FOLDER_VERB_DOCS: [(&str, &str); 4] = [
    ("reflect", "Identity, the folder it views, its bridge, and whether its change relay is on. No args."),
    ("list", "args: path? (a folder inside this one, relative to it; empty = this folder) — the folder's entries {files: [{name, path, type, size?}]}."),
    ("reveal", "args: path (req — a file or folder inside this one, relative to it) — shows where it sits: {revealed, folder_path, dirs} where dirs are the folders to open on the way down; not_found when nothing is there."),
    ("watch", "args: none — start relaying this folder's entry changes onto this agent: `{type: added|changed|removed, path, name}` (path relative to the folder's bridge); WATCH this agent to receive them. Idempotent."),
];

/// The verb docs a view record answers (python `instance_verbs(agent)`): its own
/// table for `view: file|folder`, None for any other record (the bridge's table).
pub(crate) fn verb_docs_of(meta: &Map<String, Value>) -> Option<Value> {
    let table: &[(&str, &str)] = match meta.get("view").and_then(Value::as_str)? {
        FILE => &FILE_VERB_DOCS,
        FOLDER => &FOLDER_VERB_DOCS,
        _ => return None,
    };
    Some(Value::Object(
        table
            .iter()
            .map(|(n, d)| (n.to_string(), json!(d)))
            .collect(),
    ))
}

/// python `views.FOLDER_EVENT` — the bridge's disk kind → a folder's event type. Any
/// other kind is None: the relay drops it, it is never relabelled as a change.
fn folder_event(raw: &str) -> Option<&'static str> {
    match raw {
        "added" => Some("added"),
        "modified" => Some("changed"),
        "deleted" => Some("removed"),
        _ => None,
    }
}

/// A view's grant on its bridge: its GRANTS.
fn grants(kind: &str) -> Vec<&'static str> {
    if kind == FILE {
        FILE_GRANTS.to_vec()
    } else {
        FOLDER_GRANTS.to_vec()
    }
}

/// python `views.scope_token`: the token a bridge's `callable_by` entry for a view
/// carries — its kind and path, `file:<path>` or `folder:<prefix>`.
fn scope_token(kind: &str, path: &str) -> String {
    format!("{kind}:{path}")
}

/// The `(kind, path)` a scope token names, or None for any other grant value.
fn scope_of_token(token: &str) -> Option<(&str, &str)> {
    let (kind, path) = token.split_once(':')?;
    (kind == FILE || kind == FOLDER).then_some((kind, path))
}

/// python `views.view_callers`: who may reach a view — whoever may reach its bridge, for
/// the verbs it answers. A bridge grant of `*` is all of them; a list keeps the verbs
/// the view answers. The bridge's `$parent` is named literally, its `$self` is skipped.
pub(crate) fn view_callers(
    grants: &Map<String, Value>,
    parent: Option<&str>,
    kind: &str,
) -> Map<String, Value> {
    let own: &[&str] = if kind == FILE {
        &FILE_ANSWERS
    } else {
        &FOLDER_ANSWERS
    };
    let mut out: std::collections::BTreeMap<String, BTreeSet<String>> = Default::default();
    for (caller, grant) in grants {
        if caller == "$self" {
            continue;
        }
        let caller = if caller == "$parent" {
            match parent {
                Some(p) => p.to_string(),
                None => continue,
            }
        } else {
            caller.clone()
        };
        let verbs: Vec<String> = if grant.as_str() == Some("*") {
            own.iter().map(|v| v.to_string()).collect()
        } else {
            grant
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|v| own.contains(v))
                .map(str::to_string)
                .collect()
        };
        if !verbs.is_empty() {
            out.entry(caller).or_default().extend(verbs);
        }
    }
    out.into_iter()
        .map(|(k, v)| (k, json!(v.into_iter().collect::<Vec<_>>())))
        .collect()
}

/// python `views.inside`: is the bridge path `target` inside the view's own path `own`?
/// A file is its own path only; a folder is itself and everything under it. An empty
/// `own` is no path at all, so it admits nothing.
pub(crate) fn inside(kind: &str, own: &str, target: &str) -> bool {
    if own.is_empty() {
        return false;
    }
    if kind == FILE {
        return target == own;
    }
    own == "." || target == own || target.starts_with(&format!("{own}/"))
}

/// python `views.caller_scope`: the `(kind, path)` a caller is scoped to on this bridge,
/// read from the bridge's own `callable_by` entry for it (its scope token). None for a
/// caller that is not path-scoped (an operator, or a plain agent granted on the bridge).
///
/// Fail closed: a VIEW caller whose entry has no single scope token (or two), or a view
/// of ANOTHER bridge, is returned with an empty path, which matches nothing.
pub(crate) fn caller_scope(
    kernel: &Arc<Kernel>,
    bridge: &str,
    caller: Option<&AgentId>,
) -> Option<(String, String)> {
    let caller = caller?;
    let meta = kernel.agents.get(caller)?.record().meta;
    let grant = kernel.agents.get(&AgentId::from(bridge)).and_then(|b| {
        b.record()
            .meta
            .get("callable_by")
            .and_then(Value::as_object)
            .and_then(|cb| cb.get(&caller.0).cloned())
    });
    let tokens: Vec<(&str, &str)> = grant
        .as_ref()
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(scope_of_token)
        .collect();
    let view = meta
        .get("view")
        .and_then(Value::as_str)
        .filter(|k| *k == FILE || *k == FOLDER);
    if let Some(kind) = view {
        if meta.get("bridge").and_then(Value::as_str) != Some(bridge) || tokens.len() != 1 {
            return Some((kind.to_string(), String::new()));
        }
    }
    match tokens.as_slice() {
        [(kind, path)] => Some((kind.to_string(), path.to_string())),
        _ => None,
    }
}

/// python `views.bridge_path`: the bridge path a view's relative `rel` names. A file
/// takes no operand but its own path; a folder's operand is inside the folder.
fn bridge_path(kind: &str, own: &str, rel: &str) -> Option<String> {
    if kind == FILE {
        let sub = if rel.is_empty() {
            Some(own.to_string())
        } else {
            norm(rel)
        };
        return (sub.as_deref() == Some(own)).then(|| own.to_string());
    }
    let sub = norm(rel)?;
    if own == "." {
        return Some(sub);
    }
    Some(if sub == "." {
        own.to_string()
    } else {
        format!("{own}/{sub}")
    })
}

/// python `views.scope`: the bridge path a verb's `path` names, or the refusal.
fn scope(verb: &str, kind: &str, own: &str, rel: Option<&Value>) -> Result<String, Value> {
    let rel = match rel {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(rf::bad(verb, "path", "a string")),
    };
    bridge_path(kind, own, rel).ok_or_else(|| {
        let mut v = rf::refusal(
            rf::UNAUTHORIZED,
            format!(
                "{verb}: {} is outside this agent's path {}",
                py_repr(rel),
                py_repr(own)
            ),
        );
        rf::with(
            &mut v,
            "hint",
            Value::String(
                "a view reaches only its own path; open a view of that path (open_view)"
                    .to_string(),
            ),
        );
        rf::with(&mut v, "path", Value::String(rel.to_string()));
        v
    })
}

/// python `_forward_args`: the verb's own arguments, minus the routing fields.
fn forward_args(payload: &Value) -> Map<String, Value> {
    payload
        .as_object()
        .map(|o| {
            o.iter()
                .filter(|(k, _)| k.as_str() != "type" && k.as_str() != "path")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect()
        })
        .unwrap_or_default()
}

/// python `send_bridge`: one send to the bridge, as the calling view. A send that
/// never settled with a reply is a refusal of its own, never a silent empty answer.
pub(crate) async fn send_bridge(kernel: &Arc<Kernel>, bridge: &str, payload: Value) -> Value {
    let verb = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let out = kernel
        .send(vec![Item::new(AgentId::from(bridge), payload)])
        .await
        .into_iter()
        .next();
    match out {
        Some(o) if o.result.is_object() => o.result,
        _ => {
            let mut v = rf::refusal(
                rf::TRANSPORT_ERROR,
                format!("{bridge} {verb}: the send did not settle with a reply"),
            );
            rf::with(&mut v, "bridge", Value::String(bridge.to_string()));
            v
        }
    }
}

/// The view records the kernel holds, by kind and bridge and path.
fn find_view(kernel: &Arc<Kernel>, bridge: &str, kind: &str, path: &str) -> Option<String> {
    kernel.agent_ids().into_iter().find_map(|id| {
        let meta = kernel.agents.get(&id)?.record().meta;
        let hit = meta.get("view").and_then(Value::as_str) == Some(kind)
            && meta.get("bridge").and_then(Value::as_str) == Some(bridge)
            && meta.get("path").and_then(Value::as_str) == Some(path);
        hit.then_some(id.0)
    })
}

/// python `open_view`: reuse the view of `path` on this bridge, or make it. The
/// bridge grants the new view exactly its grant in its `callable_by`. A reused view
/// whose entry holds no scope token (made before the token) is stamped with it; an entry
/// with any scope token is left as it is.
///
/// `chosen` is a caller-picked id for the new view. It is taken only when it is free: a
/// `chosen` that is another agent, or a view of another path, is refused; so is a path
/// that already has a view under another id. Nothing is replaced or ignored.
pub(crate) async fn open_view(
    kernel: &Arc<Kernel>,
    bridge: &Arc<Agent>,
    kind: &str,
    path: &str,
    chosen: Option<&str>,
) -> Value {
    let bid = bridge.id.0.clone();
    let existing = find_view(kernel, &bid, kind, path);
    if let Some(chosen) = chosen {
        if let Some(held) = kernel
            .agents
            .get(&AgentId::from(chosen))
            .map(|a| a.record().meta)
        {
            if !is_view_of(&held, &bid, kind, path) {
                let mut r = rf::refusal(
                    rf::EXISTS,
                    format!(
                        "open_view: {chosen:?} is already an agent, not a {kind} view of {path:?}"
                    ),
                );
                rf::with(&mut r, "id", json!(chosen));
                return r;
            }
            if let Err(r) = stamp(kernel, bridge, chosen, kind, path, true) {
                return r;
            }
            return json!({"id": chosen, "kind": kind, "path": path, "created": false});
        }
        if let Some(existing) = &existing {
            let mut r = rf::refusal(
                rf::EXISTS,
                format!("open_view: {path:?} already has the {kind} view {existing:?}"),
            );
            rf::with(&mut r, "hint", json!("open it by that id, or omit `id`"));
            rf::with(&mut r, "id", json!(existing));
            return r;
        }
    }
    if let Some(existing) = existing {
        if let Err(r) = stamp(kernel, bridge, &existing, kind, path, true) {
            return r;
        }
        return json!({"id": existing, "kind": kind, "path": path, "created": false});
    }
    let Some(root) = kernel.root() else {
        return json!({"error": "kernel: no root agent registered"});
    };
    let own_grants = grants(kind);
    let base = bridge_name(path);
    let bridge_grants = bridge
        .record()
        .meta
        .get("callable_by")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let bridge_parent = bridge.parent_id.as_ref().map(|p| p.0.clone());
    let mut payload = json!({
        "handler_module": super::HANDLER_MODULE,
        "parent_id": bid,
        "view": kind,
        "bridge": bid,
        "path": path,
        "display_name": base,
        "can_call": {bid.clone(): own_grants.clone()},
        "callable_by": Value::Object(view_callers(&bridge_grants, bridge_parent.as_deref(), kind)),
    });
    if let Some(chosen) = chosen {
        payload["id"] = json!(chosen);
    }
    let made = create_from_payload(kernel, &root, &payload).await;
    if made.get("error").is_some() {
        return made;
    }
    let vid = made
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Err(r) = stamp(kernel, bridge, &vid, kind, path, false) {
        return r;
    }
    json!({"id": vid, "kind": kind, "path": path, "created": true})
}

/// Grant the view `vid` its grants and its scope token in the bridge's `callable_by`.
/// With `only_unscoped`, an entry that already holds any scope token is left as it is (a
/// view made before the token is repaired; a deliberate narrowing stands). One step under
/// the record's lock: a key a concurrent grant adds is kept, and the change is announced
/// on the state stream so the root loader persists it.
fn stamp(
    kernel: &Arc<Kernel>,
    bridge: &Arc<Agent>,
    vid: &str,
    kind: &str,
    path: &str,
    only_unscoped: bool,
) -> Result<(), Value> {
    let scope = scope_token(kind, path);
    let edited = kernel.edit_callable_by(&bridge.id, |cb| {
        let held: Vec<String> = cb
            .get(vid)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if only_unscoped && held.iter().any(|v| scope_of_token(v).is_some()) {
            return false;
        }
        let mut set: BTreeSet<String> = held.into_iter().collect();
        set.extend(grants(kind).iter().map(|s| s.to_string()));
        set.insert(scope);
        let next = json!(set.into_iter().collect::<Vec<_>>());
        if cb.get(vid) == Some(&next) {
            return false;
        }
        cb.insert(vid.to_string(), next);
        true
    });
    edited.map(|_| ()).map_err(|e| {
        let mut r = rf::refusal(
            rf::INVALID_META,
            format!("open_view: the bridge did not take the grant for {vid:?}: {e}"),
        );
        rf::with(&mut r, "id", json!(vid));
        r
    })
}

/// Is this record the view of `path` of this kind on this bridge?
fn is_view_of(meta: &Map<String, Value>, bridge: &str, kind: &str, path: &str) -> bool {
    meta.get("view").and_then(Value::as_str) == Some(kind)
        && meta.get("bridge").and_then(Value::as_str) == Some(bridge)
        && meta.get("path").and_then(Value::as_str) == Some(path)
}

/// python `os.path.basename(path) or "."`.
fn bridge_name(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    if base.is_empty() {
        ".".to_string()
    } else {
        base.to_string()
    }
}

/// The views this bundle is running: each view's relay, by view id.
#[derive(Default)]
pub(crate) struct Relays {
    held: Mutex<HashMap<String, Relay>>,
}

struct Relay {
    inbox: AgentId,
    bridge: String,
    path: String,
}

/// python `views.relay_inbox`: the inbox a view's relay reads.
pub(crate) fn relay_inbox(view: &str) -> AgentId {
    AgentId::from(format!("{view}#relay"))
}

/// python `views.deliver`: put one change on a view's relay inbox (the inbox drops its
/// oldest when full). False, and logged, when the view has no relay inbox.
pub(crate) fn deliver(kernel: &Arc<Kernel>, view: &str, payload: Value) -> bool {
    let inbox = relay_inbox(view);
    match kernel.inboxes.get(&inbox) {
        Some(tx) => {
            tx.send(InboxItem::text(payload));
            true
        }
        None => {
            eprintln!("  [file_bridge] view relay: {view:?} has no relay inbox; change dropped");
            false
        }
    }
}

/// python `_relay_rule`: one bridge `changed` → the view's event, or nothing.
pub(crate) fn relay_event(kind: &str, own: &str, ev: &Value) -> Option<Value> {
    let p = ev.get("path").and_then(Value::as_str)?;
    let raw = ev.get("kind").and_then(Value::as_str);
    let Some(raw) = raw.filter(|k| matches!(*k, "added" | "modified" | "deleted")) else {
        // The bridge reports added / modified / deleted, always. Anything else is a bug
        // upstream: dropped and logged, never relabelled as a change.
        eprintln!("  [file_bridge] view relay: dropped event with kind {raw:?} for {p:?}");
        return None;
    };
    if kind == FILE {
        if p != own {
            return None;
        }
        return Some(json!({"type": "changed", "path": own, "kind": raw}));
    }
    let parent = match p.rsplit_once('/') {
        Some((d, _)) => d,
        None => ".",
    };
    if p == own || parent != own {
        return None; // the folder itself, or something deeper than its entries
    }
    let name = p.rsplit('/').next().unwrap_or(p);
    Some(json!({"type": folder_event(raw)?, "path": p, "name": name}))
}

/// python `_relay`: the view's inbox, drained — each bridge change it is handed
/// becomes the view's own event. The loop ends when the inbox is removed.
async fn relay(
    kernel: Arc<Kernel>,
    view: AgentId,
    mut rx: InboxReceiver,
    kind: String,
    own: String,
) {
    while let Some(item) = rx.recv().await {
        if item.payload.get("type").and_then(Value::as_str) != Some("changed") {
            continue;
        }
        if let Some(out) = relay_event(&kind, &own, &item.payload) {
            kernel.emit(&view, out).await;
        }
    }
}

/// python `views.watch`: arm the bridge's disk watch for this view's path once, and
/// relay its changes onto the view. The relay inbox is registered first, so the bridge's
/// first push has a reader.
async fn watch(
    kernel: &Arc<Kernel>,
    relays: &Relays,
    view: &AgentId,
    kind: &str,
    own: &str,
    bridge: &str,
) -> Value {
    if relays.held.lock().expect("relays").contains_key(&view.0) {
        return json!({"watching": own, "agent": view.0, "relayed": true});
    }
    let inbox = relay_inbox(&view.0);
    let rx = kernel.ensure_inbox(&inbox);
    let r = send_bridge(kernel, bridge, json!({"type": "watch_path", "path": own})).await;
    if r.get("error").is_some() {
        kernel.remove_inbox(&inbox);
        return r;
    }
    kernel.spawn_task(
        view,
        "watch",
        relay(
            Arc::clone(kernel),
            view.clone(),
            rx,
            kind.to_string(),
            own.to_string(),
        ),
    );
    relays.held.lock().expect("relays").insert(
        view.0.clone(),
        Relay {
            inbox,
            bridge: bridge.to_string(),
            path: own.to_string(),
        },
    );
    json!({"watching": own, "agent": view.0, "relayed": false})
}

/// python `views.stop`: the view is going away — its relay ends, and the bridge stops
/// pushing it changes (its path is let go).
pub(crate) async fn stop(kernel: &Arc<Kernel>, relays: &Relays, view: &AgentId) {
    let st = relays.held.lock().expect("relays").remove(&view.0);
    let Some(st) = st else { return };
    kernel.remove_inbox(&st.inbox);
    send_bridge(
        kernel,
        &st.bridge,
        json!({"type": "unwatch_path", "path": st.path}),
    )
    .await;
}

/// The view's verb table: `file` answers read, write, watch, reflect; `folder` answers
/// list, reveal, watch, reflect. Anything else is unhandled by the view.
pub(crate) async fn dispatch(
    kernel: &Arc<Kernel>,
    relays: &Relays,
    id: &AgentId,
    kind: &str,
    verb: &str,
    payload: &Value,
) -> Value {
    let meta = kernel
        .agents
        .get(id)
        .map(|a| a.record().meta)
        .unwrap_or_default();
    let own = meta
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let bridge = meta
        .get("bridge")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match (kind, verb) {
        (_, "reflect") => {
            let watching = relays.held.lock().expect("relays").contains_key(&id.0);
            reflect(kind, id, &meta, watching)
        }
        (FILE, "read" | "write") => match scope(verb, FILE, &own, payload.get("path")) {
            Err(e) => e,
            Ok(target) => {
                let mut body = forward_args(payload);
                body.insert("type".into(), json!(verb));
                body.insert("path".into(), json!(target));
                send_bridge(kernel, &bridge, Value::Object(body)).await
            }
        },
        (FILE | FOLDER, "watch") => watch(kernel, relays, id, kind, &own, &bridge).await,
        (FOLDER, "list") => match scope("list", FOLDER, &own, payload.get("path")) {
            Err(e) => e,
            Ok(target) => {
                send_bridge(kernel, &bridge, json!({"type": "list", "path": target})).await
            }
        },
        (FOLDER, "reveal") => reveal(kernel, &own, &bridge, payload).await,
        _ => {
            let mut v = rf::refusal(
                rf::UNHANDLED,
                format!("{kind} agent: unknown type {}", py_repr(verb)),
            );
            rf::with(&mut v, "verb", json!(verb));
            v
        }
    }
}

/// python `reflect` of a view: what it IS — its kind, path, bridge, and whether its
/// change relay is on (`watching`: the relay is held, as python's `RELAYS` has it).
fn reflect(kind: &str, id: &AgentId, meta: &Map<String, Value>, watching: bool) -> Value {
    let sentence = if kind == FILE {
        "A file on a file_bridge — one live object per path: its text, its writes and its changes."
    } else {
        "A folder on a file_bridge — one live object per folder: its entries and their changes."
    };
    json!({
        "id": id.0,
        "sentence": sentence,
        "kind": kind,
        "path": meta.get("path").cloned().unwrap_or(Value::Null),
        "bridge": meta.get("bridge").cloned().unwrap_or(Value::Null),
        "watching": watching,
    })
}

/// python `_reveal`: where a path inside the folder sits — the folders to open on the
/// way down. `not_found` when nothing is there.
async fn reveal(kernel: &Arc<Kernel>, own: &str, bridge: &str, payload: &Value) -> Value {
    let Some(rel) = payload
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return rf::missing("reveal", "path");
    };
    let target = match scope("reveal", FOLDER, own, payload.get("path")) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let parent = match target.rsplit_once('/') {
        Some((d, _)) => d.to_string(),
        None => ".".to_string(),
    };
    let listed = send_bridge(kernel, bridge, json!({"type": "list", "path": parent})).await;
    if listed.get("error").is_some() {
        return listed;
    }
    let name = target.rsplit('/').next().unwrap_or(&target);
    let present = listed
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|f| f.get("name").and_then(Value::as_str) == Some(name));
    if !present {
        let mut v = rf::refusal(
            rf::NOT_FOUND,
            format!("reveal: {} is not in this folder", py_repr(rel)),
        );
        rf::with(&mut v, "path", json!(rel));
        return v;
    }
    // The path inside the folder: python's `posixpath.relpath(target, own)`.
    let inside = if own == "." {
        target.clone()
    } else {
        target
            .strip_prefix(own)
            .map(|s| s.trim_start_matches('/'))
            .filter(|s| !s.is_empty())
            .unwrap_or(".")
            .to_string()
    };
    let dirs: Vec<String> = match inside.rsplit_once('/') {
        Some((d, _)) => {
            let parts: Vec<&str> = d.split('/').collect();
            (0..parts.len()).map(|i| parts[..=i].join("/")).collect()
        }
        None => Vec::new(),
    };
    json!({"revealed": inside, "folder_path": own, "dirs": dirs})
}

/// The `open_view` verb on a bridge: the arguments checked the way python checks
/// them, then the view reused or made. The path must be exposed (`allow`), as any
/// path verb's is.
pub(crate) async fn open_view_verb(
    kernel: &Arc<Kernel>,
    meta: &Map<String, Value>,
    agent_id: &AgentId,
    payload: &Value,
) -> Value {
    let kind = payload.get("kind").and_then(Value::as_str);
    let kind = match kind {
        Some(k @ (FILE | FOLDER)) => k,
        _ => return rf::bad("open_view", "kind", "`file` or `folder`"),
    };
    let path = match path_arg("open_view", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let Some(norm_path) = norm(path) else {
        return rf::bad("open_view", "path", "a root-relative path inside the root");
    };
    if kind == FILE && norm_path == "." {
        return rf::bad("open_view", "path", "a file path (the root is a folder)");
    }
    let allow = super::allow_list(meta);
    let root = clamp_root(meta, &kernel.workdir());
    if let Some(err) = gate_path("open_view", &root, &allow, &norm_path) {
        return err;
    }
    let chosen = match payload.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if let Some(err) = rf::guard_agent_id("open_view", s) {
                return err;
            }
            Some(s.as_str())
        }
        Some(_) => return rf::bad("open_view", "id", "an agent id (a string)"),
    };
    let Some(bridge) = kernel.agents.get(agent_id).map(|a| Arc::clone(a.value())) else {
        return json!({"error": format!("no agent {}", agent_id.0)});
    };
    open_view(kernel, &bridge, kind, &norm_path, chosen).await
}

/// The `validate_meta` judgement on the view field: a view is `file` or `folder`.
pub(crate) fn view_refusal(meta: &Map<String, Value>) -> Option<String> {
    match meta.get("view") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s == FILE || s == FOLDER => None,
        Some(_) => Some(
            "view must be `file` or `folder` — a view of one path on this bridge; open one \
             with open_view"
                .to_string(),
        ),
    }
}
