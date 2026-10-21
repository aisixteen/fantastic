//! THE AI SET-UP WIZARD's host side (owner 2026-10-12: the terminal's set-up page
//! is the canvas's two-page wizard). Each act is ONE call, as the human:
//!
//! - [`create`]: a NEW provider's details → its key (when pasted: where keys are
//!   kept, if not chosen yet, then the key itself), the record typed by the
//!   backend's schema, saved under its name (the first saved one becomes the
//!   default), and an AI agent written from it — its id the name, or the next
//!   free `<name>-<n>`;
//! - [`from_saved`]: an AI agent from a saved set-up, as it is;
//! - [`save_edit`]: a saved set-up's fields rewritten under its name.
//!
//! The same steps the canvas's set-up cell takes through `kern/setup_agent`.

use serde_json::{json, Map, Value};

use crate::kern::host::Host;

/// A key pasted on page 2: its value, and where keys are kept when that is not
/// chosen yet (`plain` | `secure`).
#[derive(Debug, Clone, PartialEq)]
pub struct KeyIn {
    pub secret: String,
    pub mode: Option<String>,
}

/// The first free agent id for `name`: itself, else `<name>-2`, `<name>-3`…
pub fn free_id(host: &Host, name: &str) -> String {
    let taken = |id: &str| {
        host.kernel
            .agents
            .contains_key(&fantastic_kernel::AgentId::from(id))
    };
    if !taken(name) {
        return name.to_string();
    }
    (2..)
        .map(|n| format!("{name}-{n}"))
        .find(|c| !taken(c))
        .expect("a free id")
}

async fn key(host: &Host, module: &str, endpoint: &str, k: &KeyIn) -> Result<(), String> {
    if let Some(m) = &k.mode {
        let m = fantastic_user_home_store::secrets::Mode::parse(m)
            .ok_or_else(|| format!("no key mode {m:?} — plain or secure"))?;
        crate::kern::keys::set_mode(host, m).await?;
    }
    crate::kern::keys::store(host, module, endpoint, &k.secret)
        .await
        .map(|_| ())
}

/// Save `texts` for `module` under `name`: typed, its stored key NAMED (a ref).
async fn save(
    host: &Host,
    name: &str,
    module: &str,
    texts: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let fields = crate::kern::setup::typed(host, module, texts)?;
    let mut record = fields.clone();
    crate::kern::keys::attach(host, module, &mut record)?;
    record.insert("handler_module".into(), json!(module));
    crate::kern::saved::save(host, name, Value::Object(record)).await?;
    Ok(fields)
}

async fn write(
    host: &Host,
    name: &str,
    module: &str,
    fields: Map<String, Value>,
) -> Result<String, String> {
    let id = free_id(host, name);
    let rec = crate::kern::setup::Record {
        id: id.clone(),
        handler: module.to_string(),
        fields,
        may_call: Vec::new(),
    };
    crate::kern::setup::write(host, &rec).await?;
    Ok(id)
}

/// A NEW provider's details → key, saved set-up (the first is the default), agent.
/// Returns the agent's id.
pub async fn create(
    host: &Host,
    name: &str,
    module: &str,
    texts: &Map<String, Value>,
    k: Option<&KeyIn>,
) -> Result<String, String> {
    let endpoint = texts
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(k) = k {
        key(host, module, &endpoint, k).await?;
    }
    let fields = save(host, name, module, texts).await?;
    if crate::kern::saved::shelf(host).await?.preferred.is_none() {
        crate::kern::saved::prefer(host, Some(name)).await?;
    }
    write(host, name, module, fields).await
}

/// An AI agent from saved set-up `name`. Returns its id.
pub async fn from_saved(host: &Host, name: &str) -> Result<String, String> {
    let shelf = crate::kern::saved::shelf(host).await?;
    let s = shelf
        .saved
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no saved set-up `{name}`"))?;
    let module = s.handler();
    // its key is named again by the write, for its endpoint (`kern/keys`)
    let fields: Map<String, Value> = s
        .record
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "handler_module" | "credentials"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    write(host, name, &module, fields).await
}

/// A saved set-up's fields rewritten under its name (and a key, when pasted).
pub async fn save_edit(
    host: &Host,
    name: &str,
    module: &str,
    texts: &Map<String, Value>,
    k: Option<&KeyIn>,
) -> Result<(), String> {
    let endpoint = texts
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if let Some(k) = k {
        key(host, module, &endpoint, k).await?;
    }
    save(host, name, module, texts).await.map(|_| ())
}
