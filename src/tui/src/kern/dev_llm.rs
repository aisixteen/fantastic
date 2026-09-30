//! CRITICAL TODO(dev-llm): dev-only hardwired LLM endpoint — remove before
//! release; connector onboarding replaces it. Never commit `dev.local.yaml`.
//!
//! Compiled only under `--features dev-llm`. It gives the shell ONE AI agent,
//! `ai`, wired to the dev endpoint that `build.rs` baked in from
//! `dev.local.yaml`. The key never touches a file or the binary: it's read from
//! the keychain at startup, before any runtime thread exists, and set as the one
//! variable the anthropic backend reads at call time.
//!
//! `ai` is created by code the first time a dev build boots a store (like the
//! `mcp` leg), never by the blueprint. Its only grant is the one the human
//! needs: `user` may call it.

#[cfg(not(debug_assertions))]
compile_error!(
    "CRITICAL TODO(dev-llm): the dev LLM endpoint is hardwired into this build — `dev-llm` is never allowed in a release build"
);

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// CRITICAL TODO(dev-llm): the baked record, straight from `dev.local.yaml`.
const CFG: &str = env!("FANTASTIC_DEV_LLM_JSON");
/// The keychain entry the old CLI already uses for provider keys.
use crate::kern::keys::KEYCHAIN_SERVICE;
pub const KEYCHAIN_ACCOUNT: &str = "anthropic";
/// The AI agent this build adds.
pub const AI_ID: &str = "ai";

/// CRITICAL TODO(dev-llm): read the key from the keychain into the process env.
/// Must run before the tokio runtime is built: setting the environment once
/// threads exist is a race.
pub fn load_key() -> Result<(), String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)
        .map_err(|e| format!("keychain: {e}"))?;
    let key = entry.get_password().map_err(|e| match e {
        keyring::Error::NoEntry => format!(
            "dev-llm: no key in the keychain at {KEYCHAIN_SERVICE} / {KEYCHAIN_ACCOUNT}. Add it once: security add-generic-password -s {KEYCHAIN_SERVICE} -a {KEYCHAIN_ACCOUNT} -w"
        ),
        // Anything else is NOT "missing" — say what the keychain said.
        other => format!("dev-llm: could not read {KEYCHAIN_SERVICE} / {KEYCHAIN_ACCOUNT} from the keychain: {other}"),
    })?;
    std::env::set_var("ANTHROPIC_API_KEY", key);
    Ok(())
}

/// The `ai` record's fields, from the baked config. `num_ctx` mirrors
/// `context_window`, as the e2e harness seeds it.
fn fields() -> Map<String, Value> {
    let mut m: Map<String, Value> = serde_json::from_str(CFG).expect("build.rs wrote valid JSON");
    if let Some(w) = m.get("context_window").cloned() {
        m.insert("num_ctx".into(), w);
    }
    m
}

/// CRITICAL TODO(dev-llm): create `ai` once, under `set_of_agents`: `user` may
/// call it, and it may call EVERY agent that exists at that moment (both sides,
/// privileged verbs named on the root). Idempotent by id: an existing `ai` is
/// left as it is. Grants are MERGED, never replaced.
pub async fn ensure_ai(host: &Host) -> Result<(), String> {
    if host.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(AI_ID)) {
        return Ok(());
    }
    // Its conversation lives in the kernel's memory store, both sides granted
    // (the loop below grants every agent, the store included).
    let store = host.admit_to_memory(AI_ID).await.map_err(|e| format!("dev-llm: {e}"))?;
    let mut create = json!({
        "type": "create_agent",
        "parent_id": "set_of_agents",
        "id": AI_ID,
        "handler_module": fantastic_anthropic_backend::HANDLER_MODULE,
        "callable_by": {"user": "*"},
        "memory_store_id": store,
        "can_call": {store.as_str(): "*"},
    });
    for (k, v) in fields() {
        create[k] = v;
    }
    let r = host.operator("kernel", create).await;
    if let Some(e) = r.result.get("error").and_then(Value::as_str) {
        return Err(format!("dev-llm: could not create `{AI_ID}`: {e}"));
    }
    // The human's authority must name the new agent, or `user -> ai` is refused.
    merge(host, &host.cfg.user.clone(), "can_call", AI_ID, json!("*")).await?;
    // TODO: resolve default ai grants UXUI — for now the dev `ai` gets all.
    // THE FIRST, DEFAULT AI GETS ALL (owner decision): every agent that exists
    // now, BOTH sides of the gate, and on the root every privileged verb by name
    // — a wildcard never carries those. Agents added later are NOT opened to it
    // automatically, and every later ai agent (the set-up page) stays granular.
    let root = host.root.clone();
    let mut reach = grants(host, AI_ID, "can_call");
    for id in host.kernel.agent_ids() {
        let id = id.0;
        if id == AI_ID {
            continue;
        }
        let verbs = if id == root {
            let mut v = vec![json!("*")];
            v.extend(fantastic_kernel::authz::PRIVILEGED_VERBS.iter().map(|p| json!(p)));
            Value::Array(v)
        } else {
            json!("*")
        };
        reach.insert(id.clone(), verbs);
        merge(host, &id, "callable_by", AI_ID, json!("*")).await?;
    }
    let r = host
        .operator("kernel", json!({"type": "update_agent", "id": AI_ID, "can_call": reach}))
        .await;
    if let Some(e) = r.result.get("error").and_then(Value::as_str) {
        return Err(format!("dev-llm: could not grant `{AI_ID}` its reach: {e}"));
    }
    let _ = host.operator(AI_ID, json!({"type": "boot"})).await;
    Ok(())
}

/// One agent's grant map, as its record holds it now.
fn grants(host: &Host, id: &str, field: &str) -> Map<String, Value> {
    host.kernel
        .agents
        .get(&fantastic_kernel::AgentId::from(id))
        .and_then(|a| a.value().record().meta.get(field).and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

/// `id.field[add] = verbs`, merged into what is there.
async fn merge(host: &Host, id: &str, field: &str, add: &str, verbs: Value) -> Result<(), String> {
    let mut g = grants(host, id, field);
    g.insert(add.into(), verbs);
    let r = host.operator("kernel", json!({"type": "update_agent", "id": id, field: g})).await;
    match r.result.get("error").and_then(Value::as_str) {
        Some(e) => Err(format!("dev-llm: could not grant {id}.{field} += {add}: {e}")),
        None => Ok(()),
    }
}
