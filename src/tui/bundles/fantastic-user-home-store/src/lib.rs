//! user_home_store — the user's TEMPLATE store (`user_home_store.tools`).
//!
//! Named agent-record blueprints in the user's home config dir, one plain-text
//! file per name:
//!
//! ```text
//!   <home>/                    0700   FANTASTIC_HOME, else ~/.config/fantastic
//!   └── templates/             0700
//!       ├── opus.yaml          0600   handler_module + that bundle's config
//!       ├── qwen-vllm.yaml     0600
//!       └── .preferred         0600   the owner's explicit default (a name)
//! ```
//!
//! THE BINARY OWNS IT. Its directory and the callers it serves are fixed when the
//! product registers it ([`UserHomeStore::new`]); nothing on a record can move it
//! or widen who it answers. A project record naming this module — a cloned tree,
//! an AI's `create_agent` — gets the same store with the same short caller list,
//! so it can neither read another location nor reach it from an agent. (The lib
//! session's analysis, 2026-09-30: a file_bridge rooted in home, even
//! operator-set, would let a hydrated project record rewrite the owner's prefs.)
//!
//! A TEMPLATE IS NOT AN AGENT. It carries no identity, no per-project wiring and
//! no authority — the creator adds those at instantiation — and it must pass the
//! target bundle's own `validate_meta`, so a template that could not make a
//! working agent is refused when saved, not when used.
//!
//! Files `0600`, dirs `0700`, created with that mode (no window); a template or
//! dir readable by others is refused, naming the `chmod`. Writes are atomic
//! (temp + rename); `if_match` (the version read) refuses a lost update between
//! two running fantastics.

#![deny(missing_docs)]

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
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

/// The environment variable that names the home dir; unset → `~/.config/fantastic`.
pub const HOME_VAR: &str = "FANTASTIC_HOME";

/// Fields a template may never carry: identity, per-project wiring, authority.
pub const NOT_IN_A_TEMPLATE: [&str; 10] = [
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

/// Where the templates live, resolved ONCE by the binary: `FANTASTIC_HOME`, else
/// `$HOME/.config/fantastic`. `Err` names what is missing — never guessed.
pub fn home_dir_from_env() -> Result<PathBuf, String> {
    if let Some(h) = std::env::var_os(HOME_VAR).filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(h));
    }
    match std::env::var_os("HOME").filter(|v| !v.is_empty()) {
        Some(h) => Ok(PathBuf::from(h).join(".config").join("fantastic")),
        None => Err(format!("neither {HOME_VAR} nor HOME is set — no home dir for templates")),
    }
}

/// The store. Construct it with the dir and the callers it serves.
pub struct UserHomeStore {
    home: Result<PathBuf, String>,
    callers: Vec<String>,
}

impl UserHomeStore {
    /// `home`: the resolved dir (or why there is none). `callers`: the ONLY agent
    /// ids served, besides the operator.
    pub fn new(home: Result<PathBuf, String>, callers: Vec<String>) -> Self {
        Self { home, callers }
    }

    /// The templates dir for a READ: `None` when nothing was ever saved — a
    /// read creates nothing on the user's disk.
    fn templates_if_any(&self) -> Result<Option<PathBuf>, Value> {
        let home = self.home.as_ref().map_err(|e| refusal(NOT_FOUND, format!("user_home_store: {e}")))?;
        let dir = home.join("templates");
        if !dir.exists() {
            return Ok(None);
        }
        private(home, true)?;
        private(&dir, true)?;
        Ok(Some(dir))
    }

    /// The templates dir for a WRITE: created `0700` (both levels) if missing.
    fn templates(&self) -> Result<PathBuf, Value> {
        let home = self.home.as_ref().map_err(|e| refusal(NOT_FOUND, format!("user_home_store: {e}")))?;
        let dir = home.join("templates");
        for d in [home.as_path(), dir.as_path()] {
            if !d.exists() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(d)
                    .map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", d.display())))?;
            }
            private(d, true)?;
        }
        Ok(dir)
    }

    /// Who may reach the owner's templates — decided HERE, not only by the
    /// record's `callable_by`: the templates are the USER's resource, not this
    /// project's, and a project record (cloned, or written by an AI) must not be
    /// able to widen who reads or rewrites them. The caller is the dispatch's
    /// ORIGIN (the handler's own "sender" is itself); a call relayed across a
    /// bridge arrives as its leg, which is not one of the callers.
    fn served(&self) -> Result<(), Value> {
        match fantastic_kernel::send::current_origin() {
            None => Ok(()),
            Some(s) if self.callers.iter().any(|c| c == &s.0) => Ok(()),
            Some(s) => Err(refusal_hint(
                UNAUTHORIZED,
                format!("user_home_store serves only the CLI ({}), not {}", self.callers.join(", "), s.0),
                "templates are applied by the CLI when it creates an agent — no agent reaches them",
            )),
        }
    }
}

/// Refuse a path others can read or write — the ssh rule — naming the fix.
pub(crate) fn private(p: &Path, dir: bool) -> Result<(), Value> {
    let m = fs::metadata(p).map_err(|e| refusal(BAD_ARG, format!("user_home_store: {}: {e}", p.display())))?;
    let mode = m.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        let want = if dir { "700" } else { "600" };
        return Err(refusal_hint(
            UNAUTHORIZED,
            format!("user_home_store: {} is readable by others (mode {mode:o})", p.display()),
            format!("chmod {want} {}", p.display()),
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
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp).map_err(io)?;
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
        let bad: Vec<&str> = NOT_IN_A_TEMPLATE.iter().copied().filter(|k| rec.contains_key(*k)).collect();
        if !bad.is_empty() {
            return Err(refusal_hint(
                INVALID_META,
                format!("persist_record: a template carries no identity, wiring or authority — drop {}", bad.join(", ")),
                "the creator adds id, parent, memory store and grants when it makes an agent from a template",
            ));
        }
        let Some(bundle) = kernel.bundles.get(hm) else {
            return Err(refusal(INVALID_META, format!("persist_record: no bundle {hm:?} is linked into this build")));
        };
        // A template is checked against the backend's RECORD SCHEMA (the same
        // constants its create validates with) — minus what the CREATOR adds
        // (`NOT_IN_A_TEMPLATE`: the memory store, id, grants…). A full
        // `validate_meta` would demand exactly those, which a template must never
        // carry. A bundle with no schema is checked by its own `validate_meta`.
        let schema = bundle.record_schema();
        if schema.is_none() {
            let mut meta = rec.clone();
            meta.remove("handler_module");
            let mut target = Map::new();
            target.insert("handler_module".into(), json!(hm));
            if let Some(why) = bundle.validate_meta(&meta, &target) {
                return Err(refusal(INVALID_META, format!("persist_record: {hm} refuses this template: {why}")));
            }
        }
        if let Some(schema) = schema {
            let fields = schema["fields"].as_array().cloned().unwrap_or_default();
            let mut missing_f: Vec<String> = Vec::new();
            for f in &fields {
                let name = f["name"].as_str().unwrap_or("");
                if NOT_IN_A_TEMPLATE.contains(&name) {
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
                return Err(refusal(INVALID_META, format!("persist_record: {hm} refuses this template — required and missing: {}", missing_f.join("; "))));
            }
        }
        let path = self.templates()?.join(format!("{name}.yaml"));
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
        let Some(dir) = self.templates_if_any()? else {
            return Err(refusal(NOT_FOUND, format!("forget_record: no template {name}")));
        };
        let path = dir.join(format!("{name}.yaml"));
        let Some(bytes) = read_private(&path)? else {
            return Err(refusal(NOT_FOUND, format!("forget_record: no template {name}")));
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
        let shown = self.home.as_ref().map(|h| h.join("templates").display().to_string()).unwrap_or_default();
        let Some(dir) = self.templates_if_any()? else {
            return Ok(json!({"templates": [], "preferred": null, "dir": shown}));
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
        Ok(json!({"templates": out, "preferred": self.preferred(&dir)?, "dir": dir.display().to_string()}))
    }

    fn prefer(&self, payload: &Value) -> Result<Value, Value> {
        let Some(dir) = self.templates_if_any()? else {
            return match payload.get("name") {
                Some(Value::Null) => Ok(json!({"preferred": null})),
                _ => Err(refusal(NOT_FOUND, format!("prefer: no template {}", payload.get("name").and_then(Value::as_str).unwrap_or("")))),
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
                    return Err(refusal(NOT_FOUND, format!("prefer: no template {name}")));
                }
                write_private(&p, name.as_bytes())?;
                Ok(json!({"preferred": name}))
            }
        }
    }

    fn reflect(&self) -> Value {
        json!({
            "handler_module": HANDLER_MODULE,
            "sentence": "The user's templates: named agent-record blueprints in the home config dir, applied by the CLI when it creates an agent.",
            "dir": self.home.as_ref().map(|h| h.join("templates").display().to_string()).ok(),
            "serves": self.callers,
            "verbs": {
                "persist_record": "args: name:str (req), record:dict (req — handler_module + that bundle's config; no identity, wiring or grants), if_match:str?. Returns {name, version, path}.",
                "forget_record": "args: name:str (req), if_match:str?. Returns {name, forgotten}.",
                "load_tree": "No args. Returns {templates: [{name, version, record}], preferred, dir}.",
                "prefer": "args: name:str|null (req). The owner's default for new AI agents; null clears it. Returns {preferred}.",
            },
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

    async fn handle(&self, _agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if verb == "boot" {
            return Ok(None);
        }
        if verb == "reflect" {
            return Ok(Some(self.reflect()));
        }
        if let Err(r) = self.served() {
            return Ok(Some(r));
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

    /// Deleting the agent never deletes the owner's templates: they belong to the
    /// user, not to this project.
    async fn on_delete(&self, _agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        Ok(())
    }
}

pub mod secrets;

#[cfg(test)]
mod tests;
