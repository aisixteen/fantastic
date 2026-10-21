//! A backend's KEY, read from a credential store at USE time. Python
//! `ai_core/credentials.py`, text for text.
//!
//! The record names WHERE the key lives — `credentials: {store, ref}` — never the
//! value, and nothing reads the environment. Before EVERY request a provider
//! calls its reader with the endpoint it is about to call; the reader asks the
//! store for `api_key`, pinned to that endpoint's host (the store refuses a ref
//! bound to another host). The request runs in this agent's own turn (its
//! dispatch, or a task it spawned), so the read is this agent's send, gated as
//! it. Nothing is cached: a key rotated in the store is used by the next request,
//! with no restart.
//!
//! No `credentials` on the record ⇒ no key — for a keyless local server. A
//! backend that cannot run without one marks the field required in its record
//! schema, and the reader refuses its absence loud as well.
//!
//! Wiring, by the operator: the agent's `can_call` names the store and the store's
//! `callable_by` names the agent.

use std::sync::Arc;

use fantastic_bundle::{py_repr, py_str, py_strip, py_truthy};
use fantastic_kernel::{AgentId, Item, Kernel, Status};
use serde_json::{json, Value};

use crate::provider::ProviderError;
use async_trait::async_trait;

/// The credential field a backend sends as its key.
pub const FIELD: &str = "api_key";

/// The key could not be read — the turn fails, naming the store and the ref.
/// Python `CredentialError(RuntimeError)`: a raise renders as
/// `CredentialError: <message>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialError(pub String);

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CredentialError {}

impl From<CredentialError> for ProviderError {
    fn from(e: CredentialError) -> Self {
        Self {
            message: e.0,
            kind: "CredentialError",
            transport: false,
        }
    }
}

/// Whether `key` can ride `Authorization: Bearer <key>` as it is: visible ASCII,
/// spaces or tabs only BETWEEN visible characters. Python `header_safe`: an HTTP
/// client refuses anything else, so a key pasted with a trailing newline is
/// refused here, named by its ref.
#[must_use]
pub fn header_safe(key: &str) -> bool {
    key == key.trim_matches(|c| c == ' ' || c == '\t')
        && key
            .chars()
            .all(|c| c == ' ' || c == '\t' || ('!'..='~').contains(&c))
}

/// Why a `credentials` value is malformed, or `None`: a map of exactly a
/// non-empty `store` id and a non-empty `ref`. Python `shape_error`.
#[must_use]
pub fn shape_error(v: &Value) -> Option<String> {
    let Some(m) = v
        .as_object()
        .filter(|m| m.len() == 2 && m.contains_key("store") && m.contains_key("ref"))
    else {
        return Some("credentials must be {store: <credential_store id>, ref: <ref>}".to_string());
    };
    for k in ["store", "ref"] {
        if !m[k].as_str().is_some_and(|s| !py_strip(s).is_empty()) {
            return Some(format!("credentials.{k} must be a non-empty string"));
        }
    }
    None
}

/// What a provider awaits before EACH request — python's `key` callable,
/// `async key(endpoint) -> str | None`. [`KeyReader`] is the one the backends
/// build (the record's `credentials`, read from the store); a provider test hands
/// in its own.
#[async_trait]
pub trait KeySource: Send + Sync {
    /// The key for a request to `endpoint`, or `None` for no key at all.
    ///
    /// # Errors
    /// [`CredentialError`] — the turn fails, naming why.
    async fn key(&self, endpoint: &str) -> Result<Option<String>, CredentialError>;
}

#[async_trait]
impl KeySource for KeyReader {
    async fn key(&self, endpoint: &str) -> Result<Option<String>, CredentialError> {
        self.read(endpoint).await
    }
}

/// The reader a provider awaits before each request — python `key_reader`'s
/// `read` closure. Cheap to clone; it holds the agent, the kernel and what the
/// refusals are prefixed with.
#[derive(Clone)]
pub struct KeyReader {
    self_id: AgentId,
    kernel: Arc<Kernel>,
    backend: &'static str,
    required: bool,
}

impl std::fmt::Debug for KeyReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyReader")
            .field("self_id", &self.self_id)
            .field("backend", &self.backend)
            .field("required", &self.required)
            .finish_non_exhaustive()
    }
}

/// The reader for agent `self_id` of backend `backend` (the refusals' prefix).
/// `required`: a record without `credentials` is refused rather than keyless.
/// Python `key_reader(self_id, kernel, backend, *, required)`.
#[must_use]
pub fn key_reader(
    self_id: &AgentId,
    kernel: &Arc<Kernel>,
    backend: &'static str,
    required: bool,
) -> KeyReader {
    KeyReader {
        self_id: self_id.clone(),
        kernel: Arc::clone(kernel),
        backend,
        required,
    }
}

impl KeyReader {
    /// The key for a request to `endpoint`, or `None` when the record names no
    /// `credentials` and the key is optional. Read from the record and the store
    /// on EVERY call — never cached.
    ///
    /// # Errors
    /// [`CredentialError`] when the store refuses, the field is missing, the
    /// record's `credentials` is malformed, or a required key has no
    /// `credentials` on the record.
    pub async fn read(&self, endpoint: &str) -> Result<Option<String>, CredentialError> {
        let backend = self.backend;
        let id = self.self_id.as_str();
        let cred = crate::helpers::agent_meta(&self.self_id, &self.kernel)
            .get("credentials")
            .cloned()
            .filter(|v| !v.is_null());
        let Some(cred) = cred else {
            if self.required {
                return Err(CredentialError(format!(
                    "{backend}: no `credentials` on {id} — name the key's credential store and \
                     ref (update_agent {id} credentials={{store: <id>, ref: <ref>}})"
                )));
            }
            return Ok(None);
        };
        if let Some(bad) = shape_error(&cred) {
            return Err(CredentialError(format!("{backend}: {bad}")));
        }
        let store = cred["store"].as_str().unwrap_or("").to_string();
        let r = cred["ref"].as_str().unwrap_or("").to_string();
        // THIS AGENT'S send: the read runs inside its own turn (its dispatch, or a
        // task that dispatch spawned), so the store's `callable_by` must name it.
        let item = Item::new(
            AgentId::from(store.as_str()),
            json!({"type": "get", "ref": r, "host": endpoint}),
        );
        let out = self.kernel.send(vec![item]).await.remove(0);
        let res = &out.result;
        let failed = out.status != Status::Completed
            || !res.is_object()
            || res.get("error").is_some_and(py_truthy);
        if failed {
            let err = match res.as_object() {
                Some(m) => m.get("error").map_or_else(|| "None".to_string(), py_str),
                None => out.status.as_str().to_string(),
            };
            return Err(CredentialError(format!(
                "{backend}: credentials {store}/{r}: {err}"
            )));
        }
        match res.get("fields").and_then(|f| f.get(FIELD)) {
            // named by its ref, never by its value
            Some(Value::String(key)) if !key.is_empty() && !header_safe(key) => {
                Err(CredentialError(format!(
                    "{backend}: credential {} in {}: {} is not a legal HTTP header value (a \
                     control or non-ASCII character, or whitespace at an edge) — store it \
                     again without",
                    py_repr(&r),
                    py_repr(&store),
                    py_repr(FIELD)
                )))
            }
            Some(Value::String(key)) if !key.is_empty() => Ok(Some(key.clone())),
            _ => Err(CredentialError(format!(
                "{backend}: credential {} in {} has no {} field",
                py_repr(&r),
                py_repr(&store),
                py_repr(FIELD)
            ))),
        }
    }
}
