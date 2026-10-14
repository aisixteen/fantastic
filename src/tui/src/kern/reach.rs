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


/// The HOLDERS of the reach (strictly single-user dev tool, owner 2026-10-04):
/// each present holding leg, `user`, every AI agent (a non-root agent naming a
/// memory store), and the host-side helpers that act for the human —
/// `canvas_root` and every canvas set-up agent.
fn holders(h: &Host) -> Vec<String> {
    let root = h.kernel.root().map(|r| r.id.0.clone());
    let helper = [crate::kern::backend::HANDLER_MODULE, crate::kern::setup_agent::HANDLER_MODULE];
    let mut v: Vec<String> = HOLDING_LEGS.into_iter().filter(|l| exists(h, l)).map(String::from).collect();
    if exists(h, crate::env::HUMAN) {
        v.push(crate::env::HUMAN.to_string());
    }
    v.extend(
        h.kernel
            .agents
            .iter()
            .filter(|e| Some(&e.key().0) != root.as_ref())
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
/// [`leg_verbs`]. One write per changed record.
pub async fn grant(h: &Host, ids: &[String]) -> Result<(), String> {
    let all = json!(verbs());
    let holders = holders(h);
    let leg = exists(h, CANVAS_LEG);
    for id in ids {
        if !exists(h, id) {
            continue;
        }
        let mut cb = field(h, id, "callable_by");
        let mut changed = false;
        let narrowed = NARROWED.contains(&id.as_str());
        for holder in holders.iter().filter(|x| *x != id && !narrowed) {
            if cb.get(holder) != Some(&all) {
                cb.insert(holder.clone(), all.clone());
                changed = true;
            }
        }
        if leg && id != CANVAS_LEG {
            // MERGED into what the record names (`user` admits the leg's calls)
            let lv = union(cb.get(CANVAS_LEG), &leg_verbs(h, id));
            if cb.get(CANVAS_LEG) != Some(&lv) {
                cb.insert(CANVAS_LEG.to_string(), lv);
                changed = true;
            }
        }
        if changed {
            let r = h.operator("kernel", json!({"type": "update_agent", "id": id, "callable_by": cb})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("reach: `{id}`.callable_by: {e}"));
            }
        }
    }
    let mut writers: Vec<(String, bool)> = holders.into_iter().map(|x| (x, true)).collect();
    if leg {
        writers.push((CANVAS_LEG.to_string(), false));
    }
    for (holder, full) in writers {
        let mut cc = field(h, &holder, "can_call");
        let mut changed = false;
        for id in ids.iter().map(String::as_str).chain(["kernel"]) {
            if id == holder || (id != "kernel" && !exists(h, id)) || (full && NARROWED.contains(&id) && holder != crate::env::HUMAN) {
                continue;
            }
            // the leg's own: MERGED into what it holds (its calls to `user`)
            let want = if full { all.clone() } else { union(cc.get(id), &leg_verbs(h, id)) };
            if cc.get(id) != Some(&want) {
                cc.insert(id.to_string(), want);
                changed = true;
            }
        }
        if changed {
            let r = h.operator("kernel", json!({"type": "update_agent", "id": holder, "can_call": cc})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("reach: `{holder}`.can_call: {e}"));
            }
        }
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
        let mut cb = field(h, holder, "callable_by");
        let mut changed = false;
        for x in pairs(holder) {
            let want = union(cb.get(&x), &send);
            if cb.get(&x) != Some(&want) {
                cb.insert(x, want);
                changed = true;
            }
        }
        if changed {
            let r = h.operator("kernel", json!({"type": "update_agent", "id": holder, "callable_by": cb})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("reach: `{holder}`.callable_by (the way back): {e}"));
            }
        }
    }
    // the senders' side: whom they may send to
    for x in &everyone {
        let mut cc = field(h, x, "can_call");
        let mut changed = false;
        for holder in holders.iter().filter(|hd| hd.as_str() != x && (touched.contains(hd.as_str()) || touched.contains(x.as_str()))) {
            let want = union(cc.get(holder), &send);
            if cc.get(holder) != Some(&want) {
                cc.insert(holder.clone(), want);
                changed = true;
            }
        }
        if changed {
            let r = h.operator("kernel", json!({"type": "update_agent", "id": x, "can_call": cc})).await.result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(format!("reach: `{x}`.can_call (the way back): {e}"));
            }
        }
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
        let mut cc = field(h, holder, "can_call");
        if cc.get(&key) == Some(&json!(["*"])) {
            continue;
        }
        cc.insert(key.clone(), json!(["*"]));
        let r = h.operator("kernel", json!({"type": "update_agent", "id": holder, "can_call": cc})).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("reach: `{holder}` → `{key}`: {e}"));
        }
    }
    Ok(())
}
