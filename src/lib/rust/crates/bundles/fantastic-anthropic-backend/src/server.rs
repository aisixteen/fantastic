//! What the record leaves out is READ FROM THE API — python
//! `anthropic_backend/server.py`, text for text.
//!
//! `context_window` may be left off the provider's record, and `max_tokens` off the
//! calling chat's knobs (#632: a turn's output budget is the chat's). Before
//! `complete`, the verb that runs a turn, each one still absent is read from the
//! API's model entry — `GET {models}/{model}`, `{models}` being the endpoint with its
//! `/messages` replaced by `/models`: `context_window` from its `max_input_tokens`,
//! WRITTEN BACK to the provider's record; `max_tokens` from the entry's `max_tokens`
//! BOUNDED to [`default_max_tokens`] (260000, plus the record's `thinking_budget`,
//! never above the entry's own maximum), for that pass, and kept in this process
//! for the next pass on the same model.

use fantastic_ai_core::constants::default_max_tokens;
use fantastic_ai_core::credentials::{key_reader, KeySource};
use fantastic_ai_core::helpers;
use fantastic_kernel::refusal::{refusal, with, BAD_ARG};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// The output budgets read for a chat that names none, by provider, endpoint, model and
/// thinking budget: the API is asked once per process.
fn read_budgets() -> &'static Mutex<HashMap<String, i64>> {
    static T: OnceLock<Mutex<HashMap<String, i64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The fields a record may leave to the API, in the order they are read.
pub const INFERRED: [&str; 2] = ["max_tokens", "context_window"];
/// The verbs that run a turn of the model — the ones the API is read for.
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
    /// The API's HTTP status, when it answered one (`provider::read_reason`).
    pub status: Option<u16>,
}

fn fail(field: &str, message: String) -> ServerReadError {
    ServerReadError {
        field: field.to_string(),
        message,
        status: None,
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

/// The models the key can use: `GET {models_url(endpoint)}?limit=1000`, each entry
/// reduced to `{id, display_name, max_input_tokens, max_tokens}` (null when absent).
///
/// # Errors
/// Text naming the URL and the HTTP code when the API does not answer, or the endpoint
/// when it does not end in `/messages`.
pub async fn list(endpoint: &str, key: &dyn KeySource) -> Result<Vec<Value>, String> {
    let Some(base) = models_url(endpoint) else {
        return Err(format!(
            "the endpoint {endpoint} does not end in /messages, so the model list is not known"
        ));
    };
    let url = format!("{base}?limit=1000");
    let secret = key
        .key(endpoint)
        .await
        .map_err(|e| format!("GET {url}: {e}"))?
        .filter(|k| !k.is_empty());
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs_f64(crate::CONNECT_TIMEOUT))
        .timeout(Duration::from_secs_f64(READ_TIMEOUT))
        .build()
        .map_err(|e| format!("GET {url}: {e}"))?;
    let mut req = client
        .get(&url)
        .header("anthropic-version", crate::ANTHROPIC_VERSION);
    if let Some(k) = &secret {
        req = req.header("x-api-key", k);
    }
    let unreachable = || format!("GET {url} failed — the API is not reachable there");
    let resp = req.send().await.map_err(|_| unreachable())?;
    let code = resp.status().as_u16();
    if code != 200 {
        let why = if matches!(code, 401 | 403) {
            " — the API wants a key: name it in `credentials` ({store, ref})"
        } else {
            ""
        };
        return Err(format!("GET {url} answered HTTP {code}{why}"));
    }
    let body = resp.text().await.map_err(|_| unreachable())?;
    let parsed = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    let Some(data) = parsed.get("data").and_then(Value::as_array).cloned() else {
        return Err(format!(
            "GET {url} answered HTTP 200 without a `data` list — not a model list"
        ));
    };
    Ok(data
        .into_iter()
        .map(|entry| {
            json!({
                "id": entry.get("id").cloned().unwrap_or(Value::Null),
                "display_name": entry.get("display_name").cloned().unwrap_or(Value::Null),
                "max_input_tokens": entry
                    .get("max_input_tokens")
                    .cloned()
                    .unwrap_or(Value::Null),
                "max_tokens": entry.get("max_tokens").cloned().unwrap_or(Value::Null),
            })
        })
        .collect())
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
        return Err(ServerReadError {
            status: Some(code),
            ..fail(
                first,
                format!("{first}: GET {url} answered HTTP {code}{why}; set {first} on the record"),
            )
        });
    }
    let body = resp.text().await.map_err(|_| unreachable())?;
    let entry = serde_json::from_str::<Value>(&body).unwrap_or(Value::Null);
    let mut out = Map::new();
    for f in want {
        let src = source(f);
        match entry.get(src).and_then(Value::as_i64).filter(|n| *n >= 1) {
            Some(n) if !entry[src].is_boolean() => {
                // the entry's `max_tokens` is the model's MAXIMUM — what a record
                // left out is bounded below it (#495)
                let n = if f == "max_tokens" {
                    let budget = rec.get("thinking_budget").and_then(Value::as_i64);
                    default_max_tokens(budget, n)
                } else {
                    n
                };
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

/// Read what a pass leaves to the API: the record's absent `context_window`, written
/// back, and — when the chat names no `max_tokens` (`chat_max`) — the pass's output
/// budget. `Ok` carries that budget (`None`: the chat names one, or nothing can be
/// read); `Err` is the verb's refusal.
///
/// # Errors
/// The refusal, naming the field the API did not answer.
pub async fn resolve(
    agent_id: &AgentId,
    kernel: &Arc<Kernel>,
    model: Option<String>,
    chat_max: Option<i64>,
) -> Result<Option<i64>, Value> {
    // the model of the pass (a provider's record names none: the chat's, else its default)
    let mut rec = helpers::agent_meta(agent_id, kernel);
    if let Some(m) = model {
        rec.insert("model".into(), json!(m));
    }
    // a turn's output budget is the chat's alone: never the provider's record
    rec.remove("max_tokens");
    if let Some(m) = chat_max {
        rec.insert("max_tokens".into(), json!(m));
    }
    let kept_key = format!(
        "{} {} {} {}",
        agent_id.as_str(),
        rec.get("endpoint").and_then(Value::as_str).unwrap_or(""),
        rec.get("model").and_then(Value::as_str).unwrap_or(""),
        rec.get("thinking_budget").and_then(Value::as_i64).unwrap_or(0)
    );
    if chat_max.is_none() {
        if let Some(n) = read_budgets().lock().expect("budgets poisoned").get(&kept_key) {
            rec.insert("max_tokens".into(), json!(n));
        }
    }
    let pass_max = rec.get("max_tokens").and_then(Value::as_i64).filter(|_| chat_max.is_none());
    let named = |k: &str| {
        rec.get(k)
            .and_then(Value::as_str)
            .is_some_and(|s| !s.trim().is_empty())
    };
    // nothing to read, or nothing to read it with — the provider names a missing
    // `model` / `endpoint` when it is built
    if missing(&rec).is_empty() || !named("model") || !named("endpoint") {
        return Ok(pass_max);
    }
    let key = key_reader(agent_id, kernel, crate::NAME, false);
    let mut values = match read(&rec, &key).await {
        Ok(v) => v,
        Err(e) => {
            // a key the store refuses is permanent too: no retry mends it
            let reason = if e.field == "credentials" {
                fantastic_kernel::refusal::UNAUTHORIZED
            } else {
                fantastic_ai_core::provider::read_reason(e.status)
            };
            let mut r = refusal(reason, e.message);
            with(&mut r, "arg", json!(e.field));
            return Err(r);
        }
    };
    if let Some(bad) = crate::validate(&values, &rec) {
        let first = values.keys().next().cloned().unwrap_or_default();
        let mut r = refusal(
            BAD_ARG,
            format!("the API's values do not fit the record: {bad}"),
        );
        with(&mut r, "arg", json!(first));
        return Err(r);
    }
    let read_max = values.remove("max_tokens").and_then(|v| v.as_i64());
    if let Some(n) = read_max {
        read_budgets().lock().expect("budgets poisoned").insert(kept_key, n);
    }
    if !values.is_empty() {
        let written = kernel.write_own(agent_id, values).await;
        if fantastic_kernel::is_refused(&written) {
            return Err(written);
        }
    }
    Ok(read_max.or(pass_max))
}


