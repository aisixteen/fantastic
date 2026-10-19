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
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    missing, refusal, refusal_hint, with, BAD_ARG, INVALID_META, NOT_FOUND, UNAUTHORIZED, UNHANDLED,
};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "user_home_store.tools";

/// The short orientation readme.
pub const README: &str = include_str!("readme.md");

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

/// [`private_dirs`] for a caller outside this crate (`fantastic migrate-home`):
/// `dir` and every missing level up from `home`, each made private as this store
/// makes its own (`0700`; a protected DACL on Windows). `Err` is the refusal's text.
pub fn ensure_private_dirs(home: &Path, dir: &Path) -> Result<(), String> {
    private_dirs(home, dir).map_err(|v| {
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        match s("hint") {
            h if h.is_empty() => s("error"),
            h => format!("{} — {h}", s("error")),
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

pub(crate) fn valid_name(n: &str) -> bool {
    let mut cs = n.chars();
    matches!(cs.next(), Some(c) if c.is_ascii_alphanumeric())
        && n.len() <= 64
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn name_arg(verb: &str, payload: &Value) -> Result<String, Value> {
    match payload.get("name").and_then(Value::as_str) {
        None | Some("") => Err(missing(verb, "name")),
        Some(n) if valid_name(n) => Ok(n.to_string()),
        Some(n) => {
            let mut r = refusal(
                BAD_ARG,
                format!("{verb}: name {n:?} — letters, digits, '-' and '_', starting with a letter or digit, at most 64"),
            );
            with(&mut r, "arg", json!("name"));
            Err(r)
        }
    }
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
    fn persist(&self, payload: &Value, kernel: &Arc<Kernel>) -> Result<Value, Value> {
        let name = name_arg("persist_record", payload)?;
        let rec = match payload.get("record").and_then(Value::as_object) {
            Some(r) => r.clone(),
            None => return Err(missing("persist_record", "record")),
        };
        let Some(hm) = rec.get("handler_module").and_then(Value::as_str) else {
            return Err(missing("persist_record", "record.handler_module"));
        };
        let bad: Vec<&str> = NOT_SAVED.iter().copied().filter(|k| rec.contains_key(*k)).collect();
        if !bad.is_empty() {
            return Err(refusal_hint(
                INVALID_META,
                format!("persist_record: a saved set-up carries no identity, wiring or authority — drop {}", bad.join(", ")),
                "the creator adds id, parent, memory store and grants when it makes an agent from a saved set-up",
            ));
        }
        let Some(bundle) = kernel.bundles.get(hm) else {
            return Err(refusal(INVALID_META, format!("persist_record: no bundle {hm:?} is linked into this build")));
        };
        // A saved set-up is checked against the backend's RECORD SCHEMA (the same
        // constants its create validates with) — minus what the CREATOR adds
        // (`NOT_SAVED`: the memory store, id, grants…). A full
        // `validate_meta` would demand exactly those, which a saved set-up must never
        // carry. A bundle with no schema is checked by its own `validate_meta`.
        let schema = bundle.record_schema();
        if schema.is_none() {
            let mut meta = rec.clone();
            meta.remove("handler_module");
            let mut target = Map::new();
            target.insert("handler_module".into(), json!(hm));
            if let Some(why) = bundle.validate_meta(&meta, &target) {
                return Err(refusal(INVALID_META, format!("persist_record: {hm} refuses this saved set-up: {why}")));
            }
        }
        if let Some(schema) = schema {
            let fields = schema["fields"].as_array().cloned().unwrap_or_default();
            let mut missing_f: Vec<String> = Vec::new();
            for f in &fields {
                let name = f["name"].as_str().unwrap_or("");
                if NOT_SAVED.contains(&name) {
                    continue;
                }
                let given = rec.get(name).filter(|v| !v.is_null());
                if f["required"].as_bool() == Some(true) && given.is_none() {
                    missing_f.push(format!("{name} = {}", f["hint"].as_str().unwrap_or("")));
                }
                if let (Some(v), Some(choices)) = (given, f["enum"].as_array()) {
                    if !choices.contains(v) {
                        return Err(refusal(INVALID_META, format!("persist_record: {hm} — {name} must be one of {}", Value::Array(choices.clone()))));
                    }
                }
            }
            if !missing_f.is_empty() {
                return Err(refusal(INVALID_META, format!("persist_record: {hm} refuses this saved set-up — required and missing: {}", missing_f.join("; "))));
            }
        }
        let path = self.saved_for_write()?.join(format!("{name}.yaml"));
        let now = read_private(&path)?.map(|b| version(&b));
        if let Some(want) = payload.get("if_match").and_then(Value::as_str) {
            if now.as_deref() != Some(want) {
                return Err(stale("persist_record", &name, want, now.as_deref()));
            }
        }
        let text = fantastic_kernel::yaml_io::dump(&Value::Object(rec));
        write_private(&path, text.as_bytes())?;
        Ok(json!({"name": name, "version": version(text.as_bytes()), "path": path.display().to_string()}))
    }

    fn forget(&self, payload: &Value) -> Result<Value, Value> {
        let name = name_arg("forget_record", payload)?;
        let Some(dir) = self.saved_if_any()? else {
            return Err(refusal(NOT_FOUND, format!("forget_record: no saved set-up {name}")));
        };
        let path = dir.join(format!("{name}.yaml"));
        let Some(bytes) = read_private(&path)? else {
            return Err(refusal(NOT_FOUND, format!("forget_record: no saved set-up {name}")));
        };
        if let Some(want) = payload.get("if_match").and_then(Value::as_str) {
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

    fn prefer(&self, payload: &Value) -> Result<Value, Value> {
        let Some(dir) = self.saved_if_any()? else {
            return match payload.get("name") {
                Some(Value::Null) => Ok(json!({"preferred": null})),
                _ => Err(refusal(NOT_FOUND, format!("prefer: no saved set-up {}", payload.get("name").and_then(Value::as_str).unwrap_or("")))),
            };
        };
        let p = dir.join(PREFERRED);
        match payload.get("name") {
            Some(Value::Null) => {
                if p.exists() {
                    fs::remove_file(&p).map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", p.display())))?;
                }
                Ok(json!({"preferred": null}))
            }
            _ => {
                let name = name_arg("prefer", payload)?;
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
            "sentence": "The user's saved set-ups: named agent-record blueprints in the home dir (~/.fantastic), applied by the CLI when it creates an agent.",
            "dir": self.home.as_ref().map(|h| saved_dir(h).display().to_string()).ok(),
        })
    }
}

#[async_trait]
impl Bundle for UserHomeStore {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// HOW TO CALL it — served by the kernel's `docs` verb, never on a reflect.
    fn verb_docs(&self) -> Value {
        json!({
                "persist_record": "args: name:str (req), record:dict (req — handler_module + that bundle's config; no identity, wiring or grants), if_match:str?. Returns {name, version, path}.",
                "forget_record": "args: name:str (req), if_match:str?. Returns {name, forgotten}.",
                "load_tree": "No args. Returns {saved: [{name, version, record}], preferred, dir}.",
                "prefer": "args: name:str|null (req). The owner's default for new AI agents; null clears it. Returns {preferred}.",
            })
    }

    async fn handle(&self, _agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "boot" {
            return Ok(None);
        }
        if verb == "reflect" {
            return Ok(Some(self.reflect()));
        }
        let out = match verb {
            "persist_record" => self.persist(payload, kernel),
            "forget_record" => self.forget(payload),
            "load_tree" => self.load_tree(),
            "prefer" => self.prefer(payload),
            _ => {
                let mut r = refusal(UNHANDLED, format!("user_home_store: unknown type {verb:?}"));
                with(&mut r, "verb", json!(verb));
                Err(r)
            }
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
