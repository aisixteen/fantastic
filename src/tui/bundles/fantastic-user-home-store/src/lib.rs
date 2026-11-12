//! user_home_store — the user's SAVED SET-UP store (`user_home_store.tools`).
//!
//! Named agent-record blueprints in the user's home dir, one plain-text file
//! per name:
//!
//! ```text
//!   <home>/                        0700   FANTASTIC_HOME, else ~/.fantastic
//!   ├── user/                      0700   what the user keeps (owner 2026-10-19)
//!   │   ├── setups/                0700
//!   │   │   └── ai_agent/          0700   saved set-ups, per agent kind
//!   │   │       ├── opus.yaml      0600   handler_module + that bundle's config
//!   │   │       └── .preferred     0600   the owner's explicit default (a name)
//!   │   └── keys/                  0700   the key mode + refs (`secrets`)
//!   └── fantastics/                0700   the stored locations (`user_store`)
//! ```
//!
//! THE BINARY OWNS IT. Its directory is fixed when the CLI registers it
//! ([`UserHomeStore::new`]); nothing on a record can move it. A project record
//! naming this module — a cloned tree, an AI's `create_agent` — gets the same
//! store, so it cannot read another location. WHO reaches it is the record's
//! grants (`callable_by`), as for every agent. (The lib session's analysis,
//! 2026-09-30: a file_bridge rooted in home, even operator-set, would let a
//! hydrated project record rewrite the owner's prefs.)
//!
//! A SAVED SET-UP IS NOT AN AGENT. It carries no identity, no per-project wiring and
//! no authority — the creator adds those at instantiation — and it must pass the
//! target bundle's own `validate_meta`, so a saved set-up that could not make a
//! working agent is refused when saved, not when used.
//!
//! Files `0600`, dirs `0700`, created with that mode (no window); a saved set-up or
//! dir readable by others is refused, naming the `chmod`. Writes are atomic
//! (temp + rename); `if_match` (the version read) refuses a lost update between
//! two running fantastics.

#![deny(missing_docs)]

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::refusal::{
    refusal, refusal_hint, with, BAD_ARG, INVALID_META, NOT_FOUND, UNAUTHORIZED,
};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "user_home_store.tools";

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "The user's saved set-ups: named agent-record blueprints kept in the user's home dir, applied by the CLI when it creates an agent. A saved set-up carries no identity, wiring or authority; the creator adds those.";

pub mod record;
pub mod verbs;

/// The environment variable that names the home dir; unset → `~/.fantastic`.
pub const HOME_VAR: &str = "FANTASTIC_HOME";

/// Fields a saved set-up may never carry: identity, per-project wiring, authority.
pub const NOT_SAVED: [&str; 10] = [
    "id",
    "parent_id",
    "guid",
    "agents",
    "memory_store_id",
    "can_call",
    "callable_by",
    "ingress_rule",
    "egress_rule",
    "auth",
];

const PREFERRED: &str = ".preferred";

/// The user's group under the home: `user`.
pub fn user_dir(home: &Path) -> PathBuf {
    home.join("user")
}

/// The AI agent's saved set-ups dir under the home: `user/setups/ai_agent`.
pub fn saved_dir(home: &Path) -> PathBuf {
    user_dir(home).join("setups").join("ai_agent")
}

/// Create `dir` and every missing level up from `home`, each `0700`; refuse any
/// level others can read.
pub(crate) fn private_dirs(home: &Path, dir: &Path) -> Result<(), Value> {
    let mut levels = vec![home.to_path_buf()];
    if let Ok(rest) = dir.strip_prefix(home) {
        let mut at = home.to_path_buf();
        for c in rest.components() {
            at.push(c);
            levels.push(at.clone());
        }
    }
    for d in &levels {
        if !d.exists() {
            sys::create_private_dir(d)
                .map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", d.display())))?;
        }
        private(d, true)?;
    }
    Ok(())
}

/// [`private_dirs`] for another crate: `dir` and every missing level up from
/// `home` created `0700` (on Windows, a DACL that protects only the user), and
/// any level others can read refused. `Err` is the refusal's text, with its hint
/// (the `chmod` or `icacls` that fixes it) when it has one.
pub fn ensure_private_dir(home: &Path, dir: &Path) -> Result<(), String> {
    private_dirs(home, dir).map_err(|r| {
        let s = |k: &str| r.get(k).and_then(Value::as_str).map(str::to_string);
        match (s("error"), s("hint")) {
            (Some(e), Some(h)) if !h.is_empty() => format!("{e} — {h}"),
            (Some(e), _) => e,
            (None, _) => r.to_string(),
        }
    })
}

/// The user's own home dir: `$HOME` (`%USERPROFILE%` on Windows). `Err` names
/// the variable when it is not set.
pub fn user_home_from_env() -> Result<PathBuf, String> {
    std::env::var_os(sys::USER_HOME)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| format!("{} is not set", sys::USER_HOME))
}

/// Where the saved set-ups live, resolved ONCE by the binary: `FANTASTIC_HOME`, else
/// `$HOME/.fantastic` (`%USERPROFILE%\.fantastic` on Windows). `Err` names what is
/// missing — never guessed.
pub fn home_dir_from_env() -> Result<PathBuf, String> {
    if let Some(h) = std::env::var_os(HOME_VAR).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    match std::env::var_os(sys::USER_HOME).filter(|v| !v.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(".fantastic")),
        None => Err(format!("neither {HOME_VAR} nor {} is set — no home dir for saved set-ups", sys::USER_HOME)),
    }
}

/// The store. Construct it with its dir.
pub struct UserHomeStore {
    home: Result<PathBuf, String>,
}

impl UserHomeStore {
    /// `home`: the resolved dir (or why there is none).
    pub fn new(home: Result<PathBuf, String>) -> Self {
        Self { home }
    }

    /// The saved set-ups dir for a READ: `None` when nothing was ever saved — a
    /// read creates nothing on the user's disk.
    fn saved_if_any(&self) -> Result<Option<PathBuf>, Value> {
        let home = self.home.as_ref().map_err(|e| refusal(NOT_FOUND, format!("user_home_store: {e}")))?;
        let dir = saved_dir(home);
        if !dir.exists() {
            return Ok(None);
        }
        private(home, true)?;
        private(&dir, true)?;
        Ok(Some(dir))
    }

    /// The saved set-ups dir for a WRITE: created `0700` (both levels) if missing.
    fn saved_for_write(&self) -> Result<PathBuf, Value> {
        let home = self.home.as_ref().map_err(|e| refusal(NOT_FOUND, format!("user_home_store: {e}")))?;
        let dir = saved_dir(home);
        private_dirs(home, &dir)?;
        Ok(dir)
    }
}

/// Refuse a path others can read or write — the ssh rule — naming the fix.
pub(crate) fn private(p: &Path, dir: bool) -> Result<(), Value> {
    let mode = sys::mode_of(p).map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", p.display())))?;
    if mode & 0o077 != 0 {
        return Err(refusal_hint(
            UNAUTHORIZED,
            format!("user_home_store: {} is readable by others (mode {mode:o})", p.display()),
            sys::private_fix(p, dir),
        ));
    }
    Ok(())
}

fn version(bytes: &[u8]) -> String {
    let d = Sha256::digest(bytes);
    d.iter().map(|b| format!("{b:02x}")).collect::<String>()[..16].to_string()
}

/// Is `n` a name `forget_record` would take: its declared `name` (one pattern, in the
/// verb's declaration) read by the shared engine, for a name found on disk.
pub(crate) fn valid_name(n: &str) -> bool {
    let verb = verbs::VERBS.iter().find(|v| v.name == "forget_record").expect("forget_record is declared");
    fantastic_kernel::verbs::validate("forget_record", verb.schema(), &json!({"name": n})).is_ok()
}

/// Write `bytes` to `path` atomically, created `0600`.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Value> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    let io = |e: std::io::Error| refusal(BAD_ARG, format!("user_home_store: {}: {e}", path.display()));
    let mut f = sys::create_private_file(&tmp).map_err(io)?;
    f.write_all(bytes).and_then(|()| f.sync_all()).map_err(io)?;
    fs::rename(&tmp, path).map_err(io)
}

pub(crate) fn read_private(path: &Path) -> Result<Option<Vec<u8>>, Value> {
    if !path.exists() {
        return Ok(None);
    }
    private(path, false)?;
    fs::read(path).map(Some).map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", path.display())))
}

fn stale(verb: &str, name: &str, want: &str, have: Option<&str>) -> Value {
    let mut r = refusal_hint(
        BAD_ARG,
        format!(
            "{verb}: {name} changed since you read it (you had {want}, it is {})",
            have.unwrap_or("gone")
        ),
        "load_tree again, then retry",
    );
    with(&mut r, "arg", json!("if_match"));
    r
}

impl UserHomeStore {
    fn persist(&self, a: verbs::PersistRecord, kernel: &Arc<Kernel>) -> Result<Value, Value> {
        let name = a.name;
        let hm = a.record.handler_module.as_str();
        let Some(bundle) = kernel.bundles.get(hm) else {
            return Err(refusal(INVALID_META, format!("persist_record: no bundle {hm:?} is linked into this build")));
        };
        // A saved set-up is checked against the bundle's DECLARED RECORD (the same
        // declaration its create is checked with, by the bundle's own shared entry):
        // every field known and of its type, and the required ones present (a saved
        // set-up is a record to be born: the create's check).
        bundle.check_record("create_agent", &a.record.rest)?;
        let path = self.saved_for_write()?.join(format!("{name}.yaml"));
        let now = read_private(&path)?.map(|b| version(&b));
        if let Some(want) = a.if_match.as_deref() {
            if now.as_deref() != Some(want) {
                return Err(stale("persist_record", &name, want, now.as_deref()));
            }
        }
        let mut rec = a.record.rest;
        rec.insert("handler_module".into(), Value::String(a.record.handler_module));
        let text = fantastic_kernel::yaml_io::dump(&Value::Object(rec));
        write_private(&path, text.as_bytes())?;
        Ok(json!({"name": name, "version": version(text.as_bytes()), "path": path.display().to_string()}))
    }

    fn forget(&self, a: verbs::ForgetRecord) -> Result<Value, Value> {
        let name = a.name;
        let Some(dir) = self.saved_if_any()? else {
            return Err(refusal(NOT_FOUND, format!("forget_record: no saved set-up {name}")));
        };
        let path = dir.join(format!("{name}.yaml"));
        let Some(bytes) = read_private(&path)? else {
            return Err(refusal(NOT_FOUND, format!("forget_record: no saved set-up {name}")));
        };
        if let Some(want) = a.if_match.as_deref() {
            let have = version(&bytes);
            if have != want {
                return Err(stale("forget_record", &name, want, Some(&have)));
            }
        }
        fs::remove_file(&path).map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", path.display())))?;
        if self.preferred(&dir)?.as_deref() == Some(name.as_str()) {
            let _ = fs::remove_file(dir.join(PREFERRED));
        }
        Ok(json!({"name": name, "forgotten": true}))
    }

    fn preferred(&self, dir: &Path) -> Result<Option<String>, Value> {
        Ok(read_private(&dir.join(PREFERRED))?
            .map(|b| String::from_utf8_lossy(&b).trim().to_string())
            .filter(|s| !s.is_empty()))
    }

    fn load_tree(&self) -> Result<Value, Value> {
        let shown = self.home.as_ref().map(|h| saved_dir(h).display().to_string()).unwrap_or_default();
        let Some(dir) = self.saved_if_any()? else {
            return Ok(json!({"saved": [], "preferred": null, "dir": shown}));
        };
        let mut names: Vec<String> = fs::read_dir(&dir)
            .map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", dir.display())))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".yaml")).map(str::to_string))
            .filter(|n| valid_name(n))
            .collect();
        names.sort();
        let mut out = Vec::new();
        for n in names {
            let path = dir.join(format!("{n}.yaml"));
            let Some(bytes) = read_private(&path)? else { continue };
            let rec = fantastic_kernel::yaml_io::load(&String::from_utf8_lossy(&bytes))
                .map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", path.display())))?;
            out.push(json!({"name": n, "version": version(&bytes), "record": rec}));
        }
        Ok(json!({"saved": out, "preferred": self.preferred(&dir)?, "dir": dir.display().to_string()}))
    }

    fn prefer(&self, a: verbs::Prefer) -> Result<Value, Value> {
        let Some(dir) = self.saved_if_any()? else {
            return match a.name {
                None => Ok(json!({"preferred": null})),
                Some(n) => Err(refusal(NOT_FOUND, format!("prefer: no saved set-up {n}"))),
            };
        };
        let p = dir.join(PREFERRED);
        match a.name {
            None => {
                if p.exists() {
                    fs::remove_file(&p).map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", p.display())))?;
                }
                Ok(json!({"preferred": null}))
            }
            Some(name) => {
                if !dir.join(format!("{name}.yaml")).exists() {
                    return Err(refusal(NOT_FOUND, format!("prefer: no saved set-up {name}")));
                }
                write_private(&p, name.as_bytes())?;
                Ok(json!({"preferred": name}))
            }
        }
    }

    fn reflect(&self) -> Value {
        json!({
            "handler_module": HANDLER_MODULE,
            "sentence": MAN,
            "dir": self.home.as_ref().map(|h| saved_dir(h).display().to_string()).ok(),
        })
    }
}

#[async_trait]
impl Handler for UserHomeStore {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(&self, _agent_id: &AgentId, call: Self::Call, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let out = match call {
            verbs::Call::Reflect(_) => return Ok(Some(self.reflect())),
            verbs::Call::PersistRecord(a) => self.persist(a, kernel),
            verbs::Call::ForgetRecord(a) => self.forget(a),
            verbs::Call::LoadTree(_) => self.load_tree(),
            verbs::Call::Prefer(a) => self.prefer(a),
        };
        Ok(Some(out.unwrap_or_else(|r| r)))
    }

    /// Deleting the agent never deletes the owner's saved set-ups: they belong to the
    /// user, not to this project.
    async fn on_delete(&self, _agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        Ok(())
    }
}

pub mod secrets;
mod sys;
pub mod user_store;

#[cfg(test)]
mod tests;
