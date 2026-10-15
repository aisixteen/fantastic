//! credential_store — secrets an agent USES but never keeps.
//!
//! A rule-for-rule port of python's
//! `bundled_agents/security/credential_store/src/credential_store/tools.py`,
//! which is canonical — the verbs, the refusals and their texts, the record and
//! the on-disk file.
//!
//! A credential is addressed by a REF (`openai/main`) and is bound to ONE HOST: a
//! value plus the host it may be sent to. A bundle that needs one names it on its
//! own record (`credentials: {store, ref}`), reads it at USE time and refuses to
//! send it to any other host — so rotating a key is rewriting the ref, and no
//! restart is ever needed.
//!
//! WHERE THE VALUES LIVE — `backend` on the record, one of:
//!   * `file` (the default): a YAML file at an ABSOLUTE path —
//!     `~/.fantastic/credentials/<store id>.yaml` unless `file` names another one
//!     (`/…` or `~…`; a relative path is refused at the write, and at use when a
//!     record carries one past the write). It is written 0600 and READ ONLY IF it
//!     is a regular file owned by this user with no group/other bits — anything
//!     else is refused, loud. It is judged on the ONE open it is read from
//!     ([`fs::read_private`]: `O_NOFOLLOW`, then `fstat` of that descriptor), so no
//!     swap can land between the check and the read. The document is
//!     `{refs: {ref: {host, fields: {field: value}}}}`, keys
//!     sorted, block style, written through the kernel's own YAML writer so python
//!     reads it and it reads python's.
//!   * `keychain`: the OS keychain ([`keychain::OsKeychain`]), one item per ref,
//!     under the namespace `keychain_namespace` (default `fantastic` — macOS calls
//!     it the item's "service"), account `<store id>/<ref>`, `{host, fields}` as a
//!     JSON object. A platform with no keychain in this runtime fails loud naming it.
//!   * `env`: read-only; each ref maps its fields to environment variables
//!     (`refs: {ref: {host, env: {api_key: VAR}}}`), for a container or CI.
//!
//! THE HOST IS STORED WITH THE VALUE: `set` writes the ref's
//! declared host beside its fields — the file entry and the keychain item are
//! `{host, fields}` — and `get` refuses a value whose stored host is not the one its
//! declaration names, or that carries none: a store elsewhere declaring the same id
//! and ref with another host reads nothing. The `env` backend keeps no value at rest.
//!
//! A REF IS DECLARED ONLY BY `update_agent` on the store's own record — by the
//! store's parent or the operator, the one path:
//! `refs: {<ref>: {host, fields?/env?}}` (it
//! replaces the whole map; `host` is required — the binding). The DECLARATION is
//! the operator's and only `update_agent` writes or removes it:
//! `set` stores the value of a DECLARED ref, never creates one and
//! takes no `host` (an undeclared ref answers "no such ref", to the operator too;
//! a `host` on the call is refused); `forget` drops the value and keeps the
//! declaration. Any caller the kernel's call gate admits may `set`, `get` and
//! `forget` a declared ref — there are no per-ref access lists. `fields` on a ref
//! are the stored value's NAMES: `set` writes them, `forget` drops them.
//!
//! THE RECORD holds only metadata — the host and the field NAMES. The CALL
//! carries the value: a `set` payload and a `get` reply are recorded like any
//! call's (the ledger, the memory store, the state stream), in clear.
//!
//! WHO MAY CALL is the kernel's call gate alone.
//!
//! A python RAISE is a bundle error rendered `"<Class>: <message>"`, which the
//! kernel settles `failed` with `raised: true` — exactly the record python's
//! dispatch makes of an exception.

#![deny(missing_docs)]

pub mod keychain;
mod sys;
pub mod test_support;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fantastic_bundle::{py_json_dumps, py_repr, py_repr_value, py_str, py_strip, py_truthy};
use fantastic_file::fs;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{missing, refusal, with, BAD_ARG, NOT_FOUND, UNHANDLED};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::keychain::{Keychain, OsKeychain};

/// The `handler_module` this bundle answers to — python's package path.
pub const HANDLER_MODULE: &str = "credential_store.tools";

/// The short orientation readme — python's, verbatim.
pub const README: &str = include_str!("readme.md");

const NAME: &str = "credential_store";
const BACKENDS: [&str; 3] = ["file", "keychain", "env"];
const DEFAULT_DIR: &str = "~/.fantastic/credentials";
const KEYCHAIN_NAMESPACE: &str = "fantastic";

/// What `reflect` says this agent IS — python's `sentence`, verbatim.
pub const SENTENCE: &str = "Credential store — secrets by ref, each bound to one host.";

/// Each verb's doc line — the FIRST line of python's docstring, in `VERBS` order.
const VERBS: [(&str, &str); 6] = [
    (
        "reflect",
        "Identity + posture: the backend, where the file lives, how many refs; never a \
         value or a ref name. No args.",
    ),
    (
        "boot",
        "No-op: values are read from the backend when asked for.",
    ),
    (
        "set",
        "args: ref:str, fields:{name: value} (req). Store or rotate the value of a DECLARED \
         ref — its host is the declaration's (refs are declared by update_agent on this \
         store's record; set never creates one and takes no host). Returns {ref, host, \
         fields:[names], stored}.",
    ),
    (
        "get",
        "args: ref:str (req), host:str? (where you will send it — refused unless the ref is \
         bound to it). The credential: {ref, host, fields:{name: value}}. Send the value \
         ONLY to `host`.",
    ),
    (
        "list",
        "No args. The declared refs — metadata only: {refs:[{ref, host, fields:[names]}]}. \
         Never a value.",
    ),
    (
        "forget",
        "args: ref:str (req). Drop a ref's VALUE from the backend; its declaration (host) \
         stays — update_agent alone undeclares a ref. Returns {forgotten, ref}.",
    ),
];

// ─── python raises ─────────────────────────────────────────────────

/// A python exception out of a verb: `"<Class>: <message>"`.
fn raise(class: &str, msg: impl std::fmt::Display) -> BundleError {
    format!("{class}: {msg}").into()
}

/// A failure out of the `fs` surface, as the exception python's `fs` raises: an
/// `OSError` leads with `[Errno N]` (its class follows from the errno), a utf-8
/// decode is `UnicodeDecodeError`, a clamp refusal is the `ValueError`.
fn raise_fs(msg: String) -> BundleError {
    let class = if msg.starts_with("'utf-8' codec") {
        "UnicodeDecodeError"
    } else {
        msg.strip_prefix("[Errno ")
            .and_then(|r| r.split(']').next())
            .and_then(|n| n.parse::<i32>().ok())
            .map_or("ValueError", fantastic_bundle::os_error_class)
    };
    raise(class, msg)
}

fn record(kernel: &Kernel, id: &AgentId) -> Map<String, Value> {
    kernel
        .agents
        .get(id)
        .map(|a| a.meta.read().expect("meta poisoned").clone())
        .unwrap_or_default()
}

fn refs_of(rec: &Map<String, Value>) -> Map<String, Value> {
    rec.get("refs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// One ref's grant entry as a map (python indexes it with `.get`).
fn entry_map(entry: &Value) -> Map<String, Value> {
    entry.as_object().cloned().unwrap_or_default()
}

fn no_such_ref(r: &str) -> Value {
    let mut out = refusal(NOT_FOUND, format!("{NAME}: no such ref {}", py_repr(r)));
    with(&mut out, "arg", json!("ref"));
    out
}

/// The `netloc` python's `urllib.parse.urlsplit` finds in `url`: the scheme is
/// taken only when it is a valid one (an ASCII letter, then letters, digits,
/// `+-.`), and the netloc is what follows `//` up to the first `/?#`.
fn netloc(url: &str) -> String {
    let url: String = url
        .trim_start_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();
    let mut rest = url.as_str();
    if let Some(i) = url.find(':') {
        let scheme = &url[..i];
        if scheme
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic())
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        {
            rest = &url[i + 1..];
        }
    }
    match rest.strip_prefix("//") {
        Some(r) => {
            let end = r.find(['/', '?', '#']).unwrap_or(r.len());
            r[..end].to_string()
        }
        None => String::new(),
    }
}

/// A host as stored and compared: the `host[:port]` of a URL or of a bare host,
/// lowercased. Python `_host`.
fn host(h: Option<&Value>) -> Option<String> {
    let s = py_strip(h?.as_str()?);
    if s.is_empty() {
        return None;
    }
    let s = if s.contains("://") {
        netloc(s)
    } else {
        s.to_string()
    };
    let s = s.to_lowercase();
    (!s.is_empty()).then_some(s)
}

/// The record's `backend`, python `agent.record.get("backend") or "file"`.
fn backend_kind(rec: &Map<String, Value>) -> String {
    match rec.get("backend") {
        Some(v) if py_truthy(v) => py_str(v),
        _ => "file".to_string(),
    }
}

// ─── where the values live ───────────────────────────────────────

/// python `os.path.expanduser` (what `Path.expanduser` applies): a leading `~`
/// is `$HOME` (the password database when unset), `~user` that user's home.
fn expanduser(p: &str) -> Result<String, BundleError> {
    if !p.starts_with('~') {
        return Ok(p.to_string());
    }
    let i = p[1..].find('/').map_or(p.len(), |j| j + 1);
    let home = if i == 1 {
        match std::env::var("HOME") {
            Ok(h) => Some(h),
            Err(_) => sys::home_of(None),
        }
    } else {
        sys::home_of(Some(&p[1..i]))
    };
    let Some(home) = home else {
        // pathlib raises when nothing expanded the `~`.
        return Err(raise("RuntimeError", "Could not determine home directory."));
    };
    let trimmed = home.trim_end_matches('/');
    let out = format!("{trimmed}{}", &p[i..]);
    Ok(if out.is_empty() { "/".to_string() } else { out })
}

/// pathlib's split of a path into (parent, name): empty and `.` parts dropped,
/// `..` kept, the parent of a bare name `.` (or `/` under the root).
fn split_path(p: &str) -> (String, String) {
    let absolute = p.starts_with('/');
    let mut parts: Vec<&str> = p
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    let name = parts.pop().unwrap_or("").to_string();
    let joined = parts.join("/");
    let parent = match (absolute, joined.is_empty()) {
        (true, true) => "/".to_string(),
        (true, false) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    };
    (parent, name)
}

/// python `sort_keys=True`: every mapping's keys in order, all the way down.
fn sorted(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                out.insert(k.clone(), sorted(&m[k]));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(sorted).collect()),
        other => other.clone(),
    }
}

/// THE host-mismatch refusal: the ref is bound to `bound`, not `other`. One shape
/// for a `get {ref, host}` naming another host and a value stored for another
/// host — or for none. Python `_host_mismatch`.
fn host_mismatch(r: &str, bound: &Value, other: Option<&str>) -> Value {
    let mut out = refusal(
        BAD_ARG,
        format!(
            "{NAME}: ref {} is bound to host {}, not {} — refused, nothing read",
            py_repr(r),
            py_repr_value(bound),
            other.map_or_else(|| "None".to_string(), py_repr)
        ),
    );
    with(&mut out, "arg", json!("host"));
    out
}

/// The refusal for a stored value whose host is not the declared one — or that
/// carries none — or `None`. Python `_host_at_rest`.
fn host_at_rest(r: &str, stored: Option<&Value>, declared: Option<String>) -> Option<Value> {
    let at_rest = host(stored);
    if at_rest.is_some() && at_rest == declared {
        return None;
    }
    let bound = declared.map_or(Value::Null, Value::String);
    Some(host_mismatch(r, &bound, at_rest.as_deref()))
}

/// The refusal for a `file` that is not absolute (`/…` or `~…`), or `None`.
/// Python `_relative_file`.
fn relative_file(p: &str) -> Option<String> {
    if p.starts_with('/') || p.starts_with('~') {
        return None;
    }
    Some(format!(
        "file={} must be an absolute path (starting with / or ~) — a relative path is refused",
        py_repr(p)
    ))
}

/// The default backend: one YAML document of
/// `{refs: {ref: {host, fields: {field: value}}}}`, at an absolute path.
struct FileBackend {
    /// The directory the absolute (or `~`-expanded) `file` names.
    base: PathBuf,
    name: String,
    shown: String,
}

impl FileBackend {
    fn new(id: &AgentId, rec: &Map<String, Value>) -> Result<Self, BundleError> {
        let p = match rec.get("file") {
            Some(v) if py_truthy(v) => py_str(v),
            _ => format!("{DEFAULT_DIR}/{}.yaml", id.0),
        };
        if let Some(why) = relative_file(&p) {
            // a record written past `validate_meta`
            return Err(raise("ValueError", format!("{NAME}: {why}")));
        }
        let (parent, name) = split_path(&expanduser(&p)?);
        Ok(Self {
            base: PathBuf::from(parent),
            name,
            shown: p,
        })
    }

    fn load(&self) -> Result<Map<String, Value>, BundleError> {
        if !fs::exists(&self.base, &self.name).map_err(raise_fs)? {
            return Ok(Map::new());
        }
        // ONE open, judged on its own descriptor (`fs::read_private`): checking the
        // path and then reading the path again leaves a window a swapped leaf
        // slips through.
        let raw = match fs::read_private(&self.base, &self.name) {
            Ok(raw) => raw,
            Err(fs::PrivateReadError::NotPrivate(np)) => {
                return Err(raise(
                    "PermissionError",
                    format!(
                        "{NAME}: credential file {} must be a regular file owned by this user \
                         with mode 0600 (is 0o{:o}, uid {}) — refused, nothing read",
                        self.shown, np.mode, np.uid
                    ),
                ));
            }
            Err(fs::PrivateReadError::Os(e)) => return Err(raise_fs(e)),
        };
        let text = String::from_utf8(raw).map_err(|e| {
            raise_fs(fantastic_bundle::utf8_decode_error_str(
                e.as_bytes(),
                &e.utf8_error(),
            ))
        })?;
        let doc = fantastic_kernel::yaml_io::load(&text).map_err(|e| raise("YAMLError", e))?;
        Ok(doc
            .as_object()
            .and_then(|d| d.get("refs"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

    /// The ref's entry at rest, `{host, fields}`.
    fn get(&self, r: &str) -> Result<Option<Map<String, Value>>, BundleError> {
        Ok(self.load()?.get(r).and_then(Value::as_object).cloned())
    }

    fn save(&self, refs: Map<String, Value>) -> Result<(), BundleError> {
        let mut doc = Map::new();
        doc.insert("refs".to_string(), Value::Object(refs));
        let text = fantastic_kernel::yaml_io::dump(&sorted(&Value::Object(doc)));
        fs::write_bytes(&self.base, &self.name, text.as_bytes(), true, Some(0o600))
            .map_err(raise_fs)
    }

    fn put(&self, r: &str, at_rest: Value) -> Result<(), BundleError> {
        let mut refs = self.load()?;
        refs.insert(r.to_string(), at_rest);
        self.save(refs)
    }

    fn drop_ref(&self, r: &str) -> Result<(), BundleError> {
        let mut refs = self.load()?;
        // python `refs.pop(ref, None) is not None` — a stored null is "absent"
        if refs.shift_remove(r).is_some_and(|v| !v.is_null()) {
            self.save(refs)?;
        }
        Ok(())
    }
}

/// The OS keychain: one item per ref, `{host, fields}` as a JSON object.
struct KeychainBackend {
    kc: Arc<dyn Keychain>,
    /// The record's `keychain_namespace` — the keychain's own word for it is the
    /// item's "service".
    namespace: String,
    prefix: String,
}

/// A keychain failure, raised as python's `keyring` errors are.
fn raise_keychain(e: String) -> BundleError {
    if e.starts_with("credential_store: the keychain backend has no OS keychain") {
        raise("RuntimeError", e)
    } else {
        raise("KeyringError", e)
    }
}

impl KeychainBackend {
    fn new(kc: Arc<dyn Keychain>, id: &AgentId, rec: &Map<String, Value>) -> Self {
        let namespace = match rec.get("keychain_namespace") {
            Some(v) if py_truthy(v) => py_str(v),
            _ => KEYCHAIN_NAMESPACE.to_string(),
        };
        Self {
            kc,
            namespace,
            prefix: format!("{}/", id.0),
        }
    }

    /// The ref's item, `{host, fields}`.
    fn get(&self, r: &str) -> Result<Option<Map<String, Value>>, BundleError> {
        let account = format!("{}{r}", self.prefix);
        let Some(raw) = self
            .kc
            .get_password(&self.namespace, &account)
            .map_err(raise_keychain)?
        else {
            return Ok(None);
        };
        let v: Value = serde_json::from_str(&raw).map_err(|e| raise("JSONDecodeError", e))?;
        Ok(v.as_object().cloned())
    }

    fn put(&self, r: &str, at_rest: Value) -> Result<(), BundleError> {
        let account = format!("{}{r}", self.prefix);
        self.kc
            .set_password(
                &self.namespace,
                &account,
                &py_json_dumps(&at_rest, true, None),
            )
            .map_err(raise_keychain)
    }

    fn drop_ref(&self, r: &str) -> Result<(), BundleError> {
        let account = format!("{}{r}", self.prefix);
        if self
            .kc
            .get_password(&self.namespace, &account)
            .map_err(raise_keychain)?
            .is_some()
        {
            self.kc
                .delete_password(&self.namespace, &account)
                .map_err(raise_keychain)?;
        }
        Ok(())
    }
}

/// Read-only: each ref's fields come from the environment variables its record
/// entry names (`env: {field: VAR}`). No value at rest, so no host at rest.
fn env_get(rec: &Map<String, Value>, r: &str) -> Result<Option<Map<String, Value>>, BundleError> {
    let refs = refs_of(rec);
    let names = refs
        .get(r)
        .and_then(Value::as_object)
        .and_then(|e| e.get("env"))
        .and_then(Value::as_object);
    let Some(names) = names.filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    let mut out = Map::new();
    for (field, var) in names {
        let var = py_str(var);
        match std::env::var(&var) {
            Ok(val) if !val.is_empty() => {
                out.insert(field.clone(), Value::String(val));
            }
            _ => {
                return Err(raise(
                    "LookupError",
                    format!(
                        "{NAME}: ref {} reads {} from ${var}, which is unset",
                        py_repr(r),
                        py_repr(field)
                    ),
                ));
            }
        }
    }
    Ok(Some(out))
}

/// The record's backend, built for one call — python `_backend(agent)`.
enum Backend {
    File(FileBackend),
    Keychain(KeychainBackend),
    Env(Map<String, Value>),
}

impl Backend {
    fn get(&self, r: &str) -> Result<Option<Map<String, Value>>, BundleError> {
        match self {
            Self::File(b) => b.get(r),
            Self::Keychain(b) => b.get(r),
            Self::Env(rec) => env_get(rec, r),
        }
    }

    /// Store `fields` WITH `host`: the value at rest is
    /// `{host, fields}` — python's `put(ref, host, fields)`.
    fn put(
        &self,
        r: &str,
        host: Option<String>,
        fields: Map<String, Value>,
    ) -> Result<(), BundleError> {
        let at_rest = json!({"host": host, "fields": fields});
        match self {
            Self::File(b) => b.put(r, at_rest),
            Self::Keychain(b) => b.put(r, at_rest),
            // python's `_Env` has no `put`; `set` refuses the env backend first.
            Self::Env(_) => Err(raise(
                "AttributeError",
                "'_Env' object has no attribute 'put'",
            )),
        }
    }

    fn drop_ref(&self, r: &str) -> Result<(), BundleError> {
        match self {
            Self::File(b) => b.drop_ref(r),
            Self::Keychain(b) => b.drop_ref(r),
            Self::Env(_) => Err(raise(
                "AttributeError",
                "'_Env' object has no attribute 'drop'",
            )),
        }
    }
}

// ─── the bundle ────────────────────────────────────────────────────

/// The `credential_store.tools` bundle. It holds the keychain it talks to — the
/// OS one by default ([`Self::default`]), an in-memory double in tests
/// ([`Self::with_keychain`]).
pub struct CredentialStoreBundle {
    keychain: Arc<dyn Keychain>,
    /// ONE WRITE AT A TIME. `set`/`forget` read the backend and the record,
    /// change them and write both back; python runs that with no await in
    /// between, so it is atomic on its event loop. Dispatches here run on many
    /// threads, so the same read-modify-write is serialized explicitly — two
    /// rotations must not lose one another's ref.
    writes: std::sync::Mutex<()>,
}

impl Default for CredentialStoreBundle {
    fn default() -> Self {
        Self::with_keychain(Arc::new(OsKeychain))
    }
}

impl CredentialStoreBundle {
    /// A store whose `keychain` backend talks to `keychain` — the tests' double.
    #[must_use]
    pub fn with_keychain(keychain: Arc<dyn Keychain>) -> Self {
        Self {
            keychain,
            writes: std::sync::Mutex::new(()),
        }
    }

    fn backend(&self, id: &AgentId, rec: &Map<String, Value>) -> Result<Backend, BundleError> {
        match backend_kind(rec).as_str() {
            "file" => Ok(Backend::File(FileBackend::new(id, rec)?)),
            "keychain" => Ok(Backend::Keychain(KeychainBackend::new(
                Arc::clone(&self.keychain),
                id,
                rec,
            ))),
            "env" => Ok(Backend::Env(rec.clone())),
            other => Err(raise("KeyError", py_repr(other))),
        }
    }

    fn reflect(id: &AgentId, rec: &Map<String, Value>) -> Value {
        let backend = backend_kind(rec);
        let mut out = Map::new();
        out.insert("id".into(), json!(id.0));
        out.insert("sentence".into(), json!(SENTENCE));
        out.insert("backend".into(), json!(backend));
        out.insert("refs".into(), json!(refs_of(rec).len()));
        if backend == "file" {
            let file = match rec.get("file") {
                Some(v) if py_truthy(v) => v.clone(),
                _ => json!(format!("{DEFAULT_DIR}/{}.yaml", id.0)),
            };
            out.insert("file".into(), file);
        }
        Value::Object(out)
    }

    fn set(&self, id: &AgentId, payload: &Value, kernel: &Kernel) -> Result<Value, BundleError> {
        let Some(r) = payload
            .get("ref")
            .and_then(Value::as_str)
            .filter(|s| !py_strip(s).is_empty())
        else {
            return Ok(missing(&format!("{NAME} set"), "ref"));
        };
        if payload.get("host").is_some() {
            // the binding is the declaration's alone
            let mut out = refusal(
                BAD_ARG,
                format!(
                    "{NAME} set: takes no host — a ref's host is its declaration, written by \
                     update_agent on this store's refs"
                ),
            );
            with(&mut out, "arg", json!("host"));
            return Ok(out);
        }
        let fields = payload
            .get("fields")
            .and_then(Value::as_object)
            .filter(|f| {
                !f.is_empty()
                    && f.iter()
                        .all(|(k, v)| !k.is_empty() && v.as_str().is_some_and(|s| !s.is_empty()))
            });
        let Some(fields) = fields.cloned() else {
            let mut out = refusal(
                BAD_ARG,
                format!("{NAME} set: fields must be a non-empty map of field name to string value"),
            );
            with(&mut out, "arg", json!("fields"));
            return Ok(out);
        };
        let rec = record(kernel, id);
        if backend_kind(&rec) == "env" {
            let mut out = refusal(
                BAD_ARG,
                format!(
                    "{NAME} set: the env backend is read-only — set the environment variable and \
                     name it in the ref's `env`"
                ),
            );
            with(&mut out, "arg", json!("backend"));
            return Ok(out);
        }
        let mut refs = refs_of(&rec);
        // declared by update_agent alone — set never makes one
        let Some(mut entry) = refs.get(r).filter(|v| !v.is_null()).map(entry_map) else {
            return Ok(no_such_ref(r));
        };
        let h = host(entry.get("host"));
        // the declared host is stored WITH the value
        let mut sorted_names: Vec<String> = fields.keys().cloned().collect();
        sorted_names.sort();
        self.backend(id, &rec)?.put(r, h.clone(), fields)?;
        // the value's NAMES, never a value
        entry.insert("fields".into(), json!(sorted_names));
        refs.insert(r.to_string(), Value::Object(entry));
        let mut patch = Map::new();
        patch.insert("refs".into(), Value::Object(refs));
        kernel.update(id, patch);
        Ok(json!({"ref": r, "host": h, "fields": sorted_names, "stored": true}))
    }

    fn get(&self, id: &AgentId, payload: &Value, kernel: &Kernel) -> Result<Value, BundleError> {
        let Some(r) = payload
            .get("ref")
            .and_then(Value::as_str)
            .filter(|s| !py_strip(s).is_empty())
        else {
            return Ok(missing(&format!("{NAME} get"), "ref"));
        };
        let rec = record(kernel, id);
        let refs = refs_of(&rec);
        let Some(entry) = refs.get(r).filter(|v| !v.is_null()).map(entry_map) else {
            return Ok(no_such_ref(r));
        };
        if payload.get("host").is_some_and(|v| !v.is_null()) {
            let want = host(payload.get("host"));
            if want.is_none() || want != host(entry.get("host")) {
                return Ok(host_mismatch(
                    r,
                    entry.get("host").unwrap_or(&Value::Null),
                    want.as_deref(),
                ));
            }
        }
        let backend = self.backend(id, &rec)?;
        let mut fields = backend.get(r)?;
        if let Some(stored) = fields.as_ref().filter(|f| !f.is_empty()) {
            if !matches!(backend, Backend::Env(_)) {
                // THE HOST AT REST: a value is read only for the host it
                // was stored with — the declaration of a store elsewhere sharing
                // this id and ref names its own host, and reads nothing.
                if let Some(bad) = host_at_rest(r, stored.get("host"), host(entry.get("host"))) {
                    return Ok(bad);
                }
                fields = stored.get("fields").and_then(Value::as_object).cloned();
            }
        }
        let Some(fields) = fields.filter(|f| !f.is_empty()) else {
            let mut out = refusal(
                NOT_FOUND,
                format!("{NAME}: ref {} has no value in the backend", py_repr(r)),
            );
            with(&mut out, "arg", json!("ref"));
            return Ok(out);
        };
        Ok(json!({"ref": r, "host": host(entry.get("host")), "fields": fields}))
    }

    fn list(id: &AgentId, kernel: &Kernel) -> Value {
        let mut refs: Vec<(String, Value)> = refs_of(&record(kernel, id)).into_iter().collect();
        refs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Vec::new();
        for (r, entry) in refs {
            let entry = entry_map(&entry);
            let fields = match entry.get("fields") {
                Some(v) if py_truthy(v) => v.clone(),
                _ => {
                    let mut names: Vec<String> = entry
                        .get("env")
                        .and_then(Value::as_object)
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default();
                    names.sort();
                    json!(names)
                }
            };
            let mut row = Map::new();
            row.insert("ref".into(), json!(r));
            row.insert("host".into(), json!(host(entry.get("host"))));
            row.insert("fields".into(), fields);
            out.push(Value::Object(row));
        }
        json!({"refs": out})
    }

    fn forget(&self, id: &AgentId, payload: &Value, kernel: &Kernel) -> Result<Value, BundleError> {
        let Some(r) = payload
            .get("ref")
            .and_then(Value::as_str)
            .filter(|s| !py_strip(s).is_empty())
        else {
            return Ok(missing(&format!("{NAME} forget"), "ref"));
        };
        let rec = record(kernel, id);
        if backend_kind(&rec) == "env" {
            let mut out = refusal(
                BAD_ARG,
                format!(
                    "{NAME} forget: the env backend is read-only — unset the environment \
                     variable its ref names"
                ),
            );
            with(&mut out, "arg", json!("backend"));
            return Ok(out);
        }
        let mut refs = refs_of(&rec);
        let Some(mut entry) = refs.get(r).filter(|v| !v.is_null()).map(entry_map) else {
            return Ok(no_such_ref(r));
        };
        self.backend(id, &rec)?.drop_ref(r)?;
        // the value's names go with the value; the declaration stays
        if entry.shift_remove("fields").is_some() {
            refs.insert(r.to_string(), Value::Object(entry));
            let mut patch = Map::new();
            patch.insert("refs".into(), Value::Object(refs));
            kernel.update(id, patch);
        }
        Ok(json!({"forgotten": true, "ref": r}))
    }
}

/// `refs[...]` as python's `{ref!r}` renders it in a refusal.
fn ref_label(r: &str) -> String {
    format!("refs[{}]", py_repr(r))
}

fn is_str_list(v: &Value) -> bool {
    v.as_array()
        .is_some_and(|a| a.iter().all(|x| x.as_str().is_some_and(|s| !s.is_empty())))
}

#[async_trait]
impl Bundle for CredentialStoreBundle {
    /// The CATALOG name — python's entry-point name.
    fn name(&self) -> &str {
        NAME
    }

    fn verb_docs(&self) -> Value {
        let mut verbs = Map::new();
        for (n, doc) in VERBS {
            verbs.insert(n.to_string(), json!(doc));
        }
        Value::Object(verbs)
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// The record's own fields, refused at the write when malformed — python
    /// `validate_meta`, check for check and text for text.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        if let Some(b) = meta.get("backend").filter(|v| !v.is_null()) {
            if !b.as_str().is_some_and(|s| BACKENDS.contains(&s)) {
                return Some(format!(
                    "backend={} is not one of {}",
                    py_repr_value(b),
                    BACKENDS.join(", ")
                ));
            }
        }
        for k in ["file", "keychain_namespace"] {
            if let Some(v) = meta.get(k).filter(|v| !v.is_null()) {
                if !v.as_str().is_some_and(|s| !py_strip(s).is_empty()) {
                    return Some(format!("{k} must be a non-empty string"));
                }
            }
        }
        if let Some(why) = meta
            .get("file")
            .and_then(Value::as_str)
            .and_then(relative_file)
        {
            return Some(why);
        }
        let refs = meta.get("refs").filter(|v| !v.is_null())?;
        let Some(refs) = refs.as_object() else {
            return Some("refs must be a map of ref to {host, fields, env}".into());
        };
        for (r, e) in refs {
            if py_strip(r).is_empty() {
                return Some("a ref must be a non-empty string".to_string());
            }
            let Some(e) = e.as_object() else {
                return Some(format!("{} must be a map", ref_label(r)));
            };
            if let Some(v) = e.get("fields").filter(|v| !v.is_null()) {
                if !is_str_list(v) {
                    return Some(format!("{}.fields must be a list of strings", ref_label(r)));
                }
            }
            if !e.get("host").is_some_and(|v| !v.is_null()) {
                return Some(format!(
                    "{}.host is required — the one host this ref's value may be sent to",
                    ref_label(r)
                ));
            }
            if host(e.get("host")).is_none() {
                return Some(format!("{}.host must be a non-empty host", ref_label(r)));
            }
            if let Some(env) = e.get("env").filter(|v| !v.is_null()) {
                let ok = env.as_object().is_some_and(|m| {
                    m.values()
                        .all(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                });
                if !ok {
                    return Some(format!(
                        "{}.env must map field names to variable names",
                        ref_label(r)
                    ));
                }
            }
        }
        None
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let out = match verb {
            "reflect" => Self::reflect(agent_id, &record(kernel, agent_id)),
            // No-op: values are read from the backend when asked for.
            "boot" => return Ok(None),
            "set" => {
                let _one = self.writes.lock().unwrap_or_else(|p| p.into_inner());
                self.set(agent_id, payload, kernel)?
            }
            "get" => self.get(agent_id, payload, kernel)?,
            "list" => Self::list(agent_id, kernel),
            "forget" => {
                let _one = self.writes.lock().unwrap_or_else(|p| p.into_inner());
                self.forget(agent_id, payload, kernel)?
            }
            _ => {
                let t = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut r = refusal(
                    UNHANDLED,
                    format!("{NAME}: unknown type {}", py_repr_value(&t)),
                );
                with(&mut r, "verb", t);
                r
            }
        };
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests;
