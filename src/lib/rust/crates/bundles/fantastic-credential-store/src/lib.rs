//! credential_store — secrets an agent USES but never keeps (owner, 2026-10-01).
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
//!   * `file` (the default): a YAML file in the user's home,
//!     `~/.fantastic/credentials/<store id>.yaml` unless `file` names another
//!     path (a RELATIVE one is clamped under the kernel's running dir). It is
//!     written 0600 and READ ONLY IF it is a regular file owned by this user with
//!     no group/other bits — anything else is refused, loud. The document is
//!     `{refs: {ref: {field: value}}}`, keys sorted, block style, written through
//!     the kernel's own YAML writer so python reads it and it reads python's.
//!   * `keychain`: the OS keychain ([`keychain::OsKeychain`]), one item per ref —
//!     service `keychain_service` (default `fantastic`), account
//!     `<store id>/<ref>`, the fields as a JSON object. A platform with no
//!     keychain in this runtime fails loud naming it.
//!   * `env`: read-only; each ref maps its fields to environment variables
//!     (`refs: {ref: {host, env: {api_key: VAR}}}`), for a container or CI.
//!
//! WHO MAY DO WHAT — per ref, on the store's own record (the OPERATOR manages it
//! with `update_agent`): `refs: {<ref>: {readers: [...], writers: [...], host,
//! fields}}`. An entry is a local agent id, `$subtree:<id>` (that agent and every
//! live descendant), or a qualified `<guid>|<agent>` caller on another kernel.
//! The operator may do everything. An agent listed in `creators` may create a NEW
//! ref, which it then reads and writes. A caller with no grant on a ref is
//! answered "no such ref" — never told the ref exists.
//!
//! NO VALUE IS EVER RECORDED. `set` must be called with `secret:
//! ['payload.fields']` and `get` with `secret: ['result']`
//! ([`fantastic_kernel::secret`]): the call is refused otherwise, because the
//! value would land in the ledger, the memory store and the state stream. The
//! record holds only metadata — the host and the field NAMES.
//!
//! The kernel's call gate still decides who may reach this agent at all.
//!
//! A SECRET HOLDER, NEVER A MODEL'S TOOL (owner, 2026-10-02): the store stamps
//! `holds_secrets: true` on its own record at boot, and `validate_meta` refuses any
//! other value. ai_core refuses every model-parsed call that names an agent so
//! marked, so an AI agent's backend reads its key here while its model cannot. The
//! kernel judges first — its shape check refuses anything but `true`/`null` on every
//! agent, so this text answers `null`; its gate lets only the operator lower a mark.
//!
//! A python RAISE is a bundle error rendered `"<Class>: <message>"`, which the
//! kernel settles `failed` with `raised: true` — exactly the record python's
//! dispatch makes of an exception.

#![deny(missing_docs)]

pub mod keychain;
pub mod test_support;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use fantastic_bundle::{py_json_dumps, py_repr, py_repr_value, py_str, py_strip, py_truthy};
use fantastic_file::fs;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    missing, refusal, with, BAD_ARG, NOT_FOUND, UNAUTHORIZED, UNHANDLED,
};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::keychain::{Keychain, OsKeychain};

/// The `handler_module` this bundle answers to — python's package path.
pub const HANDLER_MODULE: &str = "credential_store.tools";

/// The short orientation readme — python's, verbatim.
pub const README: &str = include_str!("readme.md");

const NAME: &str = "credential_store";
const BACKENDS: [&str; 3] = ["file", "keychain", "env"];
/// The record field every store carries as `true` — read by ai_core, which
/// refuses a model-parsed call to an agent so marked.
pub const HOLDS_SECRETS: &str = "holds_secrets";
const DEFAULT_DIR: &str = "~/.fantastic/credentials";
const KEYCHAIN_SERVICE: &str = "fantastic";
const SUBTREE: &str = "$subtree:";

/// What `reflect` says this agent IS — python's `sentence`, verbatim.
pub const SENTENCE: &str = "Credential store — secrets by ref, each bound to one host, read only \
                            with a secret mark, behind per-ref read/write grants.";

/// Each verb's doc line — the FIRST line of python's docstring, in `VERBS` order.
const VERBS: [(&str, &str); 6] = [
    (
        "reflect",
        "Identity + posture: the backend, where the file lives, how many refs; never a \
         value or a ref name. No args.",
    ),
    (
        "boot",
        "Stamps `holds_secrets: true` on this store's record (no model turn may reach it); \
         values are read from the backend when asked for.",
    ),
    (
        "set",
        "args: ref:str, host:str (where the value may be sent), fields:{name: value} (req). \
         Store or rotate a credential. Call it with secret: ['payload.fields']. Returns \
         {ref, host, fields:[names], stored}.",
    ),
    (
        "get",
        "args: ref:str (req), host:str? (where you will send it — refused unless the ref is \
         bound to it). The credential: {ref, host, fields:{name: value}}. Call it with \
         secret: ['result']; send the value ONLY to `host`.",
    ),
    (
        "list",
        "No args. The refs YOU may read or write — metadata only: {refs:[{ref, host, \
         fields:[names], read, write}]}. Never a value.",
    ),
    (
        "forget",
        "args: ref:str (req). Drop a credential — its value and its grants. Writers and the \
         operator only. Returns {forgotten, ref}.",
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

// ─── who is calling, and what they may do ──────────────────────────

/// The calling agent's id — qualified `<guid>|<agent>` when it is on another
/// kernel — or `None` for the operator.
fn caller() -> Option<String> {
    fantastic_kernel::current_remote_caller()
        .or_else(|| fantastic_kernel::current_origin().map(|a| a.0))
}

/// Whether a readers/writers list names `caller`. The operator always passes.
fn covers(kernel: &Kernel, entries: Option<&Value>, caller: Option<&str>) -> bool {
    let Some(caller) = caller else {
        return true;
    };
    let Some(list) = entries.and_then(Value::as_array) else {
        return false;
    };
    for e in list.iter().filter_map(Value::as_str) {
        if e == caller {
            return true;
        }
        if let Some(root) = e.strip_prefix(SUBTREE) {
            if caller.contains('|') {
                continue;
            }
            let mut at = kernel
                .agents
                .get(&AgentId::from(caller))
                .map(|a| Arc::clone(&a));
            while let Some(a) = at {
                if a.id.as_str() == root {
                    return true;
                }
                at = a
                    .parent_id
                    .as_ref()
                    .and_then(|p| kernel.agents.get(p).map(|x| Arc::clone(&x)));
            }
        }
    }
    false
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

/// Whether the caller marked `prefix` (or every `prefix.<name>`) secret.
fn marked(prefix: &str, names: &[String]) -> bool {
    let paths = fantastic_kernel::current_secret();
    paths.iter().any(|p| p == prefix)
        || (!names.is_empty()
            && names
                .iter()
                .all(|n| paths.iter().any(|p| *p == format!("{prefix}.{n}"))))
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
            Err(_) => home_of(None),
        }
    } else {
        home_of(Some(&p[1..i]))
    };
    let Some(home) = home else {
        // pathlib raises when nothing expanded the `~`.
        return Err(raise("RuntimeError", "Could not determine home directory."));
    };
    let trimmed = home.trim_end_matches('/');
    let out = format!("{trimmed}{}", &p[i..]);
    Ok(if out.is_empty() { "/".to_string() } else { out })
}

/// The password database's home directory for `user` (this uid when `None`).
fn home_of(user: Option<&str>) -> Option<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let rc = match user {
        None => {
            // SAFETY: every pointer is valid for the call; `buf` outlives it.
            unsafe {
                libc::getpwuid_r(
                    libc::getuid(),
                    &mut pwd,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut out,
                )
            }
        }
        Some(name) => {
            let name = std::ffi::CString::new(name).ok()?;
            // SAFETY: as above; `name` is NUL-terminated and outlives the call.
            unsafe {
                libc::getpwnam_r(
                    name.as_ptr(),
                    &mut pwd,
                    buf.as_mut_ptr(),
                    buf.len(),
                    &mut out,
                )
            }
        }
    };
    if rc != 0 || out.is_null() || pwd.pw_dir.is_null() {
        return None;
    }
    // SAFETY: `pw_dir` points into `buf`, NUL-terminated by the C library.
    let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
    Some(dir.to_string_lossy().into_owned())
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

/// The default backend: one YAML document of `{refs: {ref: {field: value}}}`.
struct FileBackend {
    /// The base the name is clamped within: the running-dir clamp already
    /// applied for a relative `file`, or the external base an absolute/`~` one
    /// names.
    base: PathBuf,
    name: String,
    shown: String,
}

impl FileBackend {
    fn new(kernel: &Kernel, id: &AgentId, rec: &Map<String, Value>) -> Result<Self, BundleError> {
        let p = match rec.get("file") {
            Some(v) if py_truthy(v) => py_str(v),
            _ => format!("{DEFAULT_DIR}/{}.yaml", id.0),
        };
        if p.starts_with('~') || p.starts_with('/') {
            let (parent, name) = split_path(&expanduser(&p)?);
            return Ok(Self {
                base: PathBuf::from(parent),
                name,
                shown: p,
            });
        }
        // relative: under the kernel's running dir, clamped
        let (parent, name) = split_path(&p);
        let workdir = kernel.workdir().map_err(|e| raise("RuntimeError", e))?;
        let base = fs::resolve_root(&parent, &workdir).map_err(raise_fs)?;
        Ok(Self {
            base,
            name,
            shown: p,
        })
    }

    fn check(&self) -> Result<(), BundleError> {
        use std::os::unix::fs::MetadataExt;
        let st = fs::lstat(&self.base, &self.name).map_err(raise_fs)?;
        let mode = st.mode() & 0o7777;
        // SAFETY: `getuid` has no preconditions and cannot fail.
        let me = unsafe { libc::getuid() };
        if !st.file_type().is_file() || st.uid() != me || mode & 0o077 != 0 {
            return Err(raise(
                "PermissionError",
                format!(
                    "{NAME}: credential file {} must be a regular file owned by this user with \
                     mode 0600 (is 0o{mode:o}, uid {}) — refused, nothing read",
                    self.shown,
                    st.uid()
                ),
            ));
        }
        Ok(())
    }

    fn load(&self) -> Result<Map<String, Value>, BundleError> {
        if !fs::exists(&self.base, &self.name).map_err(raise_fs)? {
            return Ok(Map::new());
        }
        self.check()?;
        let text = fs::read_text(&self.base, &self.name).map_err(raise_fs)?;
        let doc = fantastic_kernel::yaml_io::load(&text).map_err(|e| raise("YAMLError", e))?;
        Ok(doc
            .as_object()
            .and_then(|d| d.get("refs"))
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default())
    }

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

    fn put(&self, r: &str, fields: Map<String, Value>) -> Result<(), BundleError> {
        let mut refs = self.load()?;
        refs.insert(r.to_string(), Value::Object(fields));
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

/// The OS keychain: one item per ref, the fields as a JSON object.
struct KeychainBackend {
    kc: Arc<dyn Keychain>,
    service: String,
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
        let service = match rec.get("keychain_service") {
            Some(v) if py_truthy(v) => py_str(v),
            _ => KEYCHAIN_SERVICE.to_string(),
        };
        Self {
            kc,
            service,
            prefix: format!("{}/", id.0),
        }
    }

    fn get(&self, r: &str) -> Result<Option<Map<String, Value>>, BundleError> {
        let account = format!("{}{r}", self.prefix);
        let Some(raw) = self
            .kc
            .get_password(&self.service, &account)
            .map_err(raise_keychain)?
        else {
            return Ok(None);
        };
        let v: Value = serde_json::from_str(&raw).map_err(|e| raise("JSONDecodeError", e))?;
        Ok(v.as_object().cloned())
    }

    fn put(&self, r: &str, fields: Map<String, Value>) -> Result<(), BundleError> {
        let account = format!("{}{r}", self.prefix);
        self.kc
            .set_password(
                &self.service,
                &account,
                &py_json_dumps(&Value::Object(fields), true, None),
            )
            .map_err(raise_keychain)
    }

    fn drop_ref(&self, r: &str) -> Result<(), BundleError> {
        let account = format!("{}{r}", self.prefix);
        if self
            .kc
            .get_password(&self.service, &account)
            .map_err(raise_keychain)?
            .is_some()
        {
            self.kc
                .delete_password(&self.service, &account)
                .map_err(raise_keychain)?;
        }
        Ok(())
    }
}

/// Read-only: each ref's fields come from the environment variables its record
/// entry names (`env: {field: VAR}`).
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

    fn put(&self, r: &str, fields: Map<String, Value>) -> Result<(), BundleError> {
        match self {
            Self::File(b) => b.put(r, fields),
            Self::Keychain(b) => b.put(r, fields),
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

    fn backend(
        &self,
        kernel: &Kernel,
        id: &AgentId,
        rec: &Map<String, Value>,
    ) -> Result<Backend, BundleError> {
        match backend_kind(rec).as_str() {
            "file" => Ok(Backend::File(FileBackend::new(kernel, id, rec)?)),
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
        let mut verbs = Map::new();
        for (n, doc) in VERBS {
            verbs.insert(n.to_string(), json!(doc));
        }
        let mut out = Map::new();
        out.insert("id".into(), json!(id.0));
        out.insert("sentence".into(), json!(SENTENCE));
        out.insert("backend".into(), json!(backend));
        out.insert("refs".into(), json!(refs_of(rec).len()));
        out.insert("verbs".into(), Value::Object(verbs));
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
        let Some(h) = host(payload.get("host")) else {
            return Ok(missing(&format!("{NAME} set"), "host"));
        };
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
        let names: Vec<String> = fields.keys().cloned().collect();
        if !marked("payload.fields", &names) {
            let mut out = refusal(
                BAD_ARG,
                format!(
                    "{NAME} set: mark the values secret — secret: ['payload.fields'] — or they \
                     would be recorded in the ledger, the memory store and the state stream"
                ),
            );
            with(&mut out, "arg", json!("secret"));
            return Ok(out);
        }
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
        let who = caller();
        let mut refs = refs_of(&rec);
        let entry = match refs.get(r).filter(|v| !v.is_null()) {
            None => {
                if who.is_some() && !covers(kernel, rec.get("creators"), who.as_deref()) {
                    return Ok(no_such_ref(r));
                }
                let owner: Vec<Value> = who.iter().map(|c| json!(c)).collect();
                let mut e = Map::new();
                e.insert("readers".into(), Value::Array(owner.clone()));
                e.insert("writers".into(), Value::Array(owner));
                e
            }
            Some(existing) => {
                let e = entry_map(existing);
                if !covers(kernel, e.get("writers"), who.as_deref()) {
                    if covers(kernel, e.get("readers"), who.as_deref()) {
                        let mut out = refusal(
                            UNAUTHORIZED,
                            format!(
                                "{NAME}: {} may read {} but not write it",
                                who.as_deref().unwrap_or("None"),
                                py_repr(r)
                            ),
                        );
                        with(&mut out, "arg", json!("ref"));
                        return Ok(out);
                    }
                    return Ok(no_such_ref(r));
                }
                e
            }
        };
        self.backend(kernel, id, &rec)?.put(r, fields)?;
        let mut sorted_names = names;
        sorted_names.sort();
        let mut entry = entry;
        entry.insert("host".into(), json!(h));
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
        let who = caller();
        let Some(entry) = refs
            .get(r)
            .filter(|v| !v.is_null())
            .map(entry_map)
            .filter(|e| covers(kernel, e.get("readers"), who.as_deref()))
        else {
            return Ok(no_such_ref(r));
        };
        if payload.get("host").is_some_and(|v| !v.is_null()) {
            let want = host(payload.get("host"));
            if want.is_none() || want != host(entry.get("host")) {
                let mut out = refusal(
                    BAD_ARG,
                    format!(
                        "{NAME}: ref {} is bound to host {}, not {} — refused, nothing read",
                        py_repr(r),
                        py_repr_value(entry.get("host").unwrap_or(&Value::Null)),
                        want.as_deref().map_or_else(|| "None".to_string(), py_repr)
                    ),
                );
                with(&mut out, "arg", json!("host"));
                return Ok(out);
            }
        }
        let names: Vec<String> = match entry.get("fields") {
            Some(v) if py_truthy(v) => v
                .as_array()
                .map(|a| a.iter().map(py_str).collect())
                .unwrap_or_default(),
            _ => entry
                .get("env")
                .and_then(Value::as_object)
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default(),
        };
        if !(marked("result", &[]) || marked("result.fields", &names)) {
            let mut out = refusal(
                BAD_ARG,
                format!(
                    "{NAME} get: mark the answer secret — secret: ['result'] — or the value would \
                     be recorded in the ledger, the memory store and the state stream"
                ),
            );
            with(&mut out, "arg", json!("secret"));
            return Ok(out);
        }
        let fields = self.backend(kernel, id, &rec)?.get(r)?;
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
        let who = caller();
        let mut refs: Vec<(String, Value)> = refs_of(&record(kernel, id)).into_iter().collect();
        refs.sort_by(|a, b| a.0.cmp(&b.0));
        let mut out = Vec::new();
        for (r, entry) in refs {
            let entry = entry_map(&entry);
            let can_read = covers(kernel, entry.get("readers"), who.as_deref());
            let can_write = covers(kernel, entry.get("writers"), who.as_deref());
            if !(can_read || can_write) {
                continue;
            }
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
            row.insert("read".into(), json!(can_read));
            row.insert("write".into(), json!(can_write));
            if who.is_none() {
                for k in ["readers", "writers"] {
                    let v = match entry.get(k) {
                        Some(v) if py_truthy(v) => v.clone(),
                        _ => json!([]),
                    };
                    row.insert(k.into(), v);
                }
            }
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
        let mut refs = refs_of(&rec);
        let who = caller();
        let writable = refs
            .get(r)
            .filter(|v| !v.is_null())
            .map(entry_map)
            .is_some_and(|e| covers(kernel, e.get("writers"), who.as_deref()));
        if !writable {
            return Ok(no_such_ref(r));
        }
        if backend_kind(&rec) != "env" {
            self.backend(kernel, id, &rec)?.drop_ref(r)?;
        }
        refs.shift_remove(r);
        let mut patch = Map::new();
        patch.insert("refs".into(), Value::Object(refs));
        kernel.update(id, patch);
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
        if meta
            .get(HOLDS_SECRETS)
            .is_some_and(|v| v != &Value::Bool(true))
        {
            return Some(
                "holds_secrets must be true — a credential_store always holds secrets".to_string(),
            );
        }
        if let Some(b) = meta.get("backend").filter(|v| !v.is_null()) {
            if !b.as_str().is_some_and(|s| BACKENDS.contains(&s)) {
                return Some(format!(
                    "backend={} is not one of {}",
                    py_repr_value(b),
                    BACKENDS.join(", ")
                ));
            }
        }
        for k in ["file", "keychain_service"] {
            if let Some(v) = meta.get(k).filter(|v| !v.is_null()) {
                if !v.as_str().is_some_and(|s| !py_strip(s).is_empty()) {
                    return Some(format!("{k} must be a non-empty string"));
                }
            }
        }
        if let Some(c) = meta.get("creators").filter(|v| !v.is_null()) {
            if !is_str_list(c) {
                return Some("creators must be a list of agent ids".to_string());
            }
        }
        let refs = meta.get("refs").filter(|v| !v.is_null())?;
        let Some(refs) = refs.as_object() else {
            return Some(
                "refs must be a map of ref to {readers, writers, host, fields, env}".into(),
            );
        };
        for (r, e) in refs {
            if py_strip(r).is_empty() {
                return Some("a ref must be a non-empty string".to_string());
            }
            let Some(e) = e.as_object() else {
                return Some(format!("{} must be a map", ref_label(r)));
            };
            for k in ["readers", "writers", "fields"] {
                if let Some(v) = e.get(k).filter(|v| !v.is_null()) {
                    if !is_str_list(v) {
                        return Some(format!("{}.{k} must be a list of strings", ref_label(r)));
                    }
                }
            }
            if e.get("host").is_some_and(|v| !v.is_null()) && host(e.get("host")).is_none() {
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
            // Stamps `holds_secrets: true` on this store's record (no model turn may
            // reach it); values are read from the backend when asked for.
            "boot" => {
                if record(kernel, agent_id).get(HOLDS_SECRETS) != Some(&Value::Bool(true)) {
                    let mut patch = Map::new();
                    patch.insert(HOLDS_SECRETS.to_string(), Value::Bool(true));
                    kernel.update(agent_id, patch);
                }
                return Ok(None);
            }
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
