//! A record that leaves `model` out has it READ FROM THE SERVER — python
//! `ollama_backend/server.py`, text for text.
//!
//! Before a verb that runs a turn ([`TURN_VERBS`]), an absent `model` is read from
//! the server the record's `endpoint` names — the ONE model `GET {endpoint}/api/tags`
//! lists — and WRITTEN BACK to the record, so reflect shows it and it is not read
//! again until the field is cleared (`null`). A server listing several names them and
//! the record must pick one. A model set on the record is used as set.
//!
//! Only the model: the window ollama loads a model with is the
//! request's `num_ctx`, else the Modelfile's, else the server's default — and that
//! default is not in the API, so nothing about the window is read.

use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::helpers;
use fantastic_kernel::refusal::{refusal, with, BAD_ARG};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// The verbs that run a turn of the model — the ones the server is read for.
pub const TURN_VERBS: [&str; 3] = ["send", "inquiry", "context_recall"];
/// Seconds to reach the server.
pub const CONNECT_TIMEOUT: f64 = 5.0;
/// Seconds to wait on the model list.
pub const READ_TIMEOUT: f64 = 30.0;

/// The ONE model a `/api/tags` body lists.
///
/// # Errors
/// A body that is no model list, or one listing other than exactly one model.
pub fn served_model(body: &str, url: &str) -> Result<String, String> {
    let models = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("models").and_then(Value::as_array).cloned())
        .ok_or_else(|| format!("model: GET {url} is not a model list"))?;
    let names: Vec<Value> = models
        .iter()
        .filter(|m| m.is_object())
        .map(|m| m.get("name").cloned().unwrap_or(Value::Null))
        .collect();
    match names.as_slice() {
        [Value::String(n)] if !n.is_empty() => Ok(n.clone()),
        _ => {
            let served = names
                .iter()
                .map(fantastic_bundle::py_str)
                .collect::<Vec<_>>()
                .join(", ");
            let served = if served.is_empty() {
                "nothing".to_string()
            } else {
                served
            };
            Err(format!(
                "model: {url} serves {served} — not one model; set model on the record"
            ))
        }
    }
}

/// The model the server at `endpoint` serves.
///
/// # Errors
/// The refusal text: unreachable, a non-200 answer, or not exactly one model.
pub async fn read_model(endpoint: &str, key: &dyn KeySource) -> Result<String, String> {
    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let secret = key.key(endpoint).await.map_err(|e| e.to_string())?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs_f64(CONNECT_TIMEOUT))
        .timeout(Duration::from_secs_f64(READ_TIMEOUT))
        .build()
        .map_err(|e| e.to_string())?;
    let mut req = client.get(&url);
    if let Some(k) = secret.as_deref().filter(|k| !k.is_empty()) {
        req = req.bearer_auth(k);
    }
    let unreachable = || {
        format!(
            "model: GET {url} failed — the server is not reachable there; set model on the record"
        )
    };
    let resp = req.send().await.map_err(|_| unreachable())?;
    let code = resp.status().as_u16();
    if code != 200 {
        return Err(format!(
            "model: GET {url} answered HTTP {code} — the server (or a door in front of it) \
             does not serve it; set model on the record"
        ));
    }
    let body = resp.text().await.map_err(|_| unreachable())?;
    served_model(&body, &url)
}

/// Read an absent `model` from the server and write it back. `None` when the
/// record names one or it was written; otherwise the verb's refusal.
pub async fn resolve(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Option<Value> {
    let rec = helpers::agent_meta(agent_id, kernel);
    let endpoint = rec
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    // Nothing to read without an endpoint — the provider names it when it is built.
    if rec.get("model").is_some_and(|m| !m.is_null()) || endpoint.is_empty() {
        return None;
    }
    let key = key_reader(agent_id, kernel, crate::BACKEND, false);
    match read_model(&endpoint, &key).await {
        Ok(model) => {
            let mut patch = Map::new();
            patch.insert("model".into(), json!(model));
            kernel.update(agent_id, patch);
            None
        }
        Err(e) => {
            let mut r = refusal(BAD_ARG, e);
            with(&mut r, "arg", json!("model"));
            Some(r)
        }
    }
}
