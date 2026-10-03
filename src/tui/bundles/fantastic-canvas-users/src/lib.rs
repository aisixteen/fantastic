//! THE CANVAS'S USERS (owner, 2026-10-02/03): one host agent, the GATEWAY
//! (`users`, `canvas_users.tools`), that every canvas tab calls. It finds the
//! USER a call is — by its token alone ([`grants::resolve`]) — and acts AS
//! that user agent, so the kernel's gate decides by the user's own grants.
//!
//! Verbs (each takes `token?`; a tab passes `tab: <its guid>` where it says):
//! - `forward {target, payload, timeout?, token?, tab?}` — the user sends
//!   `payload` to `target`; the reply comes back verbatim, the io_bridge
//!   contract (timeout default 30 s; inner marks re-rooted with
//!   `forwarded_secret`, the token never travels on). A call to the PAGE
//!   LOADER (the gateway's `page_loader`) is judged by the user's page grants
//!   and then made as the operator (the loader scopes record verbs to the
//!   caller's subtree): `load_tree` returns only what it sees, a record write
//!   needs a writing verb, and every accepted write is pushed to every user that sees
//!   it (`record_changed {id, record | removed, origin: <tab>}`). `load_tree`
//!   also returns the ANCESTORS of what the user sees, so its tree holds
//!   together (they stay unwritable). A `list_agents` reply keeps only the host
//!   agents the user's grants name.
//! - `watch_agent` / `unwatch_agent {target, tab}` — the user watches `target`
//!   (gated as the user, verb `watch`); what it mirrors lands in the user's
//!   events.
//! - `state_subscribe` / `state_unsubscribe {tab}` — the kernel's state
//!   stream, only what the user's grants name (`authorize_state_subscribe` as
//!   the user); the reply carries the filtered snapshot.
//! - `events {tab, after?, wait?}` — THE PULL: the user's events after `after`,
//!   waiting up to `wait` s (≤ 25) for one, at most [`BATCH`] per reply.
//!   `{events, next}`, or `{lost, next}` when the tab fell behind. No `after`:
//!   the tab starts at the queue's head (`{events: [], next}` at once) — it has
//!   the tree from `load_tree`, not the queue's history. An event with a raw body comes alone, its bytes
//!   the reply's body.
//! - `around {canvas?, x, y, radius}` — the cells of a page canvas (default:
//!   the tree's root) within `radius` of the point, nearest first, from the
//!   page RECORDS (no tab needs to show it): `{members: [{id, kind, title,
//!   rect, distance, backend_id?}]}`. Judged by the CALLER's own page grants: a
//!   tab (through the leg) by its token's user, any other agent — a cell's AI —
//!   by its own `can_call`.
//! - `reflect {token?}` — this gateway, and the user the token is: its id and
//!   its `can_call`.

pub mod grants;
pub mod queue;

use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{with_sender, AgentId, Blob, Item, Kernel, Status, Step, SubscriberToken};
use serde_json::{json, Map, Value};

use queue::{Queues, Watch};

/// The gateway's handler.
pub const HANDLER_MODULE: &str = "canvas_users.tools";
/// A user record's handler (it holds grants and a token hash; it answers only
/// `reflect`).
pub use grants::{DEFAULT_USER, USER_MODULE};

/// The verbs a tab calls on the gateway.
/// (`watch_agent`, never the kernel's own `watch`: a leg granted `watch` on the
/// gateway would mirror every call to it back onto the tab's socket.)
pub const VERBS: [&str; 9] =
    ["forward", "watch_agent", "unwatch_agent", "state_subscribe", "state_unsubscribe", "events", "around", "reflect", "boot"];

/// The most events one `events` reply carries.
pub const BATCH: usize = 200;
/// The longest `events` wait.
pub const MAX_WAIT: f64 = 25.0;
/// A tab that has not pulled for this long has gone: its watches drop.
pub const TAB_GONE: Duration = Duration::from_secs(90);
/// The bridge's default `forward` timeout (`io_bridge`).
pub const FORWARD_TIMEOUT: f64 = fantastic_bridge::DEFAULT_FORWARD_TIMEOUT_SECS;

/// A canvas's background: drawn behind its cells, never one of them.
const BACKGROUND: &str = "background.ts";

/// A cell's kind, as `place` names it, from its handler.
fn kind_of(hm: &str) -> &str {
    match hm {
        "html_agent.ts" => "html",
        "gl_agent.ts" => "gl",
        "terminal_view.ts" => "terminal",
        "ai_view.ts" => "chat",
        "canvas.ts" => "canvas",
        "setup_view.ts" => "setup",
        "stream_view.ts" => "stream",
        "reflect_view.ts" => "reflect",
        other => other,
    }
}

fn refuse(reason: &str, msg: impl Into<String>) -> Value {
    json!({"error": msg.into(), "reason": reason})
}

fn s<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get(k).and_then(Value::as_str).unwrap_or("")
}

/// The gateway bundle: its users' queues and its one state tap.
#[derive(Default)]
pub struct CanvasUsersBundle {
    queues: Arc<Queues>,
    tap: Mutex<Option<SubscriberToken>>,
}

/// A user record: it answers `reflect` only.
#[derive(Default)]
pub struct CanvasUserBundle;

#[async_trait]
impl Bundle for CanvasUserBundle {
    fn name(&self) -> &str {
        USER_MODULE
    }
    async fn handle(&self, _id: &AgentId, payload: &Value, _k: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Ok(match s(payload, "type") {
            "boot" | "shutdown" => None,
            "reflect" => Some(json!({
                "handler_module": USER_MODULE,
                "sentence": "A canvas user: its rights are its can_call (host agents by id, page agents as <tree guid>|<id>); a tab acts as it through the `users` gateway, by its token.",
            })),
            t => Some(refuse("unhandled", format!("canvas_user: unknown type {t:?}"))),
        })
    }
}

impl CanvasUsersBundle {
    fn page_loader(kernel: &Kernel, gw: &AgentId) -> Option<String> {
        kernel
            .agents
            .get(gw)
            .and_then(|a| a.value().record().meta.get("page_loader").and_then(Value::as_str).map(str::to_string))
    }

    /// Resolve the call's user, or the refusal.
    fn who(kernel: &Kernel, gw: &AgentId, p: &Value) -> Result<AgentId, Value> {
        grants::resolve(kernel, gw, p.get("token").and_then(Value::as_str)).map_err(|e| refuse("unauthorized", format!("users: {e}")))
    }

    /// One item as `user`, the bridge's way: the reply (and its raw body).
    async fn send_as(kernel: &Arc<Kernel>, user: &AgentId, item: Item) -> (Value, Option<Blob>) {
        let k = Arc::clone(kernel);
        let mut out = with_sender(user.clone(), async move { k.send(vec![item]).await }).await;
        let o = out.remove(0);
        let result = match o.status {
            Status::TimedOut if o.result.is_null() => refuse("timed_out", "users.forward: timeout"),
            _ => o.result,
        };
        (result, o.body)
    }

    /// One item to the PAGE LOADER as the operator. The loader scopes record
    /// verbs to the caller's own subtree (the page tree is the leg's), and the
    /// page grants judged here ARE the authorization of a page-record call.
    async fn loader_call(kernel: &Arc<Kernel>, item: Item) -> (Value, Option<Blob>) {
        let k = Arc::clone(kernel);
        let o = fantastic_kernel::with_operator(async move { k.send(vec![item]).await }).await.remove(0);
        (o.result, o.body)
    }

    /// The page tree as the loader holds it: `id → record`, and its guid.
    async fn tree(kernel: &Arc<Kernel>, loader: &str) -> (Vec<Value>, String) {
        let (r, _) = Self::loader_call(kernel, Item::new(AgentId::from(loader), json!({"type": "load_tree"}))).await;
        let recs = r.get("records").and_then(Value::as_array).cloned().unwrap_or_default();
        let guid = recs
            .iter()
            .find(|x| x.get("parent_id").and_then(Value::as_str) == Some(loader))
            .and_then(|x| x.get("guid").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        (recs, guid)
    }

    /// The ancestors of `id` (nearest first), from `parent` (an id, or none).
    fn ancestors(recs: &[Value], loader: &str, parent: Option<&str>) -> Vec<String> {
        let mut up = Vec::new();
        let mut cur = parent.map(str::to_string);
        while let Some(p) = cur.filter(|p| p != loader && !up.contains(p)) {
            cur = recs
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some(p.as_str()))
                .and_then(|r| r.get("parent_id").and_then(Value::as_str))
                .map(str::to_string);
            up.push(p);
        }
        up
    }

    fn page_verbs_of(kernel: &Kernel, user: &AgentId, recs: &[Value], loader: &str, guid: &str, id: &str, parent: Option<&str>) -> Vec<String> {
        let cc = grants::can_call(kernel, user);
        grants::page_verbs(&cc, guid, id, &Self::ancestors(recs, loader, parent))
    }

    /// Tell every user that sees page record `id` that it changed.
    fn announce(&self, kernel: &Arc<Kernel>, gw: &AgentId, recs: &[Value], loader: &str, guid: &str, id: &str, parent: Option<&str>, change: Value) {
        for u in grants::users(kernel, gw) {
            if grants::sees(&Self::page_verbs_of(kernel, &u, recs, loader, guid, id, parent)) {
                self.queues.push(&u, change.clone(), None);
            }
        }
    }

    /// `forward` — the user calls `target`; the page loader is filtered.
    async fn forward(&self, gw: &AgentId, p: &Value, body: Option<Blob>, kernel: &Arc<Kernel>) -> (Value, Option<Blob>) {
        let user = match Self::who(kernel, gw, p) {
            Ok(u) => u,
            Err(e) => return (e, None),
        };
        let target = s(p, "target");
        let Some(inner) = p.get("payload").filter(|v| v.is_object()).cloned() else {
            return (refuse("bad_arg", "users.forward: target (str) + payload (dict) required"), None);
        };
        if target.is_empty() {
            return (refuse("bad_arg", "users.forward: target (str) + payload (dict) required"), None);
        }
        let timeout = match p.get("timeout") {
            None | Some(Value::Null) => FORWARD_TIMEOUT,
            Some(v) => match v.as_f64() {
                Some(t) => t,
                None => return (refuse("bad_arg", "users.forward: timeout must be a number"), None),
            },
        };
        let mut item = Item::new(AgentId::from(target), inner.clone());
        item.timeout = Some(timeout);
        let marks = fantastic_bridge::forwarded_secret(&fantastic_kernel::current_secret());
        if !marks.is_empty() {
            item = item.with_secret(Value::Array(marks));
        }
        if let Some(b) = body {
            // the body's slot is under this call's `payload`: one step less on the far item
            let mut path = b.path;
            if path.first() == Some(&Step::Key("payload".into())) {
                path.remove(0);
            }
            item = item.with_binary(path, b.bytes);
        }
        let loader = Self::page_loader(kernel, gw);
        if loader.as_deref() != Some(target) {
            let (mut r, b) = Self::send_as(kernel, &user, item).await;
            // the host tree, as far as the user's grants name its agents
            if s(&inner, "type") == "list_agents" {
                let cc = grants::can_call(kernel, &user);
                if let Some(list) = r.get_mut("agents").and_then(Value::as_array_mut) {
                    list.retain(|a| a.get("id").and_then(Value::as_str).is_some_and(|id| grants::sees_host(&cc, id)));
                }
            }
            return (r, b);
        }
        let loader = loader.unwrap_or_default();
        let verb = s(&inner, "type");
        let (recs, guid) = Self::tree(kernel, &loader).await;
        match verb {
            "load_tree" => {
                let (mut r, b) = Self::loader_call(kernel, item).await;
                if let Some(list) = r.get_mut("records").and_then(Value::as_array_mut) {
                    // what the user sees, and the path up to the root (so the tree
                    // holds together; the page keeps that path read-only)
                    let mut keep: std::collections::HashSet<String> = std::collections::HashSet::new();
                    keep.insert(loader.clone()); // the loader's own anchor: where the page learns its leg
                    for rec in list.iter() {
                        let id = rec.get("id").and_then(Value::as_str).unwrap_or("");
                        let parent = rec.get("parent_id").and_then(Value::as_str);
                        if grants::sees(&Self::page_verbs_of(kernel, &user, &recs, &loader, &guid, id, parent)) {
                            keep.insert(id.to_string());
                            keep.extend(Self::ancestors(&recs, &loader, parent));
                        }
                    }
                    list.retain(|rec| rec.get("id").and_then(Value::as_str).is_some_and(|id| keep.contains(id)));
                }
                (r, b)
            }
            "persist_record" | "forget_record" => {
                let rec = inner.get("record");
                let id = if verb == "persist_record" { rec.and_then(|r| r.get("id")) } else { inner.get("id") }
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                // the parent: the one the write names, else the one on disk
                let parent = rec
                    .and_then(|r| r.get("parent_id"))
                    .or_else(|| inner.get("parent_id"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        recs.iter()
                            .find(|r| r.get("id").and_then(Value::as_str) == Some(id.as_str()))
                            .and_then(|r| r.get("parent_id").and_then(Value::as_str).map(str::to_string))
                    });
                // a NEW tree (its root has no guid on disk yet) is judged by its own guid
                let guid = if guid.is_empty() { rec.and_then(|r| r.get("guid")).and_then(Value::as_str).unwrap_or("").to_string() } else { guid };
                let v = Self::page_verbs_of(kernel, &user, &recs, &loader, &guid, &id, parent.as_deref());
                if !grants::writes(&v) {
                    return (refuse("unauthorized", format!("users: {} may not write the page record {id:?}", user.0)), None);
                }
                let (r, b) = Self::loader_call(kernel, item).await;
                if r.get("error").is_none() {
                    let change = if verb == "persist_record" {
                        json!({"type": "record_changed", "id": id, "record": rec, "origin": p.get("tab")})
                    } else {
                        json!({"type": "record_changed", "id": id, "removed": true, "origin": p.get("tab")})
                    };
                    self.announce(kernel, gw, &recs, &loader, &guid, &id, parent.as_deref(), change);
                }
                (r, b)
            }
            // any other loader verb: the user's own grants decide
            _ => Self::send_as(kernel, &user, item).await,
        }
    }

    /// `watch {target, tab}` as the user.
    async fn watch(&self, gw: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
        let user = match Self::who(kernel, gw, p) {
            Ok(u) => u,
            Err(e) => return e,
        };
        let (target, tab) = (s(p, "target").to_string(), s(p, "tab").to_string());
        if target.is_empty() || tab.is_empty() {
            return refuse("bad_arg", "users.watch_agent: target and tab are required");
        }
        let Some(t) = kernel.resolve_target(&target) else {
            return refuse("unknown_agent", format!("users.watch_agent: no agent {target:?}"));
        };
        let have = self.queues.with(&user, |q| {
            q.seen.insert(tab.clone(), Instant::now());
            match q.watches.get_mut(&target) {
                Some(w) => {
                    w.tabs.insert(tab.clone());
                    true
                }
                None => false,
            }
        });
        if have {
            return json!({"watching": target});
        }
        let reader = AgentId::from(format!("{}>{}>{}", gw.0, user.0, t.id.0).as_str());
        let rx = Arc::new(tokio::sync::Mutex::new(kernel.ensure_inbox(&reader)));
        let ready = kernel.ensure_inbox(&reader);
        let k = Arc::clone(kernel);
        let (tid, rd) = (t.id.clone(), reader.clone());
        if let Some(d) = with_sender(user.clone(), async move { k.watch(&tid, rd).await }).await {
            kernel.remove_inbox(&reader);
            return d;
        }
        let queues = Arc::clone(&self.queues);
        let (u2, src) = (user.clone(), target.clone());
        let task = Arc::clone(kernel).spawn_task(gw, "users_watch", async move {
            loop {
                {
                    let mut g = rx.lock().await;
                    while let Some(item) = g.try_recv() {
                        queues.push(&u2, json!({"type": "event", "src": src, "payload": item.payload}), item.body);
                    }
                }
                ready.ready().await;
            }
        });
        let mut tabs = std::collections::HashSet::new();
        tabs.insert(tab);
        self.queues.with(&user, |q| {
            q.watches.insert(target.clone(), Watch { reader, task, tabs });
        });
        json!({"watching": target})
    }

    /// Drop `tab` from `target`'s watch; the last tab out ends it.
    async fn unwatch_tab(&self, kernel: &Arc<Kernel>, user: &AgentId, target: &str, tab: &str) {
        let gone = self.queues.with(user, |q| {
            let empty = q.watches.get_mut(target).is_some_and(|w| {
                w.tabs.remove(tab);
                w.tabs.is_empty()
            });
            if empty {
                q.watches.remove(target)
            } else {
                None
            }
        });
        if let Some(w) = gone {
            if let Some(t) = kernel.resolve_target(target) {
                kernel.unwatch(&t.id, &w.reader).await;
            }
            let _ = kernel.cancel_task(&w.task).await;
            kernel.remove_inbox(&w.reader);
        }
    }

    /// Install the one state tap (once): each state event goes to every user
    /// with a subscribed tab whose grants name its agent. The kernel calls the
    /// tap SYNCHRONOUSLY, possibly holding its own locks, so the tap only hands
    /// the event over; a task reads the grants and fills the queues.
    fn ensure_tap(&self, kernel: &Arc<Kernel>, gw: &AgentId) {
        let mut g = self.tap.lock().expect("tap poisoned");
        if g.is_some() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        let tok = kernel.add_state_subscriber(Arc::new(move |v: &Value, _b: Option<&Blob>| {
            let _ = tx.send(v.clone());
        }));
        let queues = Arc::clone(&self.queues);
        let weak: Weak<Kernel> = Arc::downgrade(kernel);
        let gw_id = gw.0.clone();
        let _ = Arc::clone(kernel).spawn_task(gw, "users_state", async move {
            while let Some(v) = rx.recv().await {
                let Some(k) = weak.upgrade() else { return };
                let ids: Vec<String> = ["agent_id", "sender", "target"]
                    .iter()
                    .filter_map(|f| v.get(*f).and_then(Value::as_str).map(str::to_string))
                    .collect();
                // the gateway's OWN traffic (the pulls, the forwards) is not the
                // stream's news: relayed, every pull would make the next one's
                // reply, and the stream would never idle
                if ids.iter().any(|id| *id == gw_id) {
                    continue;
                }
                let subscribed: Vec<String> = {
                    let qs = queues.0.lock().expect("queues poisoned");
                    qs.iter().filter(|(_, q)| !q.state_tabs.is_empty()).map(|(u, _)| u.clone()).collect()
                };
                for u in subscribed {
                    let user = AgentId::from(u.as_str());
                    let cc = grants::can_call(&k, &user);
                    if ids.iter().all(|id| grants::sees_host(&cc, id)) {
                        queues.push(&user, json!({"type": "state_event", "payload": v}), None);
                    }
                }
            }
        });
        *g = Some(tok);
    }

    async fn state_subscribe(&self, gw: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
        let user = match Self::who(kernel, gw, p) {
            Ok(u) => u,
            Err(e) => return e,
        };
        let tab = s(p, "tab").to_string();
        if tab.is_empty() {
            return refuse("bad_arg", "users.state_subscribe: tab is required");
        }
        let k = Arc::clone(kernel);
        let u = user.clone();
        if let Some(d) = with_sender(user.clone(), async move { k.authorize_state_subscribe(&u) }).await {
            return d;
        }
        self.ensure_tap(kernel, gw);
        self.queues.with(&user, |q| {
            q.state_tabs.insert(tab.clone());
            q.seen.insert(tab, Instant::now());
        });
        let cc = grants::can_call(kernel, &user);
        let snap: Vec<Value> = kernel
            .state_snapshot()
            .into_iter()
            .filter(|a| a.get("id").or_else(|| a.get("agent_id")).and_then(Value::as_str).is_some_and(|id| grants::sees_host(&cc, id)))
            .collect();
        json!({"subscribed": true, "agents": snap})
    }

    /// Drop the tabs that stopped pulling: their watches and state taps.
    async fn sweep(&self, kernel: &Arc<Kernel>, user: &AgentId) {
        let stale: Vec<(String, Vec<String>)> = self.queues.with(user, |q| {
            let now = Instant::now();
            let gone: Vec<String> = q.seen.iter().filter(|(_, t)| now.duration_since(**t) > TAB_GONE).map(|(k, _)| k.clone()).collect();
            gone.iter()
                .map(|tab| {
                    q.seen.remove(tab);
                    q.state_tabs.remove(tab);
                    let targets = q.watches.iter().filter(|(_, w)| w.tabs.contains(tab)).map(|(t, _)| t.clone()).collect();
                    (tab.clone(), targets)
                })
                .collect()
        });
        for (tab, targets) in stale {
            for t in targets {
                self.unwatch_tab(kernel, user, &t, &tab).await;
            }
        }
    }

    /// `events {tab, after?, wait?}` — `(reply, body)`.
    async fn events(&self, gw: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> (Value, Option<Blob>) {
        let user = match Self::who(kernel, gw, p) {
            Ok(u) => u,
            Err(e) => return (e, None),
        };
        let tab = s(p, "tab").to_string();
        if tab.is_empty() {
            return (refuse("bad_arg", "users.events: tab is required"), None);
        }
        // no cursor: start at the head, now
        let Some(mut after) = p.get("after").and_then(Value::as_u64) else {
            let head = self.queues.with(&user, |q| {
                q.seen.insert(tab.clone(), Instant::now());
                q.seq
            });
            return (json!({"events": [], "next": head}), None);
        };
        // a cursor past this queue is from before the host restarted: start over
        if after > self.queues.with(&user, |q| q.seq) {
            after = 0;
        }
        let wait = p.get("wait").and_then(Value::as_f64).unwrap_or(MAX_WAIT).clamp(0.0, MAX_WAIT);
        self.sweep(kernel, &user).await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(wait);
        loop {
            let notify = self.queues.with(&user, |q| {
                q.seen.insert(tab.clone(), Instant::now());
                Arc::clone(&q.notify)
            });
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let got = self.queues.with(&user, |q| (q.after(after), q.seq));
            match got {
                (Err(()), now) => return (json!({"lost": true, "next": now}), None),
                (Ok(evs), _) if !evs.is_empty() => {
                    let next = evs.last().map_or(after, |e| e.seq);
                    if let Some(body) = evs[0].body.clone() {
                        let mut path = vec![Step::Key("events".into()), Step::Index(0)];
                        path.push(Step::Key("payload".into()));
                        path.extend(body.path);
                        let mut v = evs[0].v.clone();
                        v["seq"] = json!(evs[0].seq);
                        return (json!({"events": [v], "next": next}), Some(Blob::new(path, body.bytes)));
                    }
                    let list: Vec<Value> = evs
                        .into_iter()
                        .map(|e| {
                            let mut v = e.v;
                            v["seq"] = json!(e.seq);
                            v
                        })
                        .collect();
                    return (json!({"events": list, "next": next}), None);
                }
                (Ok(_), now) => {
                    if tokio::time::timeout_at(deadline, notified).await.is_err() {
                        return (json!({"events": [], "next": now.max(after)}), None);
                    }
                }
            }
        }
    }

    /// Whose page grants judge `around`: a tab's token user (it came in
    /// through the leg, the page loader's parent), else the originating agent
    /// itself; `None` = the operator (sees everything).
    fn judge(kernel: &Kernel, gw: &AgentId, p: &Value, loader: &str) -> Result<Option<AgentId>, Value> {
        let leg = kernel.agents.get(&AgentId::from(loader)).and_then(|a| a.value().parent_id.clone());
        match fantastic_kernel::current_origin() {
            None => Ok(None),
            Some(o) if Some(&o) == leg.as_ref() => Self::who(kernel, gw, p).map(Some),
            Some(o) => Ok(Some(o)),
        }
    }

    /// `around {canvas?, x, y, radius}` from the page records.
    async fn around(gw: &AgentId, p: &Value, kernel: &Arc<Kernel>) -> Value {
        let Some(loader) = Self::page_loader(kernel, gw) else {
            return refuse("not_found", "users.around: this gateway names no page_loader");
        };
        let num = |k: &str| p.get(k).and_then(Value::as_f64).filter(|v| v.is_finite());
        let (Some(x), Some(y), Some(radius)) = (num("x"), num("y"), num("radius")) else {
            return refuse("bad_arg", "users.around needs x, y and radius as numbers (radius > 0)");
        };
        if radius <= 0.0 {
            return refuse("bad_arg", "users.around needs x, y and radius as numbers (radius > 0)");
        }
        let judge = match Self::judge(kernel, gw, p, &loader) {
            Ok(j) => j,
            Err(e) => return e,
        };
        let (recs, guid) = Self::tree(kernel, &loader).await;
        let root = recs
            .iter()
            .find(|r| r.get("parent_id").and_then(Value::as_str) == Some(loader.as_str()))
            .and_then(|r| r.get("id").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let canvas = p.get("canvas").and_then(Value::as_str).filter(|c| !c.is_empty()).unwrap_or(&root).to_string();
        let sees = |id: &str, parent: Option<&str>| match &judge {
            None => true,
            Some(u) => grants::sees(&Self::page_verbs_of(kernel, u, &recs, &loader, &guid, id, parent)),
        };
        // a canvas the caller may not see answers no cells (each is judged below)
        if !recs.iter().any(|r| r.get("id").and_then(Value::as_str) == Some(canvas.as_str())) {
            return refuse("not_found", format!("users.around: no canvas {canvas:?} here"));
        }
        let mut members: Vec<(f64, Value)> = recs
            .iter()
            .filter(|r| r.get("parent_id").and_then(Value::as_str) == Some(canvas.as_str()))
            .filter(|r| r.get("handler_module").and_then(Value::as_str) != Some(BACKGROUND))
            .filter_map(|r| {
                let id = r.get("id").and_then(Value::as_str)?;
                let n = |k: &str| r.get(k).and_then(Value::as_f64);
                let (rx, ry, w, h) = (n("x")?, n("y")?, n("width")?, n("height")?);
                if !sees(id, Some(&canvas)) {
                    return None;
                }
                // to the rect's nearest edge; 0 inside
                let dx = (rx - x).max(0.0).max(x - (rx + w));
                let dy = (ry - y).max(0.0).max(y - (ry + h));
                let d = (dx * dx + dy * dy).sqrt();
                (d <= radius).then(|| {
                    let hm = r.get("handler_module").and_then(Value::as_str).unwrap_or("");
                    let mut m = json!({
                        "id": id,
                        "kind": kind_of(hm),
                        "title": r.get("display_name").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or(id),
                        "rect": {"x": rx, "y": ry, "w": w, "h": h},
                        "distance": (d * 10.0).round() / 10.0,
                    });
                    if let Some(b) = r.get("backend_id").and_then(Value::as_str) {
                        m["backend_id"] = json!(b);
                    }
                    (d, m)
                })
            })
            .collect();
        members.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        json!({"canvas": canvas, "members": members.into_iter().map(|(_, m)| m).collect::<Vec<_>>()})
    }

    async fn run(&self, gw: &AgentId, p: &Value, body: Option<Blob>, kernel: &Arc<Kernel>) -> (Reply, Option<Blob>) {
        let reply = match s(p, "type") {
            "boot" | "shutdown" => return (None, None),
            "reflect" => {
                let mut r = json!({
                    "handler_module": HANDLER_MODULE,
                    "sentence": "The canvas's users gateway: a tab's every host call goes through here with its token; it acts AS the user that token is (its can_call is the user's every right). `around {canvas?, x:num, y:num, radius:num}` → {canvas, members: [{id, kind, title, rect:{x,y,w,h}, distance, backend_id?}]}: the cells of a page canvas (default its root) within radius of the point, nearest first, from the page records — judged by YOUR page grants; reflect a cell at <page guid>|<id> for more.",
                    "verbs": VERBS,
                });
                match Self::who(kernel, gw, p) {
                    Ok(u) => {
                        r["as"] = json!(u.0);
                        r["can_call"] = Value::Object(grants::can_call(kernel, &u));
                    }
                    Err(e) => r["as_error"] = e,
                }
                r
            }
            "forward" => {
                let (r, b) = self.forward(gw, p, body, kernel).await;
                return (Some(r), b);
            }
            "watch_agent" => self.watch(gw, p, kernel).await,
            "unwatch_agent" => match Self::who(kernel, gw, p) {
                Ok(u) => {
                    self.unwatch_tab(kernel, &u, s(p, "target"), s(p, "tab")).await;
                    json!({"unwatched": s(p, "target")})
                }
                Err(e) => e,
            },
            "state_subscribe" => self.state_subscribe(gw, p, kernel).await,
            "around" => Self::around(gw, p, kernel).await,
            "state_unsubscribe" => match Self::who(kernel, gw, p) {
                Ok(u) => {
                    self.queues.with(&u, |q| q.state_tabs.remove(s(p, "tab")));
                    json!({"subscribed": false})
                }
                Err(e) => e,
            },
            "events" => {
                let (r, b) = self.events(gw, p, kernel).await;
                return (Some(r), b);
            }
            t => refuse("unhandled", format!("users: unknown type {t:?}")),
        };
        (Some(reply), None)
    }
}

#[async_trait]
impl Bundle for CanvasUsersBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        Ok(self.run(agent_id, payload, None, kernel).await.0)
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        // every call lands here (the kernel's binary path); a reply's raw body
        // goes back on whatever leg it came in
        Ok(self.run(agent_id, &header, body, kernel).await)
    }

    fn validate_meta(&self, meta: &Map<String, Value>, _record: &Map<String, Value>) -> Option<String> {
        match meta.get("page_loader") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) if !s.is_empty() => None,
            Some(_) => Some("page_loader must be the page loader's agent id".into()),
        }
    }
}
