//! THE LEGS' REACH — god mode, by grants (owner, 2026-09-30: "godmode"; for
//! the MCP leg too, 2026-10-01: "* grants … it can shape grants later"; the
//! kernel: grants only, no kernel change).
//!
//! The human's surfaces that are not the in-proc operator reach every agent the
//! way the shell does: the MCP leg (`mcp`, a coding CLI), `user` — the shell's
//! own identity, which every canvas tab calls through (`user.forward`) — every
//! AI agent, and the host-side helpers that act for the human (`canvas_root`,
//! the canvas set-up agents) — a strictly single-user dev tool (owner
//! 2026-10-04). The shell is the operator (ungated); these are not, so THIS CLI
//! — the in-proc operator — writes BOTH edges, BY ID, for every agent:
//!
//! - the agent's `callable_by` names each holder with [`verbs`];
//! - each holder's `can_call` names the agent (and `kernel`) with the same.
//!
//! The canvas leg holds only what its own frames need: `watch` on every agent
//! and the state stream on the root ([`leg_verbs`]); its calls go through
//! `user`. Done for every agent when a leg is set up, and again for each
//! agent the state stream reports `added` (made by an AI, by exec, loaded from
//! disk, or a leg itself). Every grant lives in the records: the human reshapes
//! them with `update_agent`. A leg's `ingress_rule` still decides who may open
//! frames on it. Granted at every boot (`Host::boot`); the watch starts AFTER the
//! boot's own grant writes ([`start`], called by the launcher once its legs are
//! set up) — grant records are read, changed and written whole, so a watch
//! writing while the boot writes loses one side's entries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// The canvas leg's id.
pub const CANVAS_LEG: &str = crate::kern::web::CANVAS_WS_ID;

/// The legs that HOLD the reach themselves (the canvas leg reaches only
/// `user`).
pub const HOLDING_LEGS: [&str; 1] = [crate::mcp::LEG];

/// Every verb: `*` covers each one (the kernel has no privileged list).
pub fn verbs() -> Vec<&'static str> {
    vec!["*"]
}

fn field(h: &Host, id: &str, f: &str) -> Map<String, Value> {
    h.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .and_then(|a| a.value().record().meta.get(f).and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// The HOLDERS of the reach (strictly single-user dev tool, owner 2026-10-04):
/// each present holding leg, `user`, every AI agent (a non-root agent whose handler,
/// or the base of its role, is a backend or `ai_chat.tools`), and the host-side
/// helpers that act for the human — `canvas_root` and every canvas set-up agent.
fn holders(h: &Host) -> Vec<String> {
    let root = h.kernel.root().map(|r| r.id.0.clone());
    let helper = [crate::kern::backend::HANDLER_MODULE, crate::kern::setup_agent::HANDLER_MODULE];
    let mut v: Vec<String> = HOLDING_LEGS.into_iter().filter(|l| h.kernel.exists(l)).map(String::from).collect();
    if h.kernel.exists(crate::env::HUMAN) {
        v.push(crate::env::HUMAN.to_string());
    }
    let candidates: Vec<String> = h
        .kernel
        .agents
        .iter()
        .filter(|e| Some(&e.key().0) != root.as_ref())
        .filter(|e| {
            let r = e.value().record();
            r.handler_module.as_deref().is_some_and(|m| {
                let base = h.kernel.bundles.base_of(m);
                helper.contains(&m)
                    || [m, base.as_str()].iter().any(|n| {
                        matches!(
                            *n,
                            "anthropic_backend.tools"
                                | "ollama_backend.tools"
                                | "responses_backend.tools"
                                | "vllm_backend.tools"
                                | "foundation_models_backend.tools"
                                | "ai_chat.tools"
                        )
                    })
            })
        })
        .map(|e| e.key().0.clone())
        .collect();
    v.extend(candidates);
    v
}

/// What the canvas leg itself may do on `id`: WATCH it (a tab's terminal
/// output, an AI's tokens ride the leg's own frames), EMIT to it (a cell's
/// `reload_html`, a panel's gated `emit`), and on the root also tap the state
/// stream (the stream cell). Every call goes through `user` instead.
fn leg_verbs(h: &Host, id: &str) -> Value {
    if id == h.root || id == "kernel" {
        json!(["watch", "emit", "state_subscribe"])
    } else {
        json!(["watch", "emit"])
    }
}

/// `had` ∪ `add` as a verb list — `"*"` stays `"*"`; order kept, `add` after.
fn union(had: Option<&Value>, add: &Value) -> Value {
    if had == Some(&json!("*")) {
        return json!("*");
    }
    let mut out: Vec<Value> = had.and_then(Value::as_array).cloned().unwrap_or_default();
    for v in add.as_array().into_iter().flatten() {
        if !out.contains(v) {
            out.push(v.clone());
        }
    }
    Value::Array(out)
}

/// The agents whose `callable_by` their own code narrows to `user` (the user's
/// home files): no other holder is granted them here — `user` keeps its own
/// side (its `can_call`), the leg its own verbs.
const NARROWED: [&str; 2] = [crate::kern::saved::HOME_ID, crate::kern::user_store::ID];

/// Grant every holder every verb on `ids` — both edges, BY ID (no subtree
/// keys): each target's `callable_by` names each holder, and each holder's
/// `can_call` names the target (and `kernel`); the canvas leg, both edges, its
/// [`leg_verbs`]. One write per changed record.
/// Synchronous kernel edits, so the state watch makes them before the call that
/// made the agent returns ([`start`]).
fn write_grants(h: &Host, ids: &[String]) -> Result<(), String> {
    let all = json!(verbs());
    let holders = holders(h);
    let leg = h.kernel.exists(CANVAS_LEG);
    for id in ids {
        if !h.kernel.exists(id) {
            continue;
        }
        let narrowed = NARROWED.contains(&id.as_str());
        let lv_leg = leg_verbs(h, id);
        // one locked merge: an entry another writer puts there meanwhile survives
        h.kernel
            .edit_callable_by(&fantastic_kernel::AgentId::from(id.as_str()), |cb| {
                let mut changed = false;
                for holder in holders.iter().filter(|x| *x != id && !narrowed) {
                    if cb.get(holder) != Some(&all) {
                        cb.insert(holder.clone(), all.clone());
                        changed = true;
                    }
                }
                if leg && id != CANVAS_LEG {
                    // MERGED into what the record names (`user` admits the leg's calls)
                    let lv = union(cb.get(CANVAS_LEG), &lv_leg);
                    if cb.get(CANVAS_LEG) != Some(&lv) {
                        cb.insert(CANVAS_LEG.to_string(), lv);
                        changed = true;
                    }
                }
                changed
            })
            .map_err(|e| format!("reach: `{id}`.callable_by: {e}"))?;
    }
    let mut writers: Vec<(String, bool)> = holders.into_iter().map(|x| (x, true)).collect();
    if leg {
        writers.push((CANVAS_LEG.to_string(), false));
    }
    for (holder, full) in writers {
        // a holder that is not an agent here has no record to write (the old read-then-write skipped it too)
        if !h.kernel.exists(&holder) {
            continue;
        }
        h.kernel
            .edit_can_call(&fantastic_kernel::AgentId::from(holder.as_str()), |cc| {
                let mut changed = false;
                for id in ids.iter().map(String::as_str).chain(["kernel"]) {
                    if id == holder || (id != "kernel" && !h.kernel.exists(id)) || (full && NARROWED.contains(&id) && holder != crate::env::HUMAN) {
                        continue;
                    }
                    // the leg's own: MERGED into what it holds (its calls to `user`)
                    let want = if full { all.clone() } else { union(cc.get(id), &leg_verbs(h, id)) };
                    if cc.get(id) != Some(&want) {
                        cc.insert(id.to_string(), want);
                        changed = true;
                    }
                }
                changed
            })
            .map_err(|e| format!("reach: `{holder}`.can_call: {e}"))?;
    }
    write_back(h, ids)
}

/// THE WAY BACK (owner 2026-10-11: the canvas AI is god mode — it builds): every
/// agent may `send` TO every holder — an exec agent's monitor delivery, an
/// inquiry's answer, a sub-routine the AI built reporting back. Both edges, BY ID,
/// for every pair that involves one of `ids`: the holder's `callable_by` names the
/// agent with `send`, the agent's `can_call` names the holder with `send` (merged —
/// a `*` stays `*`). One write per changed record.
fn write_back(h: &Host, ids: &[String]) -> Result<(), String> {
    let send = json!(["send"]);
    let holders = holders(h);
    let root = h.kernel.root().map(|r| r.id.0.clone());
    // the agents that may START a call back: not a store (a store only answers),
    // not another holder (holders already reach each other with `*`)
    let held: std::collections::HashSet<&str> = holders.iter().map(String::as_str).collect();
    let everyone: Vec<String> = h
        .kernel
        .agents
        .iter()
        .filter(|e| {
            let id = e.key().0.as_str();
            Some(&e.key().0) != root.as_ref() && id != CANVAS_LEG && !held.contains(id)
                && !e.value().record().handler_module.as_deref().is_some_and(|m| {
                    matches!(
                        m,
                        "ram_store.tools" | "sqlite_store.tools" | "credential_store.tools" | "user_home_store.tools" | "user_store.tools"
                    )
                })
        })
        .map(|e| e.key().0.clone())
        .collect();
    let touched: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
    let pairs = |holder: &str| -> Vec<String> {
        everyone
            .iter()
            .filter(|x| x.as_str() != holder && (touched.contains(holder) || touched.contains(x.as_str())))
            .cloned()
            .collect()
    };
    // the holders' side: who may send to them
    for holder in &holders {
        if !h.kernel.exists(holder) {
            continue;
        }
        let xs = pairs(holder);
        h.kernel
            .edit_callable_by(&fantastic_kernel::AgentId::from(holder.as_str()), |cb| {
                let mut changed = false;
                for x in xs {
                    let want = union(cb.get(&x), &send);
                    if cb.get(&x) != Some(&want) {
                        cb.insert(x, want);
                        changed = true;
                    }
                }
                changed
            })
            .map_err(|e| format!("reach: `{holder}`.callable_by (the way back): {e}"))?;
    }
    // the senders' side: whom they may send to
    for x in &everyone {
        h.kernel
            .edit_can_call(&fantastic_kernel::AgentId::from(x.as_str()), |cc| {
                let mut changed = false;
                for holder in holders.iter().filter(|hd| hd.as_str() != x && (touched.contains(hd.as_str()) || touched.contains(x.as_str()))) {
                    let want = union(cc.get(holder), &send);
                    if cc.get(holder) != Some(&want) {
                        cc.insert(holder.clone(), want);
                        changed = true;
                    }
                }
                changed
            })
            .map_err(|e| format!("reach: `{x}`.can_call (the way back): {e}"))?;
    }
    Ok(())
}

/// Grant on every agent now.
pub async fn grant_all(h: &Host) -> Result<(), String> {
    write_all(h)
}

fn write_all(h: &Host) -> Result<(), String> {
    let ids: Vec<String> = h.kernel.agents.iter().map(|e| e.key().0.clone()).collect();
    write_grants(h, &ids)
}

static WATCHING: AtomicBool = AtomicBool::new(false);

/// Wakes [`granted`] after each batch the watch grants.
static GRANTED: std::sync::LazyLock<tokio::sync::Notify> = std::sync::LazyLock::new(tokio::sync::Notify::new);

/// How long [`granted`] waits for the watch.
const GRANT_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Wait until `user` — and the canvas leg, when there is one — hold their
/// grants on `id`: the watch writes them when the state stream reports the agent
/// `added`. An agent made from inside a handler cannot write those grants (they
/// come from above), so code that acts on the new agent through `user`, or a
/// page that watches it through the leg, awaits this first. Refused when the
/// watch has not granted it within [`GRANT_WAIT`].
pub async fn granted(h: &Host, id: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + GRANT_WAIT;
    loop {
        let wake = GRANTED.notified();
        let leg_ok = !h.kernel.exists(CANVAS_LEG) || field(h, CANVAS_LEG, "can_call").contains_key(id);
        if field(h, crate::env::HUMAN, "can_call").contains_key(id) && leg_ok {
            return Ok(());
        }
        if tokio::time::timeout_at(deadline, wake).await.is_err() {
            return Err(format!("reach: `{}` was not granted `{id}` within {}s", crate::env::HUMAN, GRANT_WAIT.as_secs()));
        }
    }
}

/// Every agent now, then each one the state stream reports `added`. The watch
/// starts once per process, AFTER the boot's own grant writes (the launcher
/// calls this last); a second call only grants.
pub async fn start(h: &Host) -> Result<(), String> {
    grant_all(h).await?;
    if WATCHING.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    // THE GRANTS ARE WRITTEN IN THE SUBSCRIBER, before the call that made the agent
    // returns (`create_agent` publishes `added` and only then answers): whoever
    // makes an agent and acts on it at once — an AI's `create_agent` then `send`, a
    // spawn's task, a `send` right after a `create_agent` — finds its edges there
    // (#467). The announcement, the waiters and the system block follow in a task.
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(String, Option<String>)>();
    let (weak_kernel, cfg, root) = (Arc::downgrade(&h.kernel), h.cfg.clone(), h.root.clone());
    let sub: fantastic_kernel::StateSubscriber = Arc::new(move |v: &Value, _body: Option<&fantastic_kernel::Blob>| {
        if v.get("kind").and_then(Value::as_str) != Some("added") {
            return;
        }
        let (Some(id), Some(kernel)) = (v.get("agent_id").and_then(Value::as_str), weak_kernel.upgrade()) else { return };
        let host = Host { kernel, cfg: cfg.clone(), root: root.clone() };
        // a new holder (a leg, an AI cell) reaches every agent, not just itself
        let r = if holders(&host).iter().any(|x| x == id) { write_all(&host) } else { write_grants(&host, &[id.to_string()]) };
        let _ = tx.send((id.to_string(), r.err()));
    });
    h.kernel.add_state_subscriber(sub);
    // A PAGE CONNECTING: `<guid>|*` is the only grant that reaches a connected
    // kernel's agents (the gate compares the guid literally — `*|*` matches
    // nothing). No event says a peer connected, but a page that connects sends,
    // and every send is on the state stream: each event checks the peer table
    // for a guid not granted yet (a weak kernel ref: no cycle through the
    // subscriber it registers).
    let (ptx, mut prx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let weak = Arc::downgrade(&h.kernel);
    let seen = Arc::new(std::sync::Mutex::new(std::collections::HashSet::<String>::new()));
    let peers: fantastic_kernel::StateSubscriber = Arc::new(move |_v: &Value, _body: Option<&fantastic_kernel::Blob>| {
        let Some(k) = weak.upgrade() else { return };
        let rows = k.peer_rows();
        let mut seen = seen.lock().expect("peers seen poisoned");
        for g in rows.as_array().into_iter().flatten().filter_map(|r| r.get("guid").and_then(Value::as_str)) {
            if seen.insert(g.to_string()) {
                let _ = ptx.send(g.to_string());
            }
        }
    });
    h.kernel.add_state_subscriber(peers);
    let peer_host = Host { kernel: Arc::clone(&h.kernel), cfg: h.cfg.clone(), root: h.root.clone() };
    tokio::spawn(async move {
        while let Some(guid) = prx.recv().await {
            if let Err(e) = grant_peer(&peer_host, &guid).await {
                tracing::warn!("{e}");
            }
        }
    });
    let host = Host { kernel: Arc::clone(&h.kernel), cfg: h.cfg.clone(), root: h.root.clone() };
    tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(more) = rx.try_recv() {
                batch.push(more);
            }
            for (id, refused) in &batch {
                if let Some(e) = refused {
                    tracing::warn!("{e}");
                }
                announce(&host.kernel, std::slice::from_ref(id), refused.as_deref());
            }
            GRANTED.notify_waiters();
            // a new AI reads the project's system block (`kern/prompt`)
            for (id, _) in &batch {
                if let Err(e) = crate::kern::prompt::stamp(&host, id).await {
                    tracing::warn!("{e}");
                }
            }
        }
    });
    Ok(())
}

/// The watch's outcome for each of `ids`, on the state stream: `granted` once the
/// caller's grant is written, or `grant_refused` with why it was not (`error`). A page
/// that has just been given a view (`open_view`) waits on this, not on a probe of its
/// own (the canvas's `awaitGrant`).
pub fn announce(kernel: &fantastic_kernel::Kernel, ids: &[String], refused: Option<&str>) {
    for id in ids {
        let ev = match refused {
            None => json!({"kind": "granted", "agent_id": id}),
            Some(e) => json!({"kind": "grant_refused", "agent_id": id, "error": e}),
        };
        kernel.publish_state(&ev);
    }
}

/// The human, the canvas leg and the holding legs (`mcp`) reach every agent of
/// the connected page `guid` (`<guid>|*`), merged into what their records name.
async fn grant_peer(h: &Host, guid: &str) -> Result<(), String> {
    let key = format!("{guid}|*");
    // the holding legs too (the `mcp` leg: a coding CLI reaches the page's
    // agents — the background, the view — as it reaches the host's)
    for holder in [crate::env::HUMAN, CANVAS_LEG].into_iter().chain(HOLDING_LEGS) {
        if !h.kernel.exists(holder) {
            continue;
        }
        h.kernel
            .edit_can_call(&fantastic_kernel::AgentId::from(holder), |cc| {
                if cc.get(&key) == Some(&json!(["*"])) {
                    return false;
                }
                cc.insert(key.clone(), json!(["*"]));
                true
            })
            .map_err(|e| format!("reach: `{holder}` → `{key}`: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{announce, leg_verbs, write_grants, CANVAS_LEG, NARROWED};
    use crate::kern::host::Host;
    use fantastic_kernel::{Agent, AgentId, Kernel};
    use serde_json::{json, Map, Value};
    use std::path::PathBuf;

    /// An agent with the given parent and meta, registered on `kernel`.
    fn put(kernel: &Kernel, id: &str, parent: Option<&str>, meta: Value) {
        put_as(kernel, id, "probe.tools", parent, meta);
    }

    /// [`put`] with the agent's handler named.
    fn put_as(kernel: &Kernel, id: &str, handler: &str, parent: Option<&str>, meta: Value) {
        let meta: Map<String, Value> = meta.as_object().cloned().unwrap_or_default();
        kernel.register(Agent::new(
            AgentId::from(id),
            Some(handler.to_string()),
            parent.map(AgentId::from),
            meta,
            PathBuf::from("/nonexistent"),
            false,
        ));
    }

    /// A host on a kernel rooted at `kernel` (the root is the kernel-state handler, so the
    /// reach's operator writes land), with the human `user` and the canvas leg registered.
    async fn host_with_leg(name: &str) -> Host {
        host_with_bundles(name, |_| {}).await
    }

    /// [`host_with_leg`] with more bundles registered on the kernel.
    async fn host_with_bundles(name: &str, more: impl FnOnce(&mut Kernel)) -> Host {
        let dir = std::env::temp_dir().join(format!("fc-reach-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
            state_dir: dir.join(".fantastic"),
            work_dir: dir.to_path_buf(),
        });
        kernel.bundles.register(
            fantastic_core::HANDLER_MODULE,
            fantastic_core::KernelStateBundle::default(),
        );
        more(&mut kernel);
        let kernel = std::sync::Arc::new(kernel);
        let root = Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            Map::new(),
            dir.join(".fantastic"),
            false,
        );
        kernel.register(std::sync::Arc::clone(&root));
        kernel.set_root(root);
        let h = Host { kernel, cfg: crate::env::ClientCfg::here(), root: "kernel".to_string() };
        for (id, parent) in [(crate::env::HUMAN, "kernel"), (CANVAS_LEG, "kernel")] {
            put(&h.kernel, id, Some(parent), json!({}));
        }
        h
    }

    #[tokio::test]
    async fn narrowed_agents_keep_the_human_alone_and_no_ai_is_granted_them() {
        let h = host_with_leg("narrowed").await;
        let k = &h.kernel;
        for id in NARROWED {
            put(k, id, Some(crate::env::HUMAN), json!({"callable_by": {crate::env::HUMAN: ["*"]}}));
        }
        put(k, "ordinary", Some(crate::env::HUMAN), json!({"callable_by": {crate::env::HUMAN: ["*"]}}));
        put_as(k, "master", "ai_chat.tools", Some("kernel"), json!({"memory_store_id": "m"}));
        put(k, "m", Some("kernel"), json!({}));
        let ids: Vec<String> = NARROWED.iter().map(|s| s.to_string()).chain(["ordinary".to_string()]).collect();
        write_grants(&h, &ids).unwrap();
        let field = |id: &str, f: &str| -> Value {
            k.agents.get(&AgentId::from(id)).unwrap().value().record().meta.get(f).cloned().unwrap_or(Value::Null)
        };
        // the human reaches every one of them
        for id in NARROWED {
            assert_eq!(field(crate::env::HUMAN, "can_call")[id], json!(["*"]), "{id}");
            assert!(field("master", "can_call").get(id).is_none(), "{id}: no AI may call it: {}", field("master", "can_call"));
            assert!(field(id, "callable_by").get("master").is_none(), "{id}: {}", field(id, "callable_by"));
        }
        // an ordinary agent is reached by the AI like any other
        assert_eq!(field("ordinary", "callable_by")["master"], json!(["*"]));
        assert_eq!(field("master", "can_call")["ordinary"], json!(["*"]));
    }

    #[tokio::test]
    async fn grant_on_an_agent_on_a_child_canvas_gives_the_holders_all_and_the_canvas_leg_its_edges() {
        let h = host_with_leg("child-canvas-grant").await;
        let k = &h.kernel;
        // a child canvas nested under the main one, an AI placed on it, and an ordinary AI holder
        put(k, "canvas_root", Some("kernel"), json!({}));
        put(k, "canvas_child_a", Some("canvas_root"), json!({}));
        put_as(k, "helper", "ai_chat.tools", Some("canvas_child_a"), json!({"memory_store_id": "m"}));
        put_as(k, "master", "ai_chat.tools", Some("kernel"), json!({"memory_store_id": "m"}));
        put(k, "m", Some("kernel"), json!({}));
        write_grants(&h, &["helper".to_string(), "canvas_child_a".to_string()]).unwrap();
        let field = |id: &str, f: &str| -> Value {
            k.agents.get(&AgentId::from(id)).unwrap().value().record().meta.get(f).cloned().unwrap_or(Value::Null)
        };
        // the human and every holder reach both; the leg watches and emits on them, both edges
        for id in ["helper", "canvas_child_a"] {
            assert_eq!(field(id, "callable_by")[crate::env::HUMAN], json!(["*"]), "{id}");
            assert_eq!(field(crate::env::HUMAN, "can_call")[id], json!(["*"]), "{id}");
            assert_eq!(field(id, "callable_by")["master"], json!(["*"]), "{id}");
            assert_eq!(field("master", "can_call")[id], json!(["*"]), "{id}");
            assert_eq!(field(id, "callable_by")[CANVAS_LEG], leg_verbs(&h, id), "{id}");
            assert_eq!(field(CANVAS_LEG, "can_call")[id], leg_verbs(&h, id), "{id}");
        }
    }

    #[tokio::test]
    async fn a_record_with_a_store_field_but_a_plain_handler_is_not_a_holder() {
        let h = host_with_leg("plain-with-store-field").await;
        let k = &h.kernel;
        put_as(k, "plain", "file_bridge.tools", Some("kernel"), json!({"memory_store_id": "m"}));
        put(k, "ordinary", Some("kernel"), json!({}));
        write_grants(&h, &["ordinary".to_string()]).unwrap();
        let can_call = k.agents.get(&AgentId::from("plain")).unwrap().value().record().meta.get("can_call").cloned().unwrap_or(Value::Null);
        assert!(can_call.get("ordinary").is_none(), "{can_call}");
    }

    #[tokio::test]
    async fn an_ai_chat_agent_is_a_holder_without_a_store_field() {
        let h = host_with_leg("chat-without-store-field").await;
        let k = &h.kernel;
        put_as(k, "chat", "ai_chat.tools", Some("kernel"), json!({}));
        put(k, "ordinary", Some("kernel"), json!({}));
        write_grants(&h, &["ordinary".to_string()]).unwrap();
        let can_call = k.agents.get(&AgentId::from("chat")).unwrap().value().record().meta.get("can_call").cloned().unwrap_or(Value::Null);
        assert_eq!(can_call["ordinary"], json!(["*"]), "{can_call}");
    }

    /// A role (a meta bundle) over `ai_chat.tools` holds the reach; one over another bundle does not.
    #[tokio::test]
    async fn an_agent_of_a_role_is_a_holder_by_the_base_of_its_role() {
        let h = host_with_bundles("role-holders", |k| {
            k.bundles.register("ai_chat.tools", Sink);
            k.bundles.register("file_bridge.tools", Sink);
            k.bundles.register_meta_yaml("chat_role", "base: ai_chat.tools\nsentence: s\n").unwrap();
            k.bundles.register_meta_yaml("file_role", "base: file_bridge.tools\nsentence: s\n").unwrap();
        })
        .await;
        let k = &h.kernel;
        put_as(k, "ai_one", "chat_role", Some("kernel"), json!({}));
        put_as(k, "plain_one", "file_role", Some("kernel"), json!({}));
        put(k, "ordinary", Some("kernel"), json!({}));
        write_grants(&h, &["ordinary".to_string()]).unwrap();
        let can_call = |id: &str| k.agents.get(&AgentId::from(id)).unwrap().value().record().meta.get("can_call").cloned().unwrap_or(Value::Null);
        assert_eq!(can_call("ai_one")["ordinary"], json!(["*"]), "{}", can_call("ai_one"));
        assert!(can_call("plain_one").get("ordinary").is_none(), "{}", can_call("plain_one"));
    }

    #[test]
    fn a_grant_the_watch_writes_is_announced_on_the_state_stream_and_a_refusal_says_why() {
        let kernel = Kernel::new();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
        let sink = std::sync::Arc::clone(&seen);
        kernel.add_state_subscriber(std::sync::Arc::new(move |v: &Value, _b: Option<&fantastic_kernel::Blob>| {
            sink.lock().unwrap().push(v.clone());
        }));
        announce(&kernel, &["view_a".to_string(), "view_b".to_string()], None);
        announce(&kernel, &["view_c".to_string()], Some("reach: `user`.can_call: refused"));
        // the kernel stamps each state event with its time: compared without it
        let seen: Vec<Value> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|v| {
                let mut v = v.clone();
                v.as_object_mut().expect("an object").remove("ts");
                v
            })
            .collect();
        assert_eq!(seen.len(), 3, "{seen:?}");
        assert_eq!(seen[0], json!({"kind": "granted", "agent_id": "view_a"}));
        assert_eq!(seen[1], json!({"kind": "granted", "agent_id": "view_b"}));
        assert_eq!(
            seen[2],
            json!({"kind": "grant_refused", "agent_id": "view_c", "error": "reach: `user`.can_call: refused"})
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_grant_and_a_concurrent_callable_by_edit_on_one_agent_both_survive() {
        let h = host_with_leg("grant-race").await;
        let k = &h.kernel;
        for round in 0..20 {
            let id = format!("helper{round}");
            put(k, &id, Some("kernel"), json!({}));
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            // the competing writer adds 50 distinct keys while write_grants() runs on another thread
            let (k2, id2, b2) = (std::sync::Arc::clone(k), id.clone(), std::sync::Arc::clone(&barrier));
            let writer = std::thread::spawn(move || {
                b2.wait();
                for n in 0..50 {
                    k2.edit_callable_by(&AgentId::from(id2.as_str()), |cb| {
                        cb.insert(format!("viewer{n}"), json!(["*"]));
                        true
                    })
                    .unwrap();
                }
            });
            let h2 = Host { kernel: std::sync::Arc::clone(k), cfg: crate::env::ClientCfg::here(), root: "kernel".to_string() };
            let id1 = id.clone();
            let granter = tokio::task::spawn_blocking(move || {
                barrier.wait();
                write_grants(&h2, &[id1])
            });
            granter.await.unwrap().unwrap();
            writer.join().unwrap();
            let cb = k.agents.get(&AgentId::from(id.as_str())).unwrap().record().meta.get("callable_by").cloned().unwrap_or(Value::Null);
            assert_eq!(cb.get(crate::env::HUMAN), Some(&json!(["*"])), "round {round}: {cb}");
            for n in 0..50 {
                assert_eq!(cb.get(format!("viewer{n}")), Some(&json!(["*"])), "round {round} viewer{n}: {cb}");
            }
        }
    }

    // ── #467: THE FIRST TURN OF A NEW AGENT NEVER RUNS BEFORE ITS GRANTS ──

    const PROBE_AI: &str = "ai_chat.tools";
    const SINK: &str = "sink.tools";

    fantastic_kernel::stand_in_verbs!(probe_ai_verbs {
        Send "send" { text: String },
        Make "make" { id: Option<String> },
    });

    /// A scripted AI, no model. `send` is its FIRST TURN: one tool call, to `sink`, made as
    /// itself and so gated by its own grants; the outcome goes to `turns`. `make` creates a
    /// child AI and sends it a message at once (an AI's `create_agent`, then `send`).
    struct ProbeAi {
        turns: tokio::sync::mpsc::UnboundedSender<(String, Value)>,
    }

    #[async_trait::async_trait]
    impl fantastic_kernel::bundle::Handler for ProbeAi {
        type Call = probe_ai_verbs::Call;

        fn name(&self) -> &str {
            PROBE_AI
        }

        fn man(&self) -> &'static str {
            "A test stand-in scripted AI."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            probe_ai_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            probe_ai_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(
            &self,
            agent_id: &AgentId,
            call_: Self::Call,
            kernel: &std::sync::Arc<Kernel>,
        ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
            let call = |target: &str, payload: Value| {
                let kernel = std::sync::Arc::clone(kernel);
                let target = AgentId::from(target);
                async move { kernel.send(vec![fantastic_kernel::Item::new(target, payload)]).await.remove(0).result }
            };
            Ok(match call_ {
                probe_ai_verbs::Call::Send(_) => {
                    let tool = call("sink", json!({"type": "ping"})).await;
                    let _ = self.turns.send((agent_id.0.clone(), tool.clone()));
                    Some(json!({"tool": tool}))
                }
                probe_ai_verbs::Call::Make(m) => {
                    let id = m.id.unwrap_or_else(|| "child_ai".to_string());
                    let made = call(
                        "kernel",
                        json!({"type": "create_agent", "parent_id": agent_id.0, "id": id,
                               "handler_module": PROBE_AI, "memory_store_id": "m"}),
                    )
                    .await;
                    let first = call(&id, json!({"type": "send", "text": "go"})).await;
                    Some(json!({"made": made, "first": first}))
                }
                probe_ai_verbs::Call::Reflect(_) => None,
            })
        }
    }

    fantastic_kernel::stand_in_verbs!(sink_verbs { Ping "ping" {} });

    /// An agent any granted caller reaches: it answers `ping`.
    struct Sink;

    #[async_trait::async_trait]
    impl fantastic_kernel::bundle::Handler for Sink {
        type Call = sink_verbs::Call;

        fn name(&self) -> &str {
            SINK
        }

        fn man(&self) -> &'static str {
            "A test stand-in that answers `ping`."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            sink_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            sink_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(
            &self,
            _agent_id: &AgentId,
            call: Self::Call,
            _kernel: &std::sync::Arc<Kernel>,
        ) -> Result<fantastic_kernel::bundle::Reply, fantastic_kernel::bundle::BundleError> {
            Ok(match call {
                sink_verbs::Call::Ping(_) => Some(json!({"pong": true})),
                sink_verbs::Call::Reflect(_) => None,
            })
        }
    }

    /// A runtime nobody drives; dropped from inside a test's runtime, it is shut down without blocking.
    struct Undriven(Option<tokio::runtime::Runtime>);

    impl Drop for Undriven {
        fn drop(&mut self) {
            if let Some(rt) = self.0.take() {
                rt.shutdown_background();
            }
        }
    }

    /// The watch is one per process (`WATCHING`); each of these tests starts it on its own
    /// kernel, one at a time.
    static ONE_WATCH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// A host with the watch running (on a runtime nobody drives), an AI `master` (a holder), the store `m` and `sink`
    /// made before it started; the receiver sees every first turn's tool outcome.
    async fn watched_host(
        name: &str,
    ) -> (tokio::sync::MutexGuard<'static, ()>, Undriven, Host, tokio::sync::mpsc::UnboundedReceiver<(String, Value)>) {
        let guard = ONE_WATCH.lock().await;
        super::WATCHING.store(false, std::sync::atomic::Ordering::SeqCst);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let h = host_with_bundles(name, |k| {
            k.bundles.register(PROBE_AI, ProbeAi { turns: tx });
            k.bundles.register(SINK, Sink);
        })
        .await;
        for (id, hm, meta) in [
            ("m", SINK, json!({})),
            ("sink", SINK, json!({})),
            ("master", PROBE_AI, json!({"memory_store_id": "m"})),
        ] {
            let mut spec = meta.as_object().cloned().unwrap_or_default();
            for (k, v) in [("type", "create_agent"), ("parent_id", "kernel"), ("id", id), ("handler_module", hm)] {
                spec.insert(k.into(), json!(v));
            }
            let made = h.operator("kernel", Value::Object(spec)).await.result;
            assert!(made.get("error").is_none(), "{id}: {made}");
        }
        // The watch runs on a runtime nobody drives: the state a loaded host is in when a
        // first turn arrives before the watch's task has been scheduled. Grants that wait
        // for that task never come; the ones the subscriber writes do not need it.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let watch = Host { kernel: std::sync::Arc::clone(&h.kernel), cfg: h.cfg.clone(), root: h.root.clone() };
        let rt = std::thread::spawn(move || {
            rt.block_on(super::start(&watch)).unwrap();
            rt
        })
        .join()
        .unwrap();
        (guard, Undriven(Some(rt)), h, rx)
    }

    async fn first_turn(rx: &mut tokio::sync::mpsc::UnboundedReceiver<(String, Value)>, who: &str) -> Value {
        let (id, tool) = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
            .await
            .expect("the first turn never ran")
            .expect("the probe is alive");
        assert_eq!(id, who);
        tool
    }

    #[tokio::test]
    async fn an_ais_create_agent_then_send_runs_a_first_turn_that_can_call_a_tool() {
        let (_one, _watch, h, mut rx) = watched_host("first-turn-ai").await;
        let out = h.operator("master", json!({"type": "make", "id": "child_ai"})).await.result;
        assert!(out["made"].get("error").is_none(), "{out}");
        assert_eq!(out["first"]["tool"], json!({"pong": true}), "the child's first turn: {out}");
        assert_eq!(first_turn(&mut rx, "child_ai").await, json!({"pong": true}));
    }

    #[tokio::test]
    async fn a_send_right_after_a_create_agent_runs_a_first_turn_that_can_call_a_tool() {
        // `fantastic send` twice: the create, then the message
        let (_one, _watch, h, mut rx) = watched_host("first-turn-send").await;
        let made = h
            .operator(
                "kernel",
                json!({"type": "create_agent", "parent_id": "kernel", "id": "teammate",
                       "handler_module": PROBE_AI, "memory_store_id": "m"}),
            )
            .await
            .result;
        assert!(made.get("error").is_none(), "{made}");
        let out = h.operator("teammate", json!({"type": "send", "text": "go"})).await.result;
        assert_eq!(out["tool"], json!({"pong": true}), "{out}");
        assert_eq!(first_turn(&mut rx, "teammate").await, json!({"pong": true}));
    }
}
