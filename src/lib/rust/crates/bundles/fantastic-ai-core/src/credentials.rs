//! A backend's KEY, read from a credential store at USE time. Python
//! `ai_core/credentials.py`, text for text.
//!
//! The record names WHERE the key lives — `credentials: {store, ref}` — never the
//! value, and nothing reads the environment. Before EVERY request the verb that makes
//! it reads the key, pinned to the endpoint's host (the store refuses a ref bound to
//! another host): the read is ONE CALL TO THE STORE, A STEP (#795) — its settle runs
//! the request with the key read ([`KeyReader::key`], a [`ReadKey`]), nothing waits for
//! the store. The read is this agent's call, gated as it. Nothing is cached: a key
//! rotated in the store is used by the next request, with no restart.
//!
//! No `credentials` on the record ⇒ no key — for a keyless local server. A
//! backend that cannot run without one marks the field required in its record
//! schema, and the reader refuses its absence loud as well.
//!
//! Wiring: the agent's own line on the store, `can: {<store>: {call: [get]}}`.

use std::sync::Arc;

use fantastic_bundle::py_repr;
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
            // a key that cannot be read is not read on the next try either
            class: crate::provider::ErrorClass::Permanent(fantastic_kernel::refusal::UNAUTHORIZED),
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

/// What a provider reads its key from before EACH request — python's `key` callable,
/// `async key(endpoint) -> str | None`. [`ReadKey`] is the one the backends build (the
/// key a [`KeyReader`] step read from the store for this request); a provider test
/// hands in its own.
#[async_trait]
pub trait KeySource: Send + Sync {
    /// The key for a request to `endpoint`, or `None` for no key at all.
    ///
    /// # Errors
    /// [`CredentialError`] — the turn fails, naming why.
    async fn key(&self, endpoint: &str) -> Result<Option<String>, CredentialError>;
}

/// THE KEY OF ONE REQUEST, read before it ([`KeyReader::key`]): the provider takes it
/// for the endpoint it was read for (its host), and refuses another host by name.
#[derive(Debug, Clone)]
pub struct ReadKey {
    host: String,
    key: Result<Option<String>, CredentialError>,
}

impl ReadKey {
    /// The key `key` read for `endpoint` (no read at all: `Ok(None)`).
    #[must_use]
    pub fn new(endpoint: &str, key: Result<Option<String>, CredentialError>) -> Self {
        Self {
            host: host_of(endpoint).to_string(),
            key,
        }
    }
}

/// The host of `url` (`scheme://host[:port]/…`), lower case.
fn host_of(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

#[async_trait]
impl KeySource for ReadKey {
    async fn key(&self, endpoint: &str) -> Result<Option<String>, CredentialError> {
        if host_of(endpoint) != self.host {
            return Err(CredentialError(format!(
                "the key was read for {}, not for {}",
                self.host,
                host_of(endpoint)
            )));
        }
        self.key.clone()
    }
}

/// The reader of an agent's key — python `key_reader`'s `read` closure, as a STEP
/// (#795). Cheap to clone; it holds the agent, the kernel and what the refusals are
/// prefixed with.
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
    /// THE KEY OF A REQUEST TO `endpoint`, as a STEP: `next` gets it as a [`ReadKey`]
    /// pinned to that endpoint's host — `None` in it when the record names no
    /// `credentials` and the key is optional, a [`CredentialError`] the provider says
    /// when it would use it. Read from the record and the store at every call — never
    /// cached.
    pub async fn key<R, F, Fut>(&self, endpoint: &str, rec: R, next: F)
    where
        R: Send + 'static,
        F: FnOnce(R, Arc<dyn KeySource>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let at = endpoint.to_string();
        self.read(endpoint, rec, move |rec, key| next(rec, Arc::new(ReadKey::new(&at, key)) as Arc<dyn KeySource>))
            .await;
    }

    /// The `api_key` of a request to `endpoint`, as a STEP: `next` gets it, or `None`
    /// when the record names no `credentials` and the key is optional, or the
    /// [`CredentialError`] — the store refuses, the field is missing or not a legal
    /// header value, the record's `credentials` is malformed, or a required key has no
    /// `credentials` on the record.
    pub async fn read<R, F, Fut>(&self, endpoint: &str, rec: R, next: F)
    where
        R: Send + 'static,
        F: FnOnce(R, Result<Option<String>, CredentialError>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let backend = self.backend;
        self.read_ref(endpoint, &[FIELD], rec, move |rec, r| {
            let key = r.and_then(|got| {
                let Some((store, r, mut values)) = got else {
                    return Ok(None);
                };
                let key = values.remove(0);
                // named by its ref, never by its value
                if !header_safe(&key) {
                    return Err(CredentialError(format!(
                        "{backend}: credential {} in {}: {} is not a legal HTTP header value (a \
                         control or non-ASCII character, or whitespace at an edge) — store it \
                         again without",
                        py_repr(&r),
                        py_repr(&store),
                        py_repr(FIELD)
                    )));
                }
                Ok(Some(key))
            });
            next(rec, key)
        })
        .await;
    }

    /// The values of the fields `names` of the ref this agent's `credentials` names,
    /// in the order asked, as a STEP: `next` gets them, or `None` when the record names
    /// no `credentials` and they are optional. One `get` to the store, pinned to
    /// `endpoint`'s host (the store refuses a ref bound to another host), sent as this
    /// agent; nothing is cached. The [`CredentialError`]: the store refuses, the
    /// record's `credentials` is malformed, or a required one is absent; a named field
    /// that is absent or empty names the backend, the ref, the store and the FIELD
    /// NAME — never a value.
    pub async fn read_fields<R, F, Fut>(&self, endpoint: &str, names: &[&str], rec: R, next: F)
    where
        R: Send + 'static,
        F: FnOnce(R, Result<Option<Vec<String>>, CredentialError>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.read_ref(endpoint, names, rec, move |rec, r| next(rec, r.map(|got| got.map(|(_, _, v)| v))))
            .await;
    }

    /// TESTS: [`Self::read`] run to its end, its key handed back here.
    #[cfg(test)]
    pub(crate) async fn read_now(&self, endpoint: &str) -> Result<Option<String>, CredentialError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.read(endpoint, tx, |tx, r| async move {
            let _ = tx.send(r);
        })
        .await;
        rx.await.expect("the read ran its next step")
    }

    /// TESTS: [`Self::read_fields`] run to its end, its values handed back here.
    #[cfg(test)]
    pub(crate) async fn read_fields_now(&self, endpoint: &str, names: &[&str]) -> Result<Option<Vec<String>>, CredentialError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.read_fields(endpoint, names, tx, |tx, r| async move {
            let _ = tx.send(r);
        })
        .await;
        rx.await.expect("the read ran its next step")
    }

    /// [`Self::read_fields`] with the store id and the ref it read, for the
    /// readers that name them in their own texts.
    async fn read_ref<R, F, Fut>(&self, endpoint: &str, names: &[&str], rec: R, next: F)
    where
        R: Send + 'static,
        F: FnOnce(R, Result<Option<(String, String, Vec<String>)>, CredentialError>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let backend = self.backend;
        let id = self.self_id.as_str();
        let cred = crate::helpers::agent_meta(&self.self_id, &self.kernel)
            .get("credentials")
            .cloned()
            .filter(|v| !v.is_null());
        let Some(cred) = cred else {
            if self.required {
                return next(
                    rec,
                    Err(CredentialError(format!(
                        "{backend}: no `credentials` on {id} — name the key's credential store and \
                         ref (update_agent {id} credentials={{store: <id>, ref: <ref>}})"
                    ))),
                )
                .await;
            }
            return next(rec, Ok(None)).await;
        };
        let store = cred["store"].as_str().unwrap_or("").to_string();
        let r = cred["ref"].as_str().unwrap_or("").to_string();
        // THIS AGENT'S call: the read is a step of its own verb, gated as it; its settle
        // is the next step
        let item = Item::new(
            AgentId::from(store.as_str()),
            json!({"type": "get", "ref": r, "host": endpoint}),
        );
        let names: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
        crate::steps::call(&self.kernel, item, rec, move |rec, out| async move {
            next(rec, fields_of(backend, &store, &r, &names, &out)).await;
        })
        .await;
    }
}

/// The store's answer to a `get` of `names`, checked: the values in the order asked.
fn fields_of(
    backend: &str,
    store: &str,
    r: &str,
    names: &[String],
    out: &fantastic_kernel::Outcome,
) -> Result<Option<(String, String, Vec<String>)>, CredentialError> {
    let res = &out.result;
    let failed = out.status != Status::Completed
        || !res.is_object()
        || res.get("error").is_some_and(|e| !e.is_null());
    if failed {
        let err = fantastic_kernel::refusal::error_text(res)
            .unwrap_or_else(|| out.status.as_str().to_string());
        return Err(CredentialError(format!(
            "{backend}: credentials {store}/{r}: {err}"
        )));
    }
    let mut values = Vec::with_capacity(names.len());
    for name in names {
        match res.get("fields").and_then(|f| f.get(name.as_str())) {
            Some(Value::String(v)) if !v.is_empty() => values.push(v.clone()),
            _ => {
                return Err(CredentialError(format!(
                    "{backend}: credential {} in {} has no {} field",
                    py_repr(r),
                    py_repr(store),
                    py_repr(name)
                )))
            }
        }
    }
    Ok(Some((store.to_string(), r.to_string(), values)))
}
