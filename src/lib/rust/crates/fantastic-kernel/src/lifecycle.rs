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
use serde_json::{json, Map, Value};
use std::sync::Arc;

/// `n` bytes from the OS CSPRNG — the source class python's `secrets` module
/// and `uuid.uuid4()` draw from (`os.urandom`). A failure is FATAL: there is
/// no second source, and a clock/pid mixer is not random at all.
pub(crate) fn os_random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    fill_random(&mut buf);
    buf
}

/// Fill `buf` from the OS CSPRNG — the ONE randomness call of every crate,
/// the OS's own call ([`crate::sys`]). A failure is FATAL: there is no second
/// source.
pub fn fill_random(buf: &mut [u8]) {
    crate::sys::fill_random(buf);
}

/// Lowercase hex, python's `bytes.hex()` spelling.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// python `secrets.token_hex(n)` — `n` CSPRNG bytes as lowercase hex.
#[must_use]
pub fn token_hex(n: usize) -> String {
    hex(&os_random(n))
}

/// Mint a fresh id from `handler_module` (`<bundle>.tools` →
/// `<bundle>_<hex6>`) — python `_agent.py`'s
/// `f"{bundle}_{secrets.token_hex(3)}"`, where `bundle` is the SECOND-LAST
/// dotted segment (`handler_module.split(".")[-2]`), whatever the last one is:
/// `a.b.tools` → `b_…`, `pkg.ts` → `pkg_…`. A dotless module is used whole.
pub fn mint_id(handler_module: &str) -> String {
    let parts: Vec<&str> = handler_module.split('.').collect();
    let bundle = if parts.len() > 1 {
        parts[parts.len() - 2]
    } else {
        handler_module
    };
    format!("{bundle}_{}", hex(&os_random(3)))
}

/// Mint a stable 32-hex-char (lowercase) kernel instance guid — python
/// `kernel_state.tools`' `uuid.uuid4().hex`: 16 OS-random bytes carrying
/// uuid4's version nibble and the RFC-4122 variant bits. Called once at boot
/// when the root record carries no `guid`; an explicit/existing guid wins.
pub fn mint_guid() -> String {
    let mut b = os_random(16);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    hex(&b)
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
/// `{key: "*" | [verbs]}`, a key being one of [`crate::authz::legal_keys`] —
/// anything else is refused here, so a record can never carry a grant the gate
/// would have to guess on. Ported from python: a silently-dead grant is the
/// wrong answer — refuse loud.
pub(crate) fn bad_grant_shape(meta: &Map<String, Value>) -> Option<String> {
    for field in ["can_call", "callable_by"] {
        let Some(g) = meta.get(field) else { continue };
        let Some(map) = g.as_object() else {
            // NAME WHAT ARRIVED — python's `got {type(g).__name__}.`; without it
            // the refusal says a map is wanted but never what was sent.
            return Some(format!(
                "{field} must be a grant MAP {{\"<id|$token>\": \"*\" | \
                 [\"<verb>\", …]}} — got {}. To open wide, grant per key \
                 (e.g. {{\"$self\": \"*\", \"ai\": \"*\"}}), never a bare value",
                fantastic_bundle::py_type_name(g)
            ));
        };
        for (k, v) in map {
            let kr = fantastic_bundle::py_repr(k);
            if !crate::authz::is_grant_key(field, k) {
                return Some(format!(
                    "{field}[{kr}] is not a grant key — legal keys: {}",
                    crate::authz::legal_keys(field)
                ));
            }
            let ok = v.as_str() == Some("*")
                || v.as_array()
                    .is_some_and(|a| a.iter().all(|x| x.is_string()));
            if !ok {
                return Some(format!(
                    "{field}[{kr}] must be \"*\" or a list of verb strings"
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
    // python: `if not isinstance(handler_module, str) or not handler_module:
    // return missing(...)` — an EMPTY string is falsy and refused the same as
    // an absent one, not just a missing/non-string value.
    let Some(hm) = payload
        .get("handler_module")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return crate::refusal::missing("create_agent", "handler_module");
    };

    // `parent_id` NAMES THE OWNER. It is a real, REQUIRED argument — resolved
    // like a send target (the reserved `kernel` address included), never defaulted.
    //
    // Same falsy-string rule as `handler_module` — an empty `parent_id` is
    // refused as absent, not resolved as an (unknown) empty-string agent id.
    let Some(pid) = payload
        .get("parent_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return crate::refusal::missing_hint(
            "create_agent",
            "parent_id",
            "name the agent that will OWN the new one — your own id for a \
             tool you keep, or the root ('kernel_state') for a top-level agent",
        );
    };
    // Resolved the same way `send` does: `kernel` special-cased to the root,
    // then a literal id.
    let parent: Arc<Agent> = if pid == "kernel" {
        Arc::clone(root)
    } else {
        match kernel.resolve_target(pid) {
            Some(e) => e,
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

    // RESERVED KEYS FIRST, in SORTED order — python walks `sorted(RESERVED_RECORD_KEYS)`
    // and checks each against the payload, so the refusal names the
    // alphabetically-first reserved key present, never whichever the payload
    // happened to list first.
    let mut reserved: Vec<&str> = crate::agent::RESERVED_RECORD_KEYS.to_vec();
    reserved.sort_unstable();
    for k in reserved {
        if payload.get(k).is_some() {
            return crate::refusal::bad(
                "create_agent",
                k,
                "absent — root_path/ctx/parent name substrate wiring the \
                 record may not carry",
            );
        }
    }
    // `children_dir` BECOMES A DIRECTORY (`<agent>/<children_dir>/<child>`), so
    // it is one safe segment, exactly as an id is. A non-string is judged as ""
    // — python refuses it as "a non-empty string" too.
    if let Some(cd) = payload.get("children_dir") {
        if let Some(r) = crate::refusal::guard_agent_id_as(
            "create_agent",
            "children_dir",
            cd.as_str().unwrap_or(""),
        ) {
            return r;
        }
    }

    // Compose meta from the remaining payload fields. No per-field check here:
    // an unknown handler_module is not refused (see above), and reserved keys
    // were already ruled out.
    let mut meta: Map<String, Value> = Map::new();
    if let Some(obj) = payload.as_object() {
        for (k, v) in obj {
            if matches!(k.as_str(), "type" | "id" | "handler_module" | "parent_id") {
                continue;
            }
            meta.insert(k.clone(), v.clone());
        }
    }

    // The structural authority baseline ({$self,$parent,$subtree} on both
    // `can_call` and `callable_by`) is stamped + merged uniformly in `Agent::new`
    // for EVERY agent — so it is NOT re-stamped here; an explicit grant in the
    // payload rides in `meta` and merges over the baseline at construction.

    // WRITE-TIME VALIDATION, same gate as `update_agent` (py-canonical), so a
    // field has one answer at create and at update. Record is empty
    // at create: the merged view IS the patch.
    if let Some(why) = bad_grant_shape(&meta) {
        return crate::refusal::refusal(crate::refusal::INVALID_META, &why);
    }
    if let Some(bundle) = kernel.bundles.get(hm) {
        if let Some(why) = bundle.validate_meta(&meta, &Map::new()) {
            return crate::refusal::refusal(crate::refusal::INVALID_META, why);
        }
    }

    // A supplied id — even "" — is the caller's choice and is validated as one
    // by `guard_agent_id`; only an ABSENT id is minted (python parity). Minted
    // AFTER every refusal above (python's `parent.create()` runs last, inside
    // `rec = parent.create(...)`).
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
            format!("agent {} exists", fantastic_bundle::py_repr(&id)),
        );
        crate::refusal::with(
            &mut r,
            "hint",
            json!("pick another id, or update the existing agent"),
        );
        crate::refusal::with(&mut r, "id", json!(id));
        return r;
    }

    // In Disk mode the agent's `root_path` is also where bundles can
    // write sidecar files (chat history, schedules, etc.). In
    // InMemory mode the path is composed but never read — bundles
    // that try to write a sidecar there will surface the fs error
    // themselves.
    let root_path = kernel.children_dir_of(parent).join(&id);
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
    kernel.register(Arc::clone(&new_agent));
    parent.add_child(Arc::clone(&new_agent));

    // NO INLINE PERSIST. The record reaches disk through the ROOT LOADER's
    // `persist_writer`, which consumes the `added` event published below and
    // debounces it — python's `_FlushLoop`, and the only path that also seeds
    // the bundle's readme/docs. A kernel with no loader booted (or no store
    // wired) keeps the record in RAM, which is the auto-flush law.

    // LIFECYCLE SHAPE, python's: `{agent_id, kind, …}` like every other state
    // event, so one subscriber reads one shape. `created` with a `handler_module`
    // was a rust-only kind nothing else answers; `name` is what a view renders.
    let event = json!({
        "agent_id": new_agent.id.0,
        "kind": "added",
        "name": new_agent.name_or_id(),
        "parent_id": parent.id.0,
    });
    kernel.publish_state(&event);

    let rec_value = crate::jsonsafe::record_value(&new_agent.record());
    // BOOT — kernel bookkeeping: the kernel initializing the child it was told to
    // make, not the parent reaching out. The child's handler runs directly
    // ([`Kernel::bookkeeping`]), as the child, with no origin — never through a
    // gated send, never against the parent's own grant. Failures are logged but
    // don't abort the create — matches Python.
    //
    // Wrap in Box::pin because boot may call create_agent recursively (a boot
    // hook may create a child agent); the resulting async future is recursive
    // and Rust requires explicit indirection.
    let boot = Box::pin(kernel.bookkeeping(&new_agent, json!({"type": "boot"}), None)).await;
    let new_id = new_agent.id.clone();
    // BRANCH ON THE OUTCOME STATUS, not on the reply shape. A handler that
    // RAISED settles `failed` and is the one case python prints — on stdout,
    // with the result's `str()`. A boot that merely answered `{error:…}`
    // (`errored`) or completed prints nothing — no WARN for every refusal.
    if boot.status == crate::outcome::Status::Failed {
        println!(
            "  [create_agent] {} boot failed: {}",
            new_id.0,
            fantastic_bundle::py_str(&boot.result)
        );
    }
    // Lifecycle event on the PARENT's inbox, not the root's: whoever watches an
    // agent for new children still sees them arrive. Kernel bookkeeping too —
    // put on the parent's inbox and mirrored to its watchers directly (the
    // fanout), never a gated emit. Python
    // `parent._fanout(..., kind="emit")`.
    kernel
        .fanout(
            parent,
            "emit",
            &crate::inbox::InboxItem::text(crate::jsonsafe::obj([
                ("type", Value::from("agent_created")),
                ("id", Value::from(new_id.0.clone())),
                ("agent", rec_value.clone()),
            ])),
            crate::send::current_sender().as_ref(),
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
    let Some(id_str) = payload
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return crate::refusal::missing("delete_agent", "id");
    };
    let id = AgentId::from(id_str);
    let Some(target) = kernel.agents.get(&id).map(|e| Arc::clone(&e)) else {
        // THE ONE REFUSAL SHAPE, py-canonical: a code to branch on, the LIVE id
        // index to self-correct from, and `deleted:false` so a caller reading
        // that field is not told a typo succeeded. A bare `{error}` is a fact
        // with nothing to act on.
        let mut r = crate::refusal::unknown_agent(
            id_str,
            kernel.agents.iter().map(|e| e.key().to_string()).collect(),
        );
        crate::refusal::with(&mut r, "deleted", json!(false));
        return r;
    };
    // THE ROOT IS NOT DELETABLE. It owns the persistence surface and answers
    // every kernel verb, so cascading it empties the kernel and leaves a process
    // with no control surface at all — `{"deleted": true}` and an empty tree.
    // PYTHON'S PROSE, verbatim (`<id>` raw, no repr
    // quoting) — key order `error, reason, arg, id, deleted` (`arg`/`id`/
    // `deleted` all ride python's `**extra`, in the order the call names them).
    if target.parent_id.is_none() {
        let mut r = crate::refusal::refusal(
            crate::refusal::BAD_ARG,
            format!(
                "delete_agent: {id_str} is the ROOT — it owns persistence and \
                 answers the kernel verbs; deleting it would leave no kernel",
            ),
        );
        crate::refusal::with(&mut r, "arg", json!("id"));
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
        // THE ONE REFUSAL SHAPE, never a bare `locked: true` flag without a
        // `reason`. PYTHON'S PROSE, verbatim (both ids `py_repr`-quoted); key
        // order `error, reason, hint, blocked_by, id`.
        let mut r = crate::refusal::refusal_hint(
            crate::refusal::LOCKED,
            format!(
                "delete_agent: {} blocked by delete_lock on {}; clear it via \
                 update_agent before deleting",
                fantastic_bundle::py_repr(&id.0),
                fantastic_bundle::py_repr(&blocker.0),
            ),
            "clear the lock with update_agent, then delete again",
        );
        crate::refusal::with(&mut r, "blocked_by", json!(blocker.0));
        crate::refusal::with(&mut r, "id", json!(id.0));
        return r;
    }
    let mut gone = Vec::new();
    subtree_ids(kernel, &target, &mut gone);
    cascade_delete(kernel, &target).await;
    prune_grants(kernel, &gone);
    // Emit `agent_deleted` on the caller's inbox so watchers refresh
    // (canvas frame chrome, etc.). Mirrors Python's
    // `await self.emit(self.id, {type:"agent_deleted", id})`.
    kernel
        .emit(&caller.id, json!({"type": "agent_deleted", "id": id.0}))
        .await;
    json!({ "deleted": true, "id": id.0 })
}

fn subtree_ids(kernel: &Arc<Kernel>, agent: &Arc<Agent>, out: &mut Vec<AgentId>) {
    out.push(agent.id.clone());
    for cid in agent.child_ids() {
        if let Some(c) = kernel.agents.get(&cid).map(|e| Arc::clone(&e)) {
            subtree_ids(kernel, &c, out);
        }
    }
}

/// A deleted id leaves every grant that named it — python `_prune_grants`. Grant
/// keys are literal ids, so a stale one would hand its edge to whoever
/// re-creates that id. Each changed record is re-persisted by
/// its `updated` event ([`Kernel::update`]).
fn prune_grants(kernel: &Arc<Kernel>, gone: &[AgentId]) {
    let agents: Vec<Arc<Agent>> = kernel
        .agents
        .iter()
        .map(|e| Arc::clone(e.value()))
        .collect();
    for a in agents {
        let rec = a.record();
        let mut patch = Map::new();
        for field in ["can_call", "callable_by"] {
            let Some(Value::Object(grants)) = rec.meta.get(field) else {
                continue;
            };
            if gone.iter().any(|g| grants.contains_key(&g.0)) {
                let kept: Map<String, Value> = grants
                    .iter()
                    .filter(|(k, _)| !gone.iter().any(|g| &g.0 == *k))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                patch.insert(field.to_string(), Value::Object(kept));
            }
        }
        if !patch.is_empty() {
            kernel.update(&a.id, patch);
        }
    }
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
    // on_delete hook. Python's exact stdout line (`_agent.py:2835`,
    // `print(f"  [cascade] {self.id} bundle on_delete raised: {e}")`) — a
    // hook that raises is logged and the cascade still continues; delete is
    // not refusable. The hook is the agent's own code, so the kernel runs it AS
    // THE AGENT (its own id is the sender), whoever asked for the delete — like
    // `on_cancel`.
    if let Some(hm) = target.handler_module.as_deref() {
        if let Some(bundle) = kernel.bundles.get(hm) {
            let hook = bundle.on_delete(&target.id, kernel);
            if let Err(e) = crate::send::as_agent(&target.id, hook).await {
                println!("  [cascade] {} bundle on_delete raised: {e}", target.id.0);
            }
        }
    }
    // Detach from parent's children map.
    if let Some(parent_id) = target.parent_id.as_ref() {
        if let Some(parent) = kernel.agents.get(parent_id).map(|e| Arc::clone(&e)) {
            parent.remove_child(&target.id);
        }
    }
    // Unregister + drop inbox.
    kernel.unregister(&target.id);
    // Disk cleanup THROUGH the discovered provider (its recursive `delete`
    // verb) — the substrate owns no `fs` surface here. No-op without a wired
    // provider or in InMemory mode (the dir, if any, was never ours to remove).
    // NO INLINE FORGET: the `removed` event below is what the root loader's
    // `persist_writer` turns into the provider's recursive `delete`.
    let event = json!({
        "agent_id": target.id.0,
        "kind": "removed",
        "name": target.name_or_id(),
    });
    kernel.publish_state(&event);
}

#[cfg(test)]
mod tests;
