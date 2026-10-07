//! What a record leaves out is READ FROM THE SERVER (owner, 2026-10-07: "defaults
//! can be inferred from the endpoint") — python `vllm_backend/server.py`, text for
//! text.
//!
//! `model`, `max_tokens`, `context_window` and `kv_slots` may be left off the
//! record. Before a verb that runs a turn ([`TURN_VERBS`]), every one still absent
//! is read from the server the record's `endpoint` names and WRITTEN BACK to the
//! record — it then holds a measured value like one the operator set: reflect
//! shows it, a reboot keeps it, and the server is not asked again until the field
//! is cleared (`null`). A field the record sets is used as set, never read.
//!
//! - `model` — the ONE model the server serves (`GET {endpoint}/models`); a server
//!   serving several names them, and the record must pick one.
//! - `context_window` and `max_tokens` — that model's `max_model_len` (same list).
//!   The same number for both: a main turn whose `max_tokens` reaches the window
//!   sends NO `max_tokens`, so the server bounds the reply by what the prompt
//!   leaves.
//! - `kv_slots` — how many full-length conversations the server's KV cache holds:
//!   `floor(kv_cache_max_concurrency)` of the `vllm:cache_config_info` line on the
//!   server's Prometheus page — vLLM's own figure for requests of `max_model_len`
//!   tokens. The page is the record's `metrics_url`, or, left out, the endpoint
//!   with its `/v1` replaced by `/metrics` (vLLM serves both on one port). A door
//!   in front of the server may not forward it.
//!
//! A server that does not answer refuses the verb, naming the field, the URL and
//! what came back. The key goes only to the endpoint's host, as on a chat request.

use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::helpers;
use fantastic_kernel::refusal::{refusal, with, BAD_ARG};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// The fields a record may leave to the server, in the order they are read.
pub const INFERRED: [&str; 4] = ["model", "max_tokens", "context_window", "kv_slots"];
/// The verbs that run a turn of the model — the ones the server is read for.
pub const TURN_VERBS: [&str; 3] = ["send", "inquiry", "context_recall"];

const INFO: &str = "vllm:cache_config_info";

/// A field the server did not answer: the field, and what to tell the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerReadError {
    pub field: String,
    pub message: String,
}

fn fail(field: &str, message: String) -> ServerReadError {
    ServerReadError {
        field: field.to_string(),
        message,
    }
}

/// The record's fields left to the server.
#[must_use]
pub fn missing(rec: &Map<String, Value>) -> Vec<&'static str> {
    INFERRED
        .into_iter()
        .filter(|f| rec.get(*f).is_none_or(Value::is_null))
        .collect()
}

/// The record's `metrics_url`, or the endpoint's `/v1` replaced by `/metrics`;
/// `None` when neither names one (an endpoint that does not end in `/v1`).
#[must_use]
pub fn metrics_url(rec: &Map<String, Value>) -> Option<String> {
    if let Some(url) = rec.get("metrics_url").and_then(Value::as_str) {
        if !url.trim().is_empty() {
            return Some(url.trim().to_string());
        }
    }
    let endpoint = rec.get("endpoint").and_then(Value::as_str).unwrap_or("");
    let endpoint = endpoint.trim_end_matches('/');
    endpoint
        .strip_suffix("/v1")
        .map(|root| format!("{root}/metrics"))
}

async fn get(
    client: &reqwest::Client,
    url: &str,
    key: Option<&str>,
    field: &str,
) -> Result<String, ServerReadError> {
    let mut req = client.get(url);
    if let Some(key) = key {
        req = req.bearer_auth(key);
    }
    let unreachable = || {
        fail(
            field,
            format!(
                "{field}: GET {url} failed — the server is not reachable there; set \
                 {field} on the record"
            ),
        )
    };
    let resp = req.send().await.map_err(|_| unreachable())?;
    let status = resp.status().as_u16();
    if status != 200 {
        return Err(fail(
            field,
            format!(
                "{field}: GET {url} answered HTTP {status} — the server (or a door in \
                 front of it) does not serve it; set {field} on the record"
            ),
        ));
    }
    resp.text().await.map_err(|_| unreachable())
}

/// The `/models` entry the record names, or the ONE the server serves.
///
/// # Errors
/// A body that is no model list, a named model it does not serve, or several
/// served models and none named.
pub fn served_model(
    body: &str,
    model: Option<&str>,
    url: &str,
    field: &str,
) -> Result<Map<String, Value>, ServerReadError> {
    let data = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v.get("data").and_then(Value::as_array).cloned())
        .ok_or_else(|| fail(field, format!("{field}: GET {url} is not a model list")))?;
    let entries: Vec<Map<String, Value>> = data
        .into_iter()
        .filter_map(|m| m.as_object().cloned())
        .collect();
    let served = entries
        .iter()
        .map(|m| match m.get("id") {
            Some(Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => "None".to_string(),
        })
        .collect::<Vec<_>>()
        .join(", ");
    let served = if served.is_empty() {
        "nothing".to_string()
    } else {
        served
    };
    let Some(model) = model else {
        return match entries.as_slice() {
            [one] => Ok(one.clone()),
            _ => Err(fail(
                "model",
                format!("model: {url} serves {served} — not one model; set model on the record"),
            )),
        };
    };
    entries
        .into_iter()
        .find(|m| m.get("id").and_then(Value::as_str) == Some(model))
        .ok_or_else(|| {
            fail(
                field,
                format!("{field}: {url} does not serve model '{model}' — it serves {served}"),
            )
        })
}

/// That entry's `max_model_len`.
///
/// # Errors
/// An entry that names no positive `max_model_len`.
pub fn max_model_len(
    entry: &Map<String, Value>,
    url: &str,
    field: &str,
) -> Result<i64, ServerReadError> {
    match entry.get("max_model_len").and_then(Value::as_i64) {
        Some(n) if n >= 1 => Ok(n),
        _ => {
            let id = match entry.get("id") {
                Some(Value::String(s)) => format!("'{s}'"),
                Some(v) => v.to_string(),
                None => "None".to_string(),
            };
            Err(fail(
                field,
                format!(
                    "{field}: {url} names no max_model_len for {id}; set {field} on the record"
                ),
            ))
        }
    }
}

/// The `name="value"` labels of one Prometheus line, in order.
fn labels(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(open) = line.find('{') else {
        return out;
    };
    let mut rest = &line[open + 1..];
    loop {
        rest = rest.trim_start_matches([',', ' ']);
        let Some(eq) = rest.find("=\"") else {
            return out;
        };
        let name = rest[..eq].to_string();
        let mut value = String::new();
        let mut chars = rest[eq + 2..].char_indices();
        let mut end = None;
        while let Some((i, c)) = chars.next() {
            match c {
                '\\' => {
                    if let Some((_, n)) = chars.next() {
                        value.push(n);
                    }
                }
                '"' => {
                    end = Some(eq + 2 + i + 1);
                    break;
                }
                _ => value.push(c),
            }
        }
        let Some(end) = end else {
            return out;
        };
        out.push((name, value));
        rest = &rest[end..];
    }
}

/// `floor(kv_cache_max_concurrency)` of the ONE `vllm:cache_config_info` line.
///
/// # Errors
/// Not exactly one such line, no `kv_cache_max_concurrency` on it, or under one.
pub fn kv_slots(body: &str, url: &str) -> Result<i64, ServerReadError> {
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| l.starts_with(&format!("{INFO}{{")))
        .collect();
    if lines.len() != 1 {
        return Err(fail(
            "kv_slots",
            format!(
                "kv_slots: {url} carries {} {INFO} lines, not one — set kv_slots on the \
                 record",
                lines.len()
            ),
        ));
    }
    let raw = labels(lines[0])
        .into_iter()
        .find(|(k, _)| k == "kv_cache_max_concurrency")
        .map(|(_, v)| v);
    let Some((raw, value)) = raw.and_then(|r| r.trim().parse::<f64>().ok().map(|f| (r, f))) else {
        return Err(fail(
            "kv_slots",
            format!(
                "kv_slots: {url} names no kv_cache_max_concurrency — set kv_slots on the \
                 record"
            ),
        ));
    };
    let slots = value.floor();
    if slots < 1.0 {
        return Err(fail(
            "kv_slots",
            format!(
                "kv_slots: {url} holds {raw} full-length conversations, under one — set \
                 kv_slots on the record"
            ),
        ));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(slots as i64)
}

fn netloc(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#']).next().unwrap_or("")
}

fn seconds(rec: &Map<String, Value>, key: &str, default: f64) -> f64 {
    rec.get(key)
        .and_then(Value::as_f64)
        .filter(|s| *s > 0.0)
        .unwrap_or(default)
}

/// The values for the record's absent fields, read from the server.
///
/// # Errors
/// [`ServerReadError`] naming the first field it could not read.
pub async fn read(
    rec: &Map<String, Value>,
    key: &dyn KeySource,
) -> Result<Map<String, Value>, ServerReadError> {
    let want = missing(rec);
    let endpoint = rec
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let secret = key
        .key(&endpoint)
        .await
        .map_err(|e| fail(want.first().copied().unwrap_or("model"), e.to_string()))?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs_f64(seconds(
            rec,
            "connect_timeout",
            5.0,
        )))
        .read_timeout(Duration::from_secs_f64(seconds(rec, "read_timeout", 900.0)))
        .build()
        .map_err(|e| fail(want.first().copied().unwrap_or("model"), e.to_string()))?;
    let mut out = Map::new();
    if want.iter().any(|f| *f != "kv_slots") {
        let field = want
            .iter()
            .copied()
            .find(|f| *f != "kv_slots")
            .unwrap_or("model");
        let url = format!("{endpoint}/models");
        let body = get(&client, &url, secret.as_deref(), field).await?;
        let model = rec.get("model").and_then(Value::as_str);
        let entry = served_model(&body, model, &url, field)?;
        if want.contains(&"model") {
            out.insert(
                "model".into(),
                entry.get("id").cloned().unwrap_or(Value::Null),
            );
        }
        for f in ["max_tokens", "context_window"] {
            if want.contains(&f) {
                out.insert(f.into(), json!(max_model_len(&entry, &url, f)?));
            }
        }
    }
    if want.contains(&"kv_slots") {
        let Some(url) = metrics_url(rec) else {
            return Err(fail(
                "kv_slots",
                format!(
                    "kv_slots: the endpoint {endpoint} does not end in /v1, so the server's \
                     metrics page is not known — set metrics_url or kv_slots on the record"
                ),
            ));
        };
        let key = secret
            .as_deref()
            .filter(|_| netloc(&url) == netloc(&endpoint));
        let body = get(&client, &url, key, "kv_slots").await?;
        out.insert("kv_slots".into(), json!(kv_slots(&body, &url)?));
    }
    Ok(out)
}

/// Read the record's absent fields from the server and write them back. `None`
/// when nothing is absent or all were written; otherwise the verb's refusal.
pub async fn resolve(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Option<Value> {
    let rec = helpers::agent_meta(agent_id, kernel);
    if missing(&rec).is_empty() {
        return None;
    }
    let key = key_reader(agent_id, kernel, crate::NAME, false);
    let values = match read(&rec, &key).await {
        Ok(v) => v,
        Err(e) => {
            let mut r = refusal(BAD_ARG, e.message);
            with(&mut r, "arg", json!(e.field));
            return Some(r);
        }
    };
    // What the server gave passes the same write check as the operator's values.
    if let Some(bad) = crate::validate(&values, &rec) {
        let first = values.keys().next().cloned().unwrap_or_default();
        let mut r = refusal(
            BAD_ARG,
            format!("the server's values do not fit the record: {bad}"),
        );
        with(&mut r, "arg", json!(first));
        return Some(r);
    }
    kernel.update(agent_id, values);
    None
}
