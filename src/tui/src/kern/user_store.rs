//! THE USER STORE's agent (`user_store`, owner 2026-10-12): the keys, set-ups
//! and stored fantastic locations kept in the user's home
//! (`fantastic_user_home_store::user_store`), with its own html view — placed
//! from the canvas's double-click (`user store`). Made under `user` on every
//! boot, its view re-set, so a new build's view replaces the old one.

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The agent's id.
pub const ID: &str = "user_store";

/// The verbs its view calls.
const VERBS: [&str; 5] = ["list", "store_here", "unstore", "unstore_others", "reflect"];

const VIEW: &str = include_str!("user_store_view.html");

/// Make `user_store` exist under `user`, its view and the human's grant re-set.
pub async fn ensure(host: &Host) -> Result<(), String> {
    let user = host.cfg.user.clone();
    let view = json!({"kind": "html", "bytes": VIEW, "calls": VERBS});
    let grant = json!({ user.as_str(): VERBS });
    let payload = if host.kernel.agents.contains_key(&fantastic_kernel::AgentId::from(ID)) {
        json!({"type": "update_agent", "id": ID, "view": view, "callable_by": grant, "display_name": "user store"})
    } else {
        json!({"type": "create_agent", "parent_id": user.as_str(), "id": ID, "handler_module": fantastic_user_home_store::user_store::HANDLER_MODULE,
               "view": view, "callable_by": grant, "display_name": "user store"})
    };
    let r = host.act("kernel", payload).await.result;
    match r.get("error").and_then(Value::as_str) {
        Some(e) => Err(format!("could not make `{ID}`: {e}")),
        None => Ok(()),
    }
}
