//! A backend's KEY, read from a credential store at USE time (owner,
//! 2026-10-01). Python `ai_core/credentials.py`, text for text.
//!
//! The record names WHERE the key lives — `credentials: {store, ref}` — never the
//! value, and nothing reads the environment. Before EVERY request a provider
//! calls its reader with the endpoint it is about to call; the reader asks the
//! store AS THIS AGENT for `api_key`, the answer marked secret (the value lands in
//! no row, ledger or stream) and pinned to that endpoint's host (the store
//! refuses a ref bound to another host). Nothing is cached: a key rotated in the
//! store is used by the next request, with no restart.
//!
//! No `credentials` on the record ⇒ no key — for a keyless local server. A
//! backend that cannot run without one marks the field required in its record
//! schema, and the reader refuses its absence loud as well.
//!
//! Wiring, by the operator: the agent's `can_call` names the store, the store's
//! `callable_by` names the agent, and the ref's `readers` name it — the store
//! answers "no such ref" otherwise.
//!
//! A MODEL NEVER RECEIVES A SECRET (owner, 2026-10-02, option B). The grant that
//! lets the backend read its key would let the agent's MODEL send the same `get`
//! and read the value into its prompt and memory. So an agent whose record
//! carries `holds_secrets: true` (a `credential_store` stamps it on itself; an
//! operator may set it on any agent) is unreachable from a model turn:
//! [`crate::route`] — the ONE place a model-parsed call is dispatched — asks
//! [`secret_holder_refusal`] for every call's target and `reply_to` and refuses a
//! holder, loud, before anything is sent. The address is resolved the way the
//! kernel's `send` resolves it first, so `<this kernel's guid>|<holder>` is the
//! holder, not an unknown id. The reader below is untouched: it calls
//! `kernel.send` itself, never through the model's tool dispatch.
//!
//! Nor does a model JOIN a secret (audit F2, 2026-10-03): a model call whose
//! `<exec_id>` names an execution that carried `secret` marks is refused the same
//! way ([`secret_join_refusal`]) — the backend's key read is such an execution,
//! and the kernel hands a join by the SAME caller the real reply.

use std::sync::Arc;

use fantastic_bundle::{py_repr, py_str, py_strip, py_truthy};
use fantastic_kernel::refusal::{refusal, with, UNAUTHORIZED};
use fantastic_kernel::{with_sender, Agent, AgentId, Item, Kernel, Status};
use serde_json::{json, Value};

use crate::provider::ProviderError;
use async_trait::async_trait;

/// The credential field a backend sends as its key.
pub const FIELD: &str = "api_key";

/// The record field that marks an agent a SECRET HOLDER — reachable by backends,
/// never by a model turn.
pub const HOLDS_SECRETS: &str = "holds_secrets";

/// What the model reads when a call of its names a secret holder — python's
/// `SECRET_HOLDER_TEXT.format(agent=…)`, byte for byte.
#[must_use]
pub fn secret_holder_text(agent: &str) -> String {
    format!(
        "{agent} holds secrets — a secret holder answers backends, never a model turn; \
         this call was not dispatched"
    )
}

/// What the model reads when its `<exec_id>` names an execution that carried
/// secret values (a key read, or any call whose item was marked `secret`) —
/// python's `SECRET_JOIN_TEXT.format(exec_id=…)`, byte for byte.
#[must_use]
pub fn secret_join_text(exec_id: &str) -> String {
    format!(
        "exec_id {exec_id} names an execution that carried secret values — a model \
         never joins one; this call was not dispatched"
    )
}

/// The agent (and so the record) `address` names on THIS kernel, resolved the way
/// `send` resolves a target ([`Kernel::resolve_target`], then the kernel's
/// qualified-target step): a local id is itself; the reserved `kernel` (no agent
/// may take that id) is the root; `<this kernel's guid>|<id>` is that local agent
/// (`<this kernel's guid>|kernel` the root). A connected peer's `<guid>|<id>` and
/// an unknown id have no record here: `None`. Python `_local_record`.
fn local_record(kernel: &Kernel, address: &str) -> Option<Arc<Agent>> {
    if let Some(agent) = kernel.resolve_target(address) {
        return Some(agent);
    }
    let (guid, local) = address.split_once('|')?;
    if guid.is_empty() || local.is_empty() || local.contains('|') {
        return None;
    }
    let root = kernel.root()?;
    let own = root
        .meta
        .read()
        .expect("meta poisoned")
        .get("guid")
        .and_then(Value::as_str)
        == Some(guid);
    if !own {
        return None;
    }
    if local == "kernel" {
        Some(root)
    } else {
        kernel
            .agents
            .get(&AgentId::from(local))
            .map(|e| Arc::clone(&e))
    }
}

/// The refusal for a model-parsed call that names `agent_id` (as its target or
/// its `reply_to`), or `None` when the call may go. Record-driven: only an agent
/// whose record says `holds_secrets: true` is refused. Python
/// `secret_holder_refusal`.
///
/// The address is resolved FIRST, as the kernel resolves it ([`local_record`]):
/// `<this kernel's guid>|<holder>` reaches the holder, so it is refused like the
/// bare id. Only what THIS kernel can see is judged: a connected peer's
/// `<guid>|<agent>` lives on another kernel and its record is not here, so it
/// passes; that kernel's own AI layer guards its own holders, a holder's value
/// is only ever sent to a caller that marked it secret, and a model's join of
/// that marked read is [`secret_join_refusal`]'s.
#[must_use]
pub fn secret_holder_refusal(kernel: &Kernel, agent_id: &str) -> Option<Value> {
    let holds = local_record(kernel, agent_id).is_some_and(|a| {
        a.meta.read().expect("meta poisoned").get(HOLDS_SECRETS) == Some(&Value::Bool(true))
    });
    if !holds {
        return None;
    }
    let mut out = refusal(UNAUTHORIZED, secret_holder_text(agent_id));
    with(&mut out, "agent", json!(agent_id));
    Some(out)
}

/// The refusal for a model-parsed call whose `<exec_id>` names an execution that
/// carried `secret` marks, or `None` when the call may go. Python
/// `secret_join_refusal`.
///
/// A JOIN is a receive: the kernel hands the joiner that execution's outcome. A
/// model cannot mark a call secret, so such an execution was never its own — it
/// is its backend's key read (same agent: the kernel would hand back the REAL
/// reply) or another caller's (the kernel hands back the shown one). Either way a
/// model does not join it. This is what closes a holder on a CONNECTED PEER,
/// which [`secret_holder_refusal`] cannot judge: the backend's read of
/// `<peer guid>|creds` is a local execution, and joining it by its id never
/// reaches the peer. The id is resolved as the kernel's `send` resolves it
/// ([`Kernel::carried_secret`]).
#[must_use]
pub fn secret_join_refusal(kernel: &Kernel, exec_id: &Value) -> Option<Value> {
    if !kernel.carried_secret(exec_id) {
        return None;
    }
    let mut out = refusal(UNAUTHORIZED, secret_join_text(&py_str(exec_id)));
    with(&mut out, "exec_id", exec_id.clone());
    Some(out)
}

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
        }
    }
}

/// Whether `key` can ride `Authorization: Bearer <key>` as it is: visible ASCII,
/// spaces or tabs only BETWEEN visible characters. Python `header_safe`: an HTTP
/// client refuses anything else, and python's QUOTES the header it refused in its
/// error — a key pasted with a trailing newline would ride that text into the
/// caller's reply, the ledger and the logs (audit S1).
#[must_use]
pub fn header_safe(key: &str) -> bool {
    key == key.trim_matches(|c| c == ' ' || c == '\t')
        && key
            .chars()
            .all(|c| c == ' ' || c == '\t' || ('!'..='~').contains(&c))
}

/// `text` with every occurrence of `key` replaced by the placeholder a recorded
/// secret reads as. Python `without_key`: a provider HOLDS its key, so every
/// error text it raises passes through here first — an endpoint may echo the
/// request in its error body (audit S1).
#[must_use]
pub fn without_key(text: &str, key: Option<&str>) -> String {
    match key.filter(|k| !k.is_empty()) {
        Some(k) => text.replace(k, fantastic_kernel::secret::PLACEHOLDER),
        None => text.to_string(),
    }
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
        // AS ITSELF: the store's per-ref `readers` name this agent, whatever task
        // the request runs in.
        let item = Item::new(
            AgentId::from(store.as_str()),
            json!({"type": "get", "ref": r, "host": endpoint}),
        )
        .with_secret(json!(["result"]));
        let k = Arc::clone(&self.kernel);
        let out = with_sender(
            self.self_id.clone(),
            async move { k.send(vec![item]).await },
        )
        .await
        .remove(0);
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
