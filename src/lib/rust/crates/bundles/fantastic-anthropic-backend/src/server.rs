//! What the record leaves out is READ FROM THE API — python
//! `anthropic_backend/server.py`, text for text.
//!
//! `max_tokens` and `context_window` may be left off the record. Before a verb that
//! runs a turn ([`TURN_VERBS`]), each one still absent is read from the API's model
//! entry — `GET {models}/{model}`, `{models}` being the endpoint with its
//! `/messages` replaced by `/models` — and WRITTEN BACK: `max_tokens` from the
//! entry's `max_tokens`, `context_window` from its `max_input_tokens`.

use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::helpers;
use fantastic_kernel::refusal::{refusal, with, BAD_ARG};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// The fields a record may leave to the API, in the order they are read.
pub const INFERRED: [&str; 2] = ["max_tokens", "context_window"];
/// The verbs that run a turn of the model — the ones the API is read for.
pub const TURN_VERBS: [&str; 3] = ["send", "inquiry", "context_recall"];
/// Seconds to wait on the model entry.
pub const READ_TIMEOUT: f64 = 30.0;

/// The entry field each inferred field is read from.
fn source(field: &str) -> &'static str {
    if field == "max_tokens" {
        "max_tokens"
    } else {
        "max_input_tokens"
    }
}

/// A field the API did not answer, and what to tell the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerReadError {
    /// The field.
    pub field: String,
    /// The refusal text.
    pub message: String,
}

fn fail(field: &str, message: String) -> ServerReadError {
    ServerReadError {
        field: field.to_string(),
        message,
    }
}

/// The record's fields left to the API.
#[must_use]
pub fn missing(rec: &Map<String, Value>) -> Vec<&'static str> {
    INFERRED
        .into_iter()
        .filter(|f| rec.get(*f).is_none_or(Value::is_null))
        .collect()
}

/// The model list beside a Messages endpoint; `None` when the endpoint does not end
/// in `/messages`.
#[must_use]
pub fn models_url(endpoint: &str) -> Option<String> {
    endpoint
        .trim_end_matches('/')
        .strip_suffix("/messages")
        .map(|base| format!("{base}/models"))
}

/// The values for the record's absent fields.
///
/// # Errors
/// [`ServerReadError`] naming the first field it could not read.
pub async fn read(
    rec: &Map<String, Value>,
    key: &dyn KeySource,
) -> Result<Map<String, Value>, ServerReadError> {
    let want = missing(rec);
    let first = want.first().copied().unwrap_or("max_tokens");
    let endpoint = rec
        .get("endpoint")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_end_matches('/')
        .to_string();
    let Some(base) = models_url(&endpoint) else {
        return Err(fail(
            first,
            format!(
                "{first}: the endpoint {endpoint} does not end in /messages, so the model list \
                 is not known — set {first} on the record"
            ),
        ));
    };
    let model = rec.get("model").and_then(Value::as_str).unwrap_or("");
    let url = format!("{base}/{model}");
    let secret = key
        .key(&endpoint)
        .await
        .map_err(|e| fail(first, e.to_string()))?
        .filter(|k| !k.is_empty());
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs_f64(crate::CONNECT_TIMEOUT))
        .timeout(Duration::from_secs_f64(READ_TIMEOUT))
        .build()
        .map_err(|e| fail(first, e.to_string()))?;
    let mut req = client
        .get(&url)
        .header("anthropic-version", crate::ANTHROPIC_VERSION);
    if let Some(k) = &secret {
        req = req.header("x-api-key", k);
    }
    let unreachable = || {
        fail(
            first,
            format!("{first}: GET {url} failed — the API is not reachable there; set {first} on the record"),
        )
    };
    let resp = req.send().await.map_err(|_| unreachable())?;
    let code = resp.status().as_u16();
    if code != 200 {
        let why = if matches!(code, 401 | 403) {
            " — the API wants a key: name it in `credentials` ({store, ref})"
        } else {
            ""
        };
        return Err(fail(
            first,
            format!("{first}: GET {url} answered HTTP {code}{why}; set {first} on the record"),
        ));
    }
    let body = resp.text().await.map_err(|_| unreachable())?;
    let entry = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    let mut out = Map::new();
    for f in want {
        let src = source(f);
        match entry.get(src).and_then(Value::as_i64).filter(|n| *n >= 1) {
            Some(n) if !entry[src].is_boolean() => {
                out.insert(f.to_string(), json!(n));
            }
            _ => {
                return Err(fail(
                    f,
                    format!("{f}: {url} names no {src}; set {f} on the record"),
                ));
            }
        }
    }
    Ok(out)
}

/// Read the record's absent fields from the API and write them back. `None` when
/// nothing is absent or all were written; otherwise the verb's refusal.
pub async fn resolve(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Option<Value> {
    let rec = helpers::agent_meta(agent_id, kernel);
    let named = |k: &str| {
        rec.get(k)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    // nothing to read, or nothing to read it with — the provider names a missing
    // `model` / `endpoint` when it is built
    if missing(&rec).is_empty() || !named("model") || !named("endpoint") {
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
    if let Some(bad) = crate::validate(&values, &rec) {
        let first = values.keys().next().cloned().unwrap_or_default();
        let mut r = refusal(
            BAD_ARG,
            format!("the API's values do not fit the record: {bad}"),
        );
        with(&mut r, "arg", json!(first));
        return Some(r);
    }
    kernel.update(agent_id, values);
    None
}
