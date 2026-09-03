//! Agent lifecycle: create, delete (with depth-first cascade +
//! on_delete hooks + disk cleanup), update.
//!
//! `create_agent` is dispatched on a parent — the new agent becomes
//! its child under `<parent.root_path>/agents/<new_id>/`. Substrate
//! mints the id from the bundle name + a 6-hex token unless caller
//! supplies one.
//!
//! `delete_agent` is dispatched on the substrate (target id in
//! payload). It refuses if the agent carries `delete_lock: true`.
//! Otherwise it walks the subtree depth-first, calls each bundle's
//! `on_delete` hook BEFORE unregistering from `kernel.agents`, and
//! rmtrees the dir (unless the agent is ephemeral).

use crate::agent::{Agent, AgentId};
use crate::kernel::Kernel;
use crate::outcome::Item;
use crate::persistence;
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// Mint a fresh id from `handler_module` (`<bundle>.tools` →
/// `<bundle>_<hex6>`). Uses a thread-local PRNG seeded from std's
/// random source to keep things deterministic-free without adding
/// a `rand` dep at the kernel level.
pub fn mint_id(handler_module: &str) -> String {
    let bundle = handler_module
        .strip_suffix(".tools")
        .unwrap_or(handler_module);
    // Use the address of a stack allocation + current time as entropy.
    // Not cryptographic — only needs uniqueness within a workdir.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let mix = nanos ^ stack_ptr ^ std::process::id() as u64;
    let hex6 = format!("{:06x}", (mix as u32) & 0xff_ffff);
    format!("{bundle}_{hex6}")
}

/// Mint a stable 32-hex-char (lowercase) kernel instance guid.
///
/// Entropy = wall-clock nanos + pid + a stack address — NOT cryptographic,
/// only needs per-boot uniqueness. This is the kernel-side twin of
/// `fantastic-bridge`'s `mint_guid`; we can't import that here because the
/// bridge depends on the kernel, not the reverse. Called once at boot when the
/// root record carries no `guid`; an explicit/existing guid always wins.
pub fn mint_guid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let pid = std::process::id() as u64;
    let hi = (nanos as u64) ^ stack_ptr;
    let lo = ((nanos >> 64) as u64) ^ pid ^ stack_ptr.rotate_left(17);
    format!("{hi:016x}{lo:016x}")
}

/// Implementation of the `create_agent` system verb.
///
/// `payload` shape (matches the workdir wire format):
/// ```json
/// {"type":"create_agent","handler_module":"file_bridge.tools","id":"opt"}
/// ```
/// Any extra fields become the new agent's meta.
/// A grant map's SHAPE, judged at WRITE time — the gate only judges edges.
///
/// `{id|token: "*" | [verbs]}` — anything else is refused here, so a record can
/// never carry a grant the gate would have to guess on. Ported from python
/// (measured 2026-09-01, E21_plan: `update_agent` accepted `can_call:"*"`, and
/// the py gate crashed out of a web serve; the rust gate denies instead of
/// crashing, but a silently-dead grant is still the wrong answer — refuse loud).
pub(crate) fn bad_grant_shape(meta: &Map<String, Value>) -> Option<String> {
    for field in ["can_call", "callable_by"] {
        let Some(g) = meta.get(field) else { continue };
        let Some(map) = g.as_object() else {
            return Some(format!(
                "{field} must be a grant MAP {{\"<id|$token>\": \"*\" | \
                 [\"<verb>\", …]}} — to open wide, grant per key \
                 (e.g. {{\"$self\": \"*\", \"ai\": \"*\"}}), never a bare value"
            ));
        };
        for (k, v) in map {
            let ok = v.as_str() == Some("*")
                || v.as_array()
                    .is_some_and(|a| a.iter().all(|x| x.is_string()));
            if !ok {
                return Some(format!(
                    "{field}[{k:?}] must be \"*\" or a list of verb strings"
                ));
            }
        }
    }
    None
}

pub(crate) async fn create_from_payload(
    kernel: &Arc<Kernel>,
    root: &Arc<Agent>,
    payload: &Value,
) -> Value {
    let Some(hm) = payload.get("handler_module").and_then(Value::as_str) else {
        return crate::refusal::missing("create_agent", "handler_module");
    };

    // `parent_id` NAMES THE OWNER. It is a real, REQUIRED argument — resolved
    // like a send target (the `kernel` alias included), never defaulted.
    //
    // It used to be accepted and silently STRIPPED while the ADDRESSED agent
    // became the parent: `parent_id:"specialist"` produced `parent_id="core"`
    // with no error. The working mechanism was undiscoverable and the field
    // that looked like it expressed placement did nothing.
    let Some(pid) = payload.get("parent_id").and_then(Value::as_str) else {
        return crate::refusal::missing_hint(
            "create_agent",
            "parent_id",
            "name the OWNER of the new agent — your own id grows your own \
             subtree (no grant needed); the root id for top-level.",
        );
    };
    let parent: Arc<Agent> = if pid == "kernel" {
        Arc::clone(root)
    } else {
        match kernel.agents.get(&AgentId::from(pid)) {
            Some(e) => Arc::clone(&e),
            None => {
                return crate::refusal::unknown_agent(
                    pid,
                    kernel.agents.iter().map(|e| e.key().0.clone()).collect(),
                )
            }
        }
    };
    let parent = &parent;
    // We allow creation of agents whose bundle isn't yet registered —
    // record persists, agent is registered but won't dispatch domain
    // verbs until the bundle lands. Matches Python's behaviour. Only
    // the LOAD path skips unknown bundles (weak-load semantics).

    // A supplied id — even "" — is the caller's choice and is validated as one
    // by `guard_agent_id`; only an ABSENT id is minted (python parity).
    let id = match payload.get("id") {
        None | Some(Value::Null) => mint_id(hm),
        Some(v) => v.as_str().unwrap_or("").to_string(),
    };
    // An id BECOMES A DIRECTORY, so it must be one safe segment. Enforced at
    // the mint, so every consumer inherits it rather than each remembering.
    if let Some(r) = crate::refusal::guard_agent_id("create_agent", &id) {
        return r;
    }
    if kernel.agents.contains_key(&AgentId::from(id.as_str())) {
        let mut r = crate::refusal::refusal(
            crate::refusal::EXISTS,
            format!("agent {id:?} already exists"),
        );
        crate::refusal::with(&mut r, "id", json!(id));
        return r;
    }

    // Compose meta from the remaining payload fields.
    let mut meta: Map<String, Value> = Map::new();
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj {
            if matches!(k.as_str(), "type" | "id" | "handler_module" | "parent_id") {
                continue;
            }
            // Reserved keys name substrate wiring; a record may not carry them
            // (the loader refuses such a record on the next boot).
            if crate::agent::RESERVED_RECORD_KEYS.contains(&k.as_str()) {
                return crate::refusal::bad(
                    "create_agent",
                    k,
                    "absent — root_path/ctx/parent name substrate wiring the \
                     record may not carry",
                );
            }
            meta.insert(k.clone(), v.clone());
        }
    }

    // The structural authority baseline ({$self,$parent,$children} on both
    // `can_call` and `callable_by`) is stamped + merged uniformly in `Agent::new`
    // for EVERY agent — so it is NOT re-stamped here; an explicit grant in the
    // payload rides in `meta` and merges over the baseline at construction.

    // WRITE-TIME VALIDATION, same gate as `update_agent` (py-canonical: create
    // used to bypass it, so `create_agent ingress_rule=bogus` was ACCEPTED here
    // and REFUSED one update later — two answers for one field). Record is empty
    // at create: the merged view IS the patch.
    if let Some(why) = bad_grant_shape(&meta) {
        return crate::refusal::refusal(crate::refusal::INVALID_META, &why);
    }
    if let Some(bundle) = kernel.bundles.get(hm) {
        if let Some(why) = bundle.validate_meta(&meta, &Map::new()) {
            return crate::refusal::refusal(crate::refusal::INVALID_META, why);
        }
    }

    // In Disk mode the agent's `root_path` is also where bundles can
    // write sidecar files (chat history, schedules, etc.). In
    // InMemory mode the path is composed but never read — bundles
    // that try to write a sidecar there will surface the fs error
    // themselves.
    let root_path = parent.children_dir().join(&id);
    let new_agent = Agent::new(
        AgentId::from(id.as_str()),
        Some(hm.to_string()),
        Some(parent.id.clone()),
        meta,
        root_path,
        false,
    );
    // Wire into kernel + parent FIRST, then persist. Order matters now that
    // persistence routes THROUGH a discovered `file_bridge` provider (a child of
    // root): a freshly-created store must already be a registered child so it can
    // persist its OWN record through itself (find_store sees it). No-ops in
    // InMemory mode or when no provider is wired (RAM — see `persistence::persist`).
    let _rx = kernel.register(Arc::clone(&new_agent));
    parent
        .children
        .insert(new_agent.id.clone(), Arc::clone(&new_agent));

    // Persist + seed readme (the bundle ships its readme via `Bundle::readme()`).
    // Both go through the provider's stream verbs; merge-not-overwrite.
    if let Err(e) = persistence::persist(kernel, &new_agent).await {
        return json!({ "error": format!("persist: {e}") });
    }
    if let Some(bundle) = kernel.bundles.get(hm) {
        if let Some(readme) = bundle.readme() {
            let _ = persistence::seed_readme(kernel, &new_agent, readme).await;
        }
    }

    let event = json!({
        "type": "created",
        "id": new_agent.id.0,
        "parent_id": parent.id.0,
        "handler_module": hm,
    });
    kernel.publish_state(&event);

    let rec_value = serde_json::to_value(new_agent.record()).unwrap_or(Value::Null);
    // Fire the new agent's `boot` hook (Python does the same — bundles
    // may auto-spawn paired agents here). Failures
    // are logged but don't abort the create — matches Python.
    //
    // Wrap in Box::pin because boot may call create_agent recursively
    // (a boot hook may create a child agent); the resulting
    // async future is recursive and Rust requires explicit indirection.
    let new_id = new_agent.id.clone();
    let kernel_for_boot = Arc::clone(kernel);
    // Operator authority: booting the child is a side effect of the already-
    // authorized create_agent (the kernel initializing what it was told to make),
    // not the parent reaching out — so it never runs against the parent's grant.
    let boot_reply = Box::pin(crate::send::with_operator(async move {
        kernel_for_boot
            .send(vec![Item::new(new_id.clone(), json!({"type": "boot"}))])
            .await
            .remove(0)
            .result
    }))
    .await;
    let new_id = new_agent.id.clone();
    if let Some(err) = boot_reply.get("error").and_then(Value::as_str) {
        tracing::warn!(agent = %new_id, error = %err, "boot after create_agent errored");
    }
    // Emit `agent_created` lifecycle event on the parent's inbox so
    // watchers (canvas frame chrome) refresh without polling. Mirrors
    // Python's `await self.emit(self.id, {type:"agent_created", ...})`.
    kernel
        .emit(
            &parent.id,
            json!({
                "type": "agent_created",
                "id": new_id.0,
                "agent": rec_value.clone(),
            }),
        )
        .await;
    rec_value
}

/// DFS through `target`'s subtree looking for the first agent (including
/// `target` itself) whose `delete_lock` meta is true. Returns the locked
/// agent's id, or `None` if the whole subtree is clear. Mirrors Python's
/// `Agent._find_locked_descendant`.
fn find_locked_descendant(kernel: &Kernel, target: &Arc<Agent>) -> Option<AgentId> {
    if target.is_delete_locked() {
        return Some(target.id.clone());
    }
    for cid in target.child_ids() {
        if let Some(child) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) {
            if let Some(blocker) = find_locked_descendant(kernel, &child) {
                return Some(blocker);
            }
        }
    }
    None
}

/// Implementation of the `delete_agent` system verb.
pub(crate) async fn delete_from_payload(
    kernel: &Arc<Kernel>,
    caller: &Arc<Agent>,
    payload: &Value,
) -> Value {
    let Some(id_str) = payload.get("id").and_then(Value::as_str) else {
        return crate::refusal::missing("delete_agent", "id");
    };
    let id = AgentId::from(id_str);
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        // THE ONE REFUSAL SHAPE, py-canonical: a code to branch on, the LIVE id
        // index to self-correct from, and `deleted:false` so a caller reading
        // that field is not told a typo succeeded. The bare `{error}` this used
        // to answer is the measured 34-retry case — a fact with nothing to act
        // on.
        let mut r = crate::refusal::unknown_agent(
            id_str,
            kernel.agents.iter().map(|e| e.key().to_string()).collect(),
        );
        crate::refusal::with(&mut r, "deleted", json!(false));
        return r;
    };
    // THE ROOT IS NOT DELETABLE. It owns the persistence surface and answers
    // every kernel verb, so cascading it empties the kernel and leaves a process
    // with no control surface at all — `{"deleted": true}` and an empty tree
    // (measured 2026-09-02).
    if target.parent_id.is_none() {
        let mut r = crate::refusal::bad(
            "delete_agent",
            "id",
            "not the ROOT — it owns persistence and answers the kernel verbs; \
             deleting it would leave no kernel",
        );
        crate::refusal::with(&mut r, "id", json!(id_str));
        crate::refusal::with(&mut r, "deleted", json!(false));
        return r;
    }
    // Refuse the cascade if ANY agent in the subtree (including the
    // target itself) carries `delete_lock=true`. Walking the subtree
    // matches Python's `_find_locked_descendant` — without this, the
    // caller learns "delete refused" but not which descendant blocks
    // them. Returns the first locked id so the caller can clear it
    // (`update_agent id=<blocker> delete_lock=null`) before retrying.
    if let Some(blocker) = find_locked_descendant(kernel, &target) {
        // THE ONE REFUSAL SHAPE (#27). This carried a bare `locked: true` flag
        // and no `reason` until 2026-08-27, when the shared agentic corpus
        // caught it — in python, rust and swift at once, because the flag #27
        // declared dropped was never actually removed anywhere.
        let mut r = crate::refusal::refusal_hint(
            crate::refusal::LOCKED,
            format!(
                "delete_agent: {} blocked by delete_lock on descendant {}",
                id.0, blocker.0,
            ),
            "clear the lock with update_agent, then delete again",
        );
        crate::refusal::with(&mut r, "id", json!(id.0));
        crate::refusal::with(&mut r, "blocked_by", json!(blocker.0));
        return r;
    }
    cascade_delete(kernel, &target).await;
    // Emit `agent_deleted` on the caller's inbox so watchers refresh
    // (canvas frame chrome, etc.). Mirrors Python's
    // `await self.emit(self.id, {type:"agent_deleted", id})`.
    kernel
        .emit(&caller.id, json!({"type": "agent_deleted", "id": id.0}))
        .await;
    json!({ "deleted": true, "id": id.0 })
}

/// Depth-first delete: each leaf's `on_delete` hook fires before its
/// parent's; the agent unregisters from `kernel.agents` and parent's
/// `children` map only AFTER its hook completes successfully.
pub async fn cascade_delete(kernel: &Arc<Kernel>, target: &Arc<Agent>) {
    // Snapshot children first (since we mutate during iteration).
    let child_ids = target.child_ids();
    for cid in child_ids {
        if let Some(child) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) {
            Box::pin(cascade_delete(kernel, &child)).await;
        }
    }
    // on_delete hook.
    if let Some(hm) = target.handler_module.as_deref() {
        if let Some(bundle) = kernel.bundles.get(hm) {
            if let Err(e) = bundle.on_delete(&target.id, kernel).await {
                tracing::warn!(
                    agent = %target.id,
                    handler_module = %hm,
                    error = %e,
                    "on_delete hook errored; continuing cascade",
                );
            }
        }
    }
    // Detach from parent's children map.
    if let Some(parent_id) = target.parent_id.as_ref() {
        if let Some(parent) = kernel.agents.get(parent_id).map(|e| Arc::clone(&e)) {
            parent.children.remove(&target.id);
        }
    }
    // Unregister + drop inbox.
    kernel.unregister(&target.id);
    // Disk cleanup THROUGH the discovered provider (its recursive `delete`
    // verb) — the substrate owns no `fs` surface here. No-op without a wired
    // provider or in InMemory mode (the dir, if any, was never ours to remove).
    let _ = persistence::forget(kernel, target).await;
    let event = json!({ "type": "removed", "id": target.id.0 });
    kernel.publish_state(&event);
}

#[cfg(test)]
mod tests;
