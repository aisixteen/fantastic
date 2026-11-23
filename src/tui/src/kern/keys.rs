//! PROVIDER KEYS — in the kernel's CREDENTIAL STORE, never the environment
//! (owner, 2026-10-01: keys only through `credential_store`; no restarts).
//!
//! - ONE store agent, `credentials`, under `user` (the blueprint's record, outside
//!   the user space). Its BACKEND is the user's key mode
//!   (`fantastic_user_home_store::secrets`): plain → one 0600 file in the user's
//!   home, secure → the keychain, set on the record at boot. No mode chosen = no
//!   backend, and a key cannot be stored until one is.
//! - A key is a REF, `<provider>/<host>` (`responses/api.openai.com`), bound to
//!   the endpoint's host: the store refuses to hand it out for any other host.
//!   The ref is DECLARED on the store's record (`update_agent` on its `refs`),
//!   then its value written by the shell, the operator, marked `secret`: every
//!   row, event and mirror of the call shows it as `[secret]`.
//! - An AI agent NAMES its ref on its record (`credentials: {store, ref}`) and
//!   its backend reads it before every request: a stored or rotated key is live
//!   on the next call, with no restart. Reading it takes the agent's own line
//!   `credentials: {call: [get]}` — in the garage, the line its `create` line forces.
//! - The refs (metadata only — host, field names) are also remembered in the
//!   user's home, so every project's store knows the keys already stored there.


use fantastic_user_home_store::secrets::{Mode, Secrets};
use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// The credential store's id.
pub const STORE: &str = "credentials";
/// The one credential field a backend sends as its key.
pub const FIELD: &str = "api_key";

fn secrets() -> Secrets {
    Secrets::new(fantastic_user_home_store::home_dir_from_env())
}

/// Where keys are kept: the chosen mode (`None` until chosen) and, in words,
/// where that is.
pub fn mode() -> Result<(Option<Mode>, String), String> {
    let s = secrets();
    let m = s.mode()?;
    Ok((m, s.describe(m)))
}

/// The `host[:port]` of an endpoint URL, lowercased — the ref's second half.
pub fn host_of(endpoint: &str) -> Option<String> {
    let rest = endpoint.split_once("://").map_or(endpoint, |(_, r)| r);
    let h = rest.split(['/', '?', '#']).next().unwrap_or("").rsplit('@').next().unwrap_or("").to_lowercase();
    (!h.is_empty()).then_some(h)
}

/// The ref a key for `module` at `endpoint` lives under.
pub fn ref_for(module: &str, endpoint: &str) -> Result<String, String> {
    let provider = module.strip_suffix("_backend.tools").or_else(|| module.strip_suffix(".tools")).unwrap_or(module);
    let host = host_of(endpoint).ok_or_else(|| format!("no host in the endpoint {endpoint:?} — fill the endpoint first; a key is bound to its host"))?;
    Ok(format!("{provider}/{host}"))
}

fn store_meta(h: &Host) -> Option<Map<String, Value>> {
    h.kernel.resolve_target(STORE).map(|a| a.record().meta.clone())
}

fn refs_of(meta: &Map<String, Value>) -> Map<String, Value> {
    meta.get("refs").and_then(Value::as_object).cloned().unwrap_or_default()
}

/// The store's record, as the user's key mode says: its backend set to the
/// mode's, and every ref the user's home remembers made known to it. With no
/// mode chosen there is nothing to set. The store itself is the blueprint's.
pub async fn ensure_store(h: &Host) -> Result<(), String> {
    let s = secrets();
    let Some(m) = s.mode()? else { return Ok(()) };
    // the store's `file` must be ABSOLUTE; a relative FANTASTIC_HOME is made so here
    let home = std::path::absolute(s.home()?).map_err(|e| format!("the user's home: {e}"))?;
    let mut fields = m.store(&home).as_object().cloned().unwrap_or_default();
    let mut refs = store_meta(h).map(|m| refs_of(&m)).unwrap_or_default();
    for (r, host) in s.refs()? {
        let e = refs.entry(r).or_insert_with(|| json!({}));
        if let Some(o) = e.as_object_mut() {
            o.entry("host").or_insert(json!(host));
            o.entry("fields").or_insert(json!([FIELD]));
        }
    }
    fields.insert("refs".into(), Value::Object(refs));
    if store_meta(h).is_none() {
        return Err(format!("the credential store `{STORE}` is not in the tree"));
    }
    let mut payload = json!({"type": "update_agent", "id": STORE});
    payload.as_object_mut().expect("an object").extend(fields);
    let r = h.operator("kernel", payload).await.result;
    match r.get("error").and_then(Value::as_str) {
        Some(e) => Err(format!("the credential store: {e}")),
        None => Ok(()),
    }
}

/// Choose (or change) where keys are kept. Refused while keys are stored in
/// another mode: they cannot move, and would be left behind.
pub async fn set_mode(h: &Host, m: Mode) -> Result<String, String> {
    let s = secrets();
    let now = s.mode()?;
    if now.is_some_and(|n| n != m) {
        let n = s.refs()?.len();
        if n > 0 {
            return Err(format!(
                "{n} key(s) are kept {} — they cannot move to another mode; forget them first",
                s.describe(now)
            ));
        }
    }
    s.set_mode(m)?;
    ensure_store(h).await?;
    Ok(s.describe(Some(m)))
}

/// Declare ref `r` bound to `host` on the store's record, keeping every other
/// ref: `refs` is written whole, by the operator.
async fn declare(h: &Host, r: &str, host: &str) -> Result<(), String> {
    let mut refs = store_meta(h).map(|m| refs_of(&m)).unwrap_or_default();
    let e = refs.entry(r.to_string()).or_insert_with(|| json!({}));
    if let Some(o) = e.as_object_mut() {
        o.insert("host".into(), json!(host));
        o.entry("fields").or_insert(json!([FIELD]));
    }
    let out = h.operator("kernel", json!({"type": "update_agent", "id": STORE, "refs": refs})).await.result;
    match out.get("error").and_then(Value::as_str) {
        Some(e) => Err(format!("the key's ref {r} could not be declared: {e}")),
        None => Ok(()),
    }
}

/// Store `secret` as the key for `module` at `endpoint`. Returns the ref.
pub async fn store(h: &Host, module: &str, endpoint: &str, secret: &str) -> Result<String, String> {
    let s = secrets();
    let m = s.mode()?.ok_or("choose where keys are kept first — plain or secure")?;
    let r = ref_for(module, endpoint)?;
    ensure_store(h).await?;
    // the ref is DECLARED first, on the store's record (`set` never creates one)
    declare(h, &r, &host_of(endpoint).unwrap_or_default()).await?;
    let out = h.operator_secret(STORE, json!({"type": "set", "ref": r, "fields": {FIELD: secret}}), &["fields.api_key"]).await.result;
    if let Some(e) = out.get("error").and_then(Value::as_str) {
        return Err(format!("the key was not stored: {e}"));
    }
    s.remember_ref(&r, &host_of(endpoint).unwrap_or_default())?;
    let _ = m;
    Ok(r)
}

/// Every key the user's home remembers: `(ref, host)` — metadata only, never
/// a value (the set-up wizard's "saved keys").
pub fn stored() -> Result<Vec<(String, String)>, String> {
    secrets().refs()
}

/// Whether `module`'s records take a key: `Some(required)` when its record
/// schema declares a `credentials` field.
pub fn keyed(h: &Host, module: &str) -> Option<bool> {
    h.kernel
        .bundles
        .catalog()
        .find(|(m, _)| *m == module)
        .map(|(_, b)| fantastic_kernel::verbs::record_fields(b.record()))
        .and_then(|s| {
            s["fields"].as_array().and_then(|a| {
                a.iter().find(|f| f["name"] == "credentials").map(|f| f["required"].as_bool().unwrap_or(false))
            })
        })
}

/// Name, on a record about to be created, the key it needs: the ref for its
/// handler at its endpoint, when one is stored. A backend that REQUIRES a key
/// with none stored is refused here, naming the ref. A record that already
/// names `credentials` is left as it is.
pub fn attach(h: &Host, module: &str, rec: &mut Map<String, Value>) -> Result<(), String> {
    if rec.contains_key("credentials") {
        return Ok(());
    }
    let Some(required) = keyed(h, module) else { return Ok(()) };
    let endpoint = rec.get("endpoint").and_then(Value::as_str).unwrap_or("");
    match ref_for(module, endpoint) {
        Ok(r) if has(h, &r) => {
            rec.insert("credentials".into(), json!({"store": STORE, "ref": r}));
            Ok(())
        }
        Ok(r) if required => Err(format!("no key stored for {r} — store one first (k on the set-up page)")),
        Err(e) if required => Err(e),
        _ => Ok(()),
    }
}

/// Whether a key is stored under `r`.
pub fn has(h: &Host, r: &str) -> bool {
    store_meta(h).is_some_and(|m| refs_of(&m).contains_key(r))
}

/// A create spec naming a key in this store: its `can` gains the line
/// `credentials: {call: [get]}` (the agent's own line to read its key).
pub fn grant_on_create(spec: &mut Map<String, Value>) {
    let named = spec
        .get("credentials")
        .and_then(Value::as_object)
        .is_some_and(|c| c.get("store").and_then(Value::as_str) == Some(STORE));
    if !named {
        return;
    }
    if let Some(Value::Object(can)) = spec.get_mut("can") {
        can.insert(STORE.into(), json!({"call": ["get"]}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_is_the_provider_and_the_endpoints_host() {
        assert_eq!(ref_for("responses_backend.tools", "https://api.openai.com/v1/responses").unwrap(), "responses/api.openai.com");
        assert_eq!(ref_for("vllm_backend.tools", "http://Box.Local:8000/v1").unwrap(), "vllm/box.local:8000");
        assert!(ref_for("vllm_backend.tools", "").is_err(), "no endpoint, no host, no ref");
        assert_eq!(ref_for("mcp_client.tools", "http://127.0.0.1:9000/mcp").unwrap(), "mcp_client/127.0.0.1:9000", "an MCP server's key: its bundle name and the url's host");
        assert_eq!(host_of("http://u:p@h.example/x"), Some("h.example".into()));
    }
}
