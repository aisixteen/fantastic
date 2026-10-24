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
//!
//! CABINETS ARE NOT REACHED BY EVERY HOLDER. A cabinet, its worker, its canvas and
//! everything on that canvas ([`cabinet_owned`]) keep the grants `fantastic_cabinet`
//! writes: messages between workers, and equipment granted inside its cabinet. This reach
//! would otherwise name every AI on both edges of a worker, which is god mode for the
//! worker. The grants it still writes for them are the human's own egress (`user` draws
//! the cabinet's panel, calls its `view`/`watch`, and calls the equipment on its canvas)
//! and the canvas leg's frames (`watch`/`emit`, [`leg_verbs`]): a cabinet's canvas is
//! served and openable like a peer canvas. No other holder reaches them.

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

fn exists(h: &Host, id: &str) -> bool {
    h.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(id))
}

fn field(h: &Host, id: &str, f: &str) -> Map<String, Value> {
    h.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .and_then(|a| a.value().record().meta.get(f).and_then(Value::as_object).cloned())
        .unwrap_or_default()
}


/// The ids of every cabinet, its worker, its canvas and everything on that canvas (its
/// equipment, and any canvas nested under it): an agent whose own record or one of whose
/// ancestors is a cabinet (`cabinet_role: cabinet`) or a cabinet's `canvas`. Computed from
/// one snapshot of the records, so it never reads the tree while the tree is iterated.
pub fn cabinet_owned(kernel: &fantastic_kernel::Kernel) -> std::collections::HashSet<String> {
    let rows: std::collections::HashMap<String, (Option<String>, bool)> = kernel
        .agents
        .iter()
        .map(|e| {
            let r = e.value().record();
            let cabinet = r.meta.get("cabinet_role").and_then(Value::as_str) == Some("cabinet");
            (e.key().0.clone(), (r.parent_id, cabinet))
        })
        .collect();
    // a cabinet's canvas roots its own owned subtree, though it is a child of the canvas
    // that spawned it, not of the cabinet
    let canvases: std::collections::HashSet<String> = kernel
        .agents
        .iter()
        .filter(|e| e.value().record().meta.get("cabinet_role").and_then(Value::as_str) == Some("cabinet"))
        .filter_map(|e| e.value().record().meta.get("canvas").and_then(Value::as_str).map(str::to_string))
        .collect();
    let mut owned = std::collections::HashSet::new();
    for id in rows.keys() {
        let mut cur = Some(id.clone());
        while let Some(c) = cur {
            let Some((parent, cabinet)) = rows.get(&c) else { break };
            if *cabinet || canvases.contains(&c) {
                owned.insert(id.clone());
                break;
            }
            cur = parent.clone();
        }
    }
    owned
}

/// The HOLDERS of the reach (strictly single-user dev tool, owner 2026-10-04):
/// each present holding leg, `user`, every AI agent (a non-root agent naming a
/// memory store), and the host-side helpers that act for the human —
/// `canvas_root` and every canvas set-up agent. A cabinet's agents are never holders
/// ([`cabinet_owned`]).
fn holders(h: &Host) -> Vec<String> {
    let root = h.kernel.root().map(|r| r.id.0.clone());
    let helper = [crate::kern::backend::HANDLER_MODULE, crate::kern::setup_agent::HANDLER_MODULE];
    let owned = cabinet_owned(&h.kernel);
    let mut v: Vec<String> = HOLDING_LEGS.into_iter().filter(|l| exists(h, l)).map(String::from).collect();
    if exists(h, crate::env::HUMAN) {
        v.push(crate::env::HUMAN.to_string());
    }
    v.extend(
        h.kernel
            .agents
            .iter()
            .filter(|e| Some(&e.key().0) != root.as_ref() && !owned.contains(&e.key().0))
            .filter(|e| {
                let r = e.value().record();
                r.meta.get("memory_store_id").and_then(Value::as_str).is_some()
                    || r.handler_module.as_deref().is_some_and(|m| helper.contains(&m))
            })
            .map(|e| e.key().0.clone()),
    );
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
/// [`leg_verbs`]. A cabinet's agents ([`cabinet_owned`]) take only the human's
/// and the canvas leg's grants. One write per changed record.
pub async fn grant(h: &Host, ids: &[String]) -> Result<(), String> {
    let all = json!(verbs());
    let holders = holders(h);
    let owned = cabinet_owned(&h.kernel);
    let leg = exists(h, CANVAS_LEG);
    for id in ids {
        if !exists(h, id) {
            continue;
        }
        let narrowed = NARROWED.contains(&id.as_str());
        let cabinet = owned.contains(id);
        let lv_leg = leg_verbs(h, id);
        // one locked merge: an entry another writer puts there meanwhile survives
        h.kernel
            .edit_callable_by(&fantastic_kernel::AgentId::from(id.as_str()), |cb| {
                let mut changed = false;
                for holder in holders.iter().filter(|x| *x != id && !narrowed && (!cabinet || *x == crate::env::HUMAN)) {
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
        h.kernel
            .edit_can_call(&fantastic_kernel::AgentId::from(holder.as_str()), |cc| {
                let mut changed = false;
                for id in ids.iter().map(String::as_str).chain(["kernel"]) {
                    if id == holder || (id != "kernel" && !exists(h, id)) || (full && NARROWED.contains(&id) && holder != crate::env::HUMAN) {
                        continue;
                    }
                    // a cabinet's agents: only the human's own calls and the canvas leg's frames
                    // reach them (its panel and its canvas are `user`'s and the leg's)
                    if id != "kernel" && owned.contains(id) && holder != crate::env::HUMAN && holder != CANVAS_LEG {
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
    grant_back(h, ids).await
}

/// THE WAY BACK (owner 2026-10-11: the canvas AI is god mode — it builds): every
/// agent may `send` TO every holder — an exec agent's monitor delivery, an
/// inquiry's answer, a sub-routine the AI built reporting back. Both edges, BY ID,
/// for every pair that involves one of `ids`: the holder's `callable_by` names the
/// agent with `send`, the agent's `can_call` names the holder with `send` (merged —
/// a `*` stays `*`). One write per changed record.
pub async fn grant_back(h: &Host, ids: &[String]) -> Result<(), String> {
    let send = json!(["send"]);
    let holders = holders(h);
    let root = h.kernel.root().map(|r| r.id.0.clone());
    let owned = cabinet_owned(&h.kernel);
    // the agents that may START a call back: not a store (a store only answers),
    // not another holder (holders already reach each other with `*`), not a cabinet's
    let held: std::collections::HashSet<&str> = holders.iter().map(String::as_str).collect();
    let everyone: Vec<String> = h
        .kernel
        .agents
        .iter()
        .filter(|e| {
            let id = e.key().0.as_str();
            Some(&e.key().0) != root.as_ref() && id != CANVAS_LEG && !held.contains(id) && !owned.contains(id)
                && !e.value().record().handler_module.as_deref().is_some_and(|m| m.contains("store"))
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
    let ids: Vec<String> = h.kernel.agents.iter().map(|e| e.key().0.clone()).collect();
    grant(h, &ids).await
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
        let leg_ok = !exists(h, CANVAS_LEG) || field(h, CANVAS_LEG, "can_call").contains_key(id);
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
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let sub: fantastic_kernel::StateSubscriber = Arc::new(move |v: &Value, _body: Option<&fantastic_kernel::Blob>| {
        if v.get("kind").and_then(Value::as_str) == Some("added") {
            if let Some(id) = v.get("agent_id").and_then(Value::as_str) {
                let _ = tx.send(id.to_string());
            }
        }
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
        while let Some(id) = rx.recv().await {
            let mut batch = vec![id];
            while let Ok(more) = rx.try_recv() {
                batch.push(more);
            }
            // a new holder (a leg, an AI cell) reaches every agent, not just the batch
            let holders_now = holders(&host);
            let r = if batch.iter().any(|b| holders_now.contains(b)) {
                grant_all(&host).await
            } else {
                grant(&host, &batch).await
            };
            if let Err(e) = r {
                tracing::warn!("{e}");
            }
            GRANTED.notify_waiters();
            // a new AI reads the project's system block (`kern/prompt`)
            for id in &batch {
                if let Err(e) = crate::kern::prompt::stamp(&host, id).await {
                    tracing::warn!("{e}");
                }
            }
        }
    });
    Ok(())
}

/// The human, the canvas leg and the holding legs (`mcp`) reach every agent of
/// the connected page `guid` (`<guid>|*`), merged into what their records name.
async fn grant_peer(h: &Host, guid: &str) -> Result<(), String> {
    let key = format!("{guid}|*");
    // the holding legs too (the `mcp` leg: a coding CLI reaches the page's
    // agents — the background, the view — as it reaches the host's)
    for holder in [crate::env::HUMAN, CANVAS_LEG].into_iter().chain(HOLDING_LEGS) {
        if !exists(h, holder) {
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
    use super::{cabinet_owned, grant, leg_verbs, CANVAS_LEG};
    use crate::kern::host::Host;
    use fantastic_kernel::{Agent, AgentId, Kernel};
    use serde_json::{json, Map, Value};
    use std::path::PathBuf;

    /// An agent with the given parent and meta, registered on `kernel`.
    fn put(kernel: &Kernel, id: &str, parent: Option<&str>, meta: Value) {
        let meta: Map<String, Value> = meta.as_object().cloned().unwrap_or_default();
        kernel.register(Agent::new(
            AgentId::from(id),
            Some("probe.tools".to_string()),
            parent.map(AgentId::from),
            meta,
            PathBuf::from("/nonexistent"),
            false,
        ));
    }

    #[test]
    fn a_cabinet_its_worker_and_the_equipment_are_owned_and_nothing_else() {
        let kernel = Kernel::new();
        put(&kernel, "manager", None, json!({"cabinet_role": "manager", "memory_store_id": "m"}));
        put(&kernel, "cabinet_a", Some("manager"), json!({"cabinet_role": "cabinet"}));
        put(&kernel, "cabinet_a_worker", Some("cabinet_a"), json!({"memory_store_id": "m"}));
        put(&kernel, "a_exec", Some("cabinet_a_worker"), json!({}));
        put(&kernel, "a_exec_sub", Some("a_exec"), json!({}));
        put(&kernel, "master", None, json!({"memory_store_id": "m"}));
        put(&kernel, "m", Some("manager"), json!({}));
        let owned = cabinet_owned(&kernel);
        for id in ["cabinet_a", "cabinet_a_worker", "a_exec", "a_exec_sub"] {
            assert!(owned.contains(id), "{id} is a cabinet's: {owned:?}");
        }
        // the manager, the master (an ordinary AI) and the store are not
        for id in ["manager", "master", "m"] {
            assert!(!owned.contains(id), "{id} is not a cabinet's: {owned:?}");
        }
    }

    #[test]
    fn a_cabinets_canvas_and_what_is_placed_on_it_are_owned_but_not_the_canvas_that_spawned_it() {
        // the spawning canvas is an ordinary canvas (a holder); its child canvas is the cabinet's
        let kernel = Kernel::new();
        put(&kernel, "canvas_root", None, json!({}));
        put(&kernel, "manager", Some("canvas_root"), json!({"cabinet_role": "manager", "canvas_id": "canvas_root"}));
        put(&kernel, "cabinet_a", Some("manager"), json!({"cabinet_role": "cabinet", "canvas": "canvas_cabinet_a"}));
        put(&kernel, "cabinet_a_worker", Some("cabinet_a"), json!({"memory_store_id": "m"}));
        put(&kernel, "canvas_cabinet_a", Some("canvas_root"), json!({}));
        put(&kernel, "helper", Some("canvas_cabinet_a"), json!({"memory_store_id": "m"}));
        put(&kernel, "canvas_deeper", Some("canvas_cabinet_a"), json!({}));
        put(&kernel, "deep_exec", Some("canvas_deeper"), json!({}));
        put(&kernel, "canvas_root_helper", Some("canvas_root"), json!({"memory_store_id": "m"}));
        put(&kernel, "m", Some("canvas_root"), json!({}));
        let owned = cabinet_owned(&kernel);
        for id in ["canvas_cabinet_a", "helper", "canvas_deeper", "deep_exec"] {
            assert!(owned.contains(id), "{id} is on a cabinet's canvas: {owned:?}");
        }
        // the spawning canvas, its own AI, and the store stay holders of the reach
        for id in ["canvas_root", "canvas_root_helper", "m", "manager"] {
            assert!(!owned.contains(id), "{id} is not a cabinet's: {owned:?}");
        }
    }

    /// A host on a kernel rooted at `kernel` (the root is the kernel-state handler, so the
    /// reach's operator writes land), with the human `user` and the canvas leg registered.
    async fn host_with_leg(name: &str) -> Host {
        let dir = std::env::temp_dir().join(format!("fc-reach-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut kernel = Kernel::new();
        kernel.bundles.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
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
    async fn grant_on_a_cabinets_equipment_gives_the_human_and_the_canvas_leg_their_edges_only() {
        let h = host_with_leg("owned-grant").await;
        let k = &h.kernel;
        // a cabinet on the canvas that spawns it; the equipment (a helper AI) is on the cabinet's canvas
        put(k, "canvas_root", Some("kernel"), json!({}));
        put(k, "manager", Some("canvas_root"), json!({"cabinet_role": "manager", "canvas_id": "canvas_root"}));
        put(k, "cabinet_a", Some("manager"), json!({"cabinet_role": "cabinet", "canvas": "canvas_cabinet_a"}));
        put(k, "canvas_cabinet_a", Some("canvas_root"), json!({}));
        put(k, "helper", Some("canvas_cabinet_a"), json!({"memory_store_id": "m"}));
        // an ordinary AI, which must not reach the cabinet's equipment (no god mode)
        put(k, "master", Some("kernel"), json!({"memory_store_id": "m"}));
        put(k, "m", Some("kernel"), json!({}));
        grant(&h, &["helper".to_string(), "canvas_cabinet_a".to_string()]).await.unwrap();
        let field = |id: &str, f: &str| -> Value {
            k.agents.get(&AgentId::from(id)).unwrap().value().record().meta.get(f).cloned().unwrap_or(Value::Null)
        };
        // the human calls the equipment and the canvas; the leg watches and emits on them
        assert_eq!(field("helper", "callable_by")[crate::env::HUMAN], json!(["*"]));
        assert_eq!(field("helper", "callable_by")[CANVAS_LEG], leg_verbs(&h, "helper"));
        assert_eq!(field("canvas_cabinet_a", "callable_by")[CANVAS_LEG], leg_verbs(&h, "canvas_cabinet_a"));
        assert_eq!(field(CANVAS_LEG, "can_call")["helper"], leg_verbs(&h, "helper"));
        assert_eq!(field(CANVAS_LEG, "can_call")["canvas_cabinet_a"], leg_verbs(&h, "canvas_cabinet_a"));
        assert_eq!(field(crate::env::HUMAN, "can_call")["helper"], json!(["*"]));
        // no other holder is granted the cabinet's equipment
        assert!(field("helper", "callable_by").get("master").is_none(), "{:?}", field("helper", "callable_by"));
        assert!(field("master", "can_call").get("helper").is_none(), "{:?}", field("master", "can_call"));
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_grant_and_a_concurrent_callable_by_edit_on_one_agent_both_survive() {
        let h = host_with_leg("grant-race").await;
        let k = &h.kernel;
        for round in 0..100 {
            let id = format!("helper{round}");
            put(k, &id, Some("kernel"), json!({}));
            let view = format!("viewer{round}");
            let (h2, k2) = (Host { kernel: std::sync::Arc::clone(k), cfg: crate::env::ClientCfg::here(), root: "kernel".to_string() }, std::sync::Arc::clone(k));
            let (id1, id2, v) = (id.clone(), id.clone(), view.clone());
            let a = tokio::spawn(async move { grant(&h2, &[id1]).await });
            let b = tokio::spawn(async move {
                k2.edit_callable_by(&AgentId::from(id2.as_str()), |cb| {
                    cb.insert(v, json!(["*"]));
                    true
                })
            });
            a.await.unwrap().unwrap();
            b.await.unwrap().unwrap();
            let cb = k.agents.get(&AgentId::from(id.as_str())).unwrap().record().meta.get("callable_by").cloned().unwrap_or(Value::Null);
            assert_eq!(cb.get(crate::env::HUMAN), Some(&json!(["*"])), "round {round}: {cb}");
            assert_eq!(cb.get(&view), Some(&json!(["*"])), "round {round}: {cb}");
        }
    }
}
