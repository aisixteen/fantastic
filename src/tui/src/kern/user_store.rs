//! THE USER'S HOME (`home`, the blueprint's record; owner 2026-10-12): the keys,
//! set-ups and stored fantastic locations kept in the user's home
//! (`fantastic_user_home_store::user_store`). Its html view is its `reflect`'s.

use serde_json::{json, Value};

use crate::kern::host::Host;

/// The agent's id.
pub const ID: &str = crate::kern::saved::HOME_ID;

/// The canvas this run serves, told to `home` — written into this fantastic's
/// home folder while it is registered, for a manager to open.
pub async fn set_canvas(host: &Host, url: &str) -> Result<(), String> {
    let r = host.operator(ID, json!({"type": "set_canvas", "url": url})).await.result;
    match r.get("error").and_then(Value::as_str) {
        Some(e) => Err(format!("`{ID}` set_canvas: {e}")),
        None => Ok(()),
    }
}
