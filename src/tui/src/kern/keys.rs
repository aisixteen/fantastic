//! PROVIDER KEYS — in the kernel's CREDENTIAL STORE, never the environment
//! (owner, 2026-10-01: keys only through `credential_store`; no restarts).
//!
//! - ONE store agent, `credentials`, under the root. Its BACKEND is the user's
//!   key mode (`fantastic_user_home_store::secrets`): plain → one 0600 file in
//!   the user's home, secure → the keychain. No mode chosen = no store, and a
//!   key cannot be stored until one is.
//! - A key is a REF, `<provider>/<host>` (`responses/api.openai.com`), bound to
//!   the endpoint's host: the store refuses to hand it out for any other host.
//!   Written by the operator with `secret: [payload.fields]` — the value lands
//!   in no row, ledger or stream.
//! - An AI agent NAMES its ref on its record (`credentials: {store, ref}`) and
//!   its backend reads it before every request: a stored or rotated key is live
//!   on the next call, with no restart. [`wire`] gives it the three grants that
//!   takes (its `can_call` [get], the store's `callable_by`, the ref's readers).
//! - The refs (metadata only — host, field names) are also remembered in the
//!   user's home, so every project's store knows the keys already stored there.

use std::sync::Arc;

use fantastic_kernel::{AgentId, Item};
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
    let provider = module.strip_suffix("_backend.tools").unwrap_or(module);
    let host = host_of(endpoint).ok_or_else(|| format!("no host in the endpoint {endpoint:?} — fill the endpoint first; a key is bound to its host"))?;
    Ok(format!("{provider}/{host}"))
}

fn store_meta(h: &Host) -> Option<Map<String, Value>> {
    h.kernel.agents.get(&AgentId::from(STORE)).map(|a| a.value().record().meta.clone())
}

fn refs_of(meta: &Map<String, Value>) -> Map<String, Value> {
    meta.get("refs").and_then(Value::as_object).cloned().unwrap_or_default()
}

/// The store, as the user's key mode says: created when missing, its backend
/// set to the mode's, and every ref the user's home remembers made known to
/// it. With no mode chosen there is nothing to keep, and no store.
pub async fn ensure_store(h: &Host) -> Result<(), String> {
    let s = secrets();
    let Some(m) = s.mode()? else { return Ok(()) };
    let mut fields = m.store(&s.home()?).as_object().cloned().unwrap_or_default();
    let mut refs = store_meta(h).map(|m| refs_of(&m)).unwrap_or_default();
    for (r, host) in s.refs()? {
        let e = refs.entry(r).or_insert_with(|| json!({}));
        if let Some(o) = e.as_object_mut() {
            o.entry("host").or_insert(json!(host));
            o.entry("fields").or_insert(json!([FIELD]));
        }
    }
    fields.insert("refs".into(), Value::Object(refs));
    let payload = if store_meta(h).is_some() {
        let mut p = json!({"type": "update_agent", "id": STORE});
        p.as_object_mut().expect("an object").extend(fields);
        p
    } else {
        let mut p = json!({"type": "create_agent", "parent_id": h.root, "id": STORE, "handler_module": fantastic_credential_store::HANDLER_MODULE});
        p.as_object_mut().expect("an object").extend(fields);
        p
    };
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

/// Store `secret` as the key for `module` at `endpoint`. Returns the ref.
pub async fn store(h: &Host, module: &str, endpoint: &str, secret: &str) -> Result<String, String> {
    let s = secrets();
    let m = s.mode()?.ok_or("choose where keys are kept first — plain or secure")?;
    let r = ref_for(module, endpoint)?;
    ensure_store(h).await?;
    let item = Item::new(
        AgentId::from(STORE),
        json!({"type": "set", "ref": r, "host": endpoint, "fields": {FIELD: secret}}),
    )
    .with_secret(json!(["payload.fields"]));
    let k = Arc::clone(&h.kernel);
    let out = fantastic_kernel::send::with_operator(async move { k.send(vec![item]).await }).await.remove(0).result;
    if let Some(e) = out.get("error").and_then(Value::as_str) {
        return Err(format!("the key was not stored: {e}"));
    }
    s.remember_ref(&r, &host_of(endpoint).unwrap_or_default())?;
    let _ = m;
    Ok(r)
}

/// Whether `module`'s records take a key: `Some(required)` when its record
/// schema declares a `credentials` field.
pub fn keyed(h: &Host, module: &str) -> Option<bool> {
    h.kernel
        .bundles
        .catalog()
        .find(|(m, _)| *m == module)
        .and_then(|(_, b)| b.record_schema())
        .and_then(|s| {
            s["fields"].as_array().and_then(|a| {
                a.iter().find(|f| f["type"] == "credentials").map(|f| f["required"].as_bool().unwrap_or(false))
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

/// The three grants an AI agent needs to read ITS key `r`: its own `can_call`
/// names the store [get], the store's `callable_by` names it [get], and the
/// ref's `readers` name it. Merged, never replaced.
pub async fn wire(h: &Host, ai: &str, r: &str) -> Result<(), String> {
    let meta = store_meta(h).ok_or("no credential store — choose where keys are kept first")?;
    let mut refs = refs_of(&meta);
    let entry = refs.get_mut(r).and_then(Value::as_object_mut).ok_or_else(|| format!("no key stored under {r}"))?;
    let readers = entry.entry("readers").or_insert(json!([]));
    if let Some(a) = readers.as_array_mut() {
        if !a.iter().any(|x| x == ai) {
            a.push(json!(ai));
        }
    }
    let mut cb = meta.get("callable_by").and_then(Value::as_object).cloned().unwrap_or_default();
    cb.insert(ai.into(), json!(["get"]));
    let mut cc = h
        .kernel
        .agents
        .get(&AgentId::from(ai))
        .and_then(|a| a.value().record().meta.get("can_call").and_then(Value::as_object).cloned())
        .unwrap_or_default();
    cc.insert(STORE.into(), json!(["get"]));
    for p in [
        json!({"type": "update_agent", "id": STORE, "refs": refs, "callable_by": cb}),
        json!({"type": "update_agent", "id": ai, "can_call": cc}),
    ] {
        let r = h.operator("kernel", p).await.result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            return Err(format!("could not grant {ai} its key: {e}"));
        }
    }
    Ok(())
}

/// Wire `ai` to the key its record names, if it names one in this store.
pub async fn wire_record(h: &Host, ai: &str, record: &Map<String, Value>) -> Result<(), String> {
    let Some(c) = record.get("credentials").and_then(Value::as_object) else { return Ok(()) };
    if c.get("store").and_then(Value::as_str) != Some(STORE) {
        return Ok(());
    }
    let r = c.get("ref").and_then(Value::as_str).unwrap_or("");
    wire(h, ai, r).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ref_is_the_provider_and_the_endpoints_host() {
        assert_eq!(ref_for("responses_backend.tools", "https://api.openai.com/v1/responses").unwrap(), "responses/api.openai.com");
        assert_eq!(ref_for("vllm_backend.tools", "http://Box.Local:8000/v1").unwrap(), "vllm/box.local:8000");
        assert!(ref_for("vllm_backend.tools", "").is_err(), "no endpoint, no host, no ref");
        assert_eq!(host_of("http://u:p@h.example/x"), Some("h.example".into()));
    }
}
