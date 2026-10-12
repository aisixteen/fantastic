//! user_store — THE USER STORE (`user_store.tools`, owner 2026-10-12): what the
//! user keeps across every fantastic they run, in the home dir:
//!
//! ```text
//!   <home>/
//!   ├── agents/ai_agent/keys/      the stored keys (refs + hosts shown, never values)
//!   ├── agents/ai_agent/templates/ the saved set-ups (names shown)
//!   └── fantastics/                0700   the stored LOCATIONS
//!       └── <id>.yaml              0600   {path, name, stored_at, last_seen}
//! ```
//!
//! `<id>` is a short hash of the path: one file per fantastic, so two running
//! ones never write the same file. A stored fantastic's `last_seen` moves on
//! each boot. This is the list a local manager of fantastics reads.
//!
//! Verbs: `list`; `store_here {name?}`, `unstore {path?}` (default: this one),
//! `unstore_others` (every location but this one).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{refusal, with, BAD_ARG, NOT_FOUND, UNHANDLED};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{private, private_dirs, read_private, secrets::Secrets, templates_dir, valid_name, write_private};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "user_store.tools";

/// The locations dir under the home.
pub fn locations_dir(home: &Path) -> PathBuf {
    home.join("fantastics")
}

/// A location's file name: the first 8 bytes of its path's sha256, hex.
pub fn location_id(path: &Path) -> String {
    let d = Sha256::digest(path.display().to_string().as_bytes());
    d.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// The store. `here` is this fantastic's project dir, fixed by the binary.
pub struct UserStore {
    home: Result<PathBuf, String>,
    here: PathBuf,
}

impl UserStore {
    /// `home`: the resolved home dir (or why there is none); `here`: this fantastic's dir.
    pub fn new(home: Result<PathBuf, String>, here: PathBuf) -> Self {
        Self { home, here }
    }

    fn home(&self) -> Result<&Path, Value> {
        self.home.as_deref().map_err(|e| refusal(NOT_FOUND, format!("user_store: {e}")))
    }

    fn file_of(&self, path: &Path) -> Result<PathBuf, Value> {
        Ok(locations_dir(self.home()?).join(format!("{}.yaml", location_id(path))))
    }

    /// Every stored location, oldest stored first.
    fn locations(&self) -> Result<Vec<Value>, Value> {
        let dir = locations_dir(self.home()?);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        private(&dir, true)?;
        let mut out = Vec::new();
        for e in fs::read_dir(&dir).map_err(|e| refusal(BAD_ARG, format!("user_store: {}: {e}", dir.display())))?.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("yaml") {
                continue;
            }
            let Some(bytes) = read_private(&p)? else { continue };
            let Ok(mut v) = fantastic_kernel::yaml_io::load(&String::from_utf8_lossy(&bytes)) else { continue };
            let path = v["path"].as_str().unwrap_or("").to_string();
            if let Some(o) = v.as_object_mut() {
                o.insert("id".into(), json!(p.file_stem().and_then(|s| s.to_str()).unwrap_or("")));
                o.insert("here".into(), json!(Path::new(&path) == self.here));
                o.insert("exists".into(), json!(Path::new(&path).join(".fantastic").is_dir()));
            }
            out.push(v);
        }
        out.sort_by_key(|v| v["stored_at"].as_u64().unwrap_or(0));
        Ok(out)
    }

    fn list(&self) -> Result<Value, Value> {
        let home = self.home()?;
        let keys: Vec<Value> = Secrets::new(Ok(home.to_path_buf()))
            .refs()
            .map_err(|e| refusal(BAD_ARG, format!("user_store: {e}")))?
            .into_iter()
            .map(|(r, h)| json!({"ref": r, "host": h}))
            .collect();
        let tdir = templates_dir(home);
        let mut templates: Vec<String> = fs::read_dir(&tdir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".yaml")).map(str::to_string))
                    .filter(|n| valid_name(n))
                    .collect()
            })
            .unwrap_or_default();
        templates.sort();
        let locations = self.locations()?;
        let stored = locations.iter().any(|l| l["here"] == true);
        Ok(json!({
            "home": home.display().to_string(),
            "here": {"path": self.here.display().to_string(), "stored": stored},
            "keys": keys,
            "templates": templates,
            "locations": locations,
        }))
    }

    fn store_here(&self, payload: &Value) -> Result<Value, Value> {
        let home = self.home()?;
        let dir = locations_dir(home);
        private_dirs(home, &dir)?;
        let file = self.file_of(&self.here)?;
        let was = read_private(&file)?.and_then(|b| fantastic_kernel::yaml_io::load(&String::from_utf8_lossy(&b)).ok());
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .map(str::to_string)
            .or_else(|| was.as_ref().and_then(|w| w["name"].as_str().map(str::to_string)))
            .unwrap_or_else(|| self.here.file_name().and_then(|n| n.to_str()).unwrap_or("fantastic").to_string());
        let t = now();
        let stored_at = was.as_ref().and_then(|w| w["stored_at"].as_u64()).unwrap_or(t);
        let rec = json!({"path": self.here.display().to_string(), "name": name, "stored_at": stored_at, "last_seen": t});
        write_private(&file, fantastic_kernel::yaml_io::dump(&rec).as_bytes())?;
        Ok(json!({"stored": rec, "id": location_id(&self.here)}))
    }

    fn unstore(&self, payload: &Value) -> Result<Value, Value> {
        let path = payload.get("path").and_then(Value::as_str).map_or_else(|| self.here.clone(), PathBuf::from);
        let file = self.file_of(&path)?;
        if !file.exists() {
            let mut r = refusal(NOT_FOUND, format!("unstore: {} is not stored", path.display()));
            with(&mut r, "arg", json!("path"));
            return Err(r);
        }
        fs::remove_file(&file).map_err(|e| refusal(BAD_ARG, format!("user_store: {}: {e}", file.display())))?;
        Ok(json!({"unstored": path.display().to_string()}))
    }

    fn unstore_others(&self) -> Result<Value, Value> {
        let mut gone = Vec::new();
        for l in self.locations()? {
            if l["here"] == true {
                continue;
            }
            let file = locations_dir(self.home()?).join(format!("{}.yaml", l["id"].as_str().unwrap_or("")));
            fs::remove_file(&file).map_err(|e| refusal(BAD_ARG, format!("user_store: {}: {e}", file.display())))?;
            gone.push(l["path"].clone());
        }
        Ok(json!({"unstored": gone}))
    }

    /// On boot: a stored fantastic's `last_seen` moves to now.
    fn seen(&self) {
        let Ok(file) = self.file_of(&self.here) else { return };
        if let Ok(Some(_)) = read_private(&file) {
            let _ = self.store_here(&json!({}));
        }
    }

    fn reflect(&self) -> Value {
        json!({
            "handler_module": HANDLER_MODULE,
            "sentence": "The user store: the keys and set-ups kept in the user's home, and the stored locations of their fantastics — store this one, unstore it, or unstore every other.",
            "home": self.home.as_ref().map(|h| h.display().to_string()).ok(),
            "here": self.here.display().to_string(),
        })
    }
}

#[async_trait]
impl Bundle for UserStore {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({
            "list": "No args. Returns {home, here: {path, stored}, keys: [{ref, host}] (never values), templates: [name], locations: [{id, path, name, stored_at, last_seen, here, exists}]}.",
            "store_here": "args: name:str? (default: the folder's name). Stores this fantastic's location. Returns {stored, id}.",
            "unstore": "args: path:str? (default: this fantastic). Returns {unstored}.",
            "unstore_others": "No args. Unstores every location but this one. Returns {unstored: [path]}.",
        })
    }

    async fn handle(&self, _agent_id: &AgentId, payload: &Value, _kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let out = match verb {
            "boot" => {
                self.seen();
                return Ok(None);
            }
            "reflect" => return Ok(Some(self.reflect())),
            "list" => self.list(),
            "store_here" => self.store_here(payload),
            "unstore" => self.unstore(payload),
            "unstore_others" => self.unstore_others(),
            _ => {
                let mut r = refusal(UNHANDLED, format!("user_store: unknown type {verb:?}"));
                with(&mut r, "verb", json!(verb));
                Err(r)
            }
        };
        Ok(Some(out.unwrap_or_else(|r| r)))
    }

    /// Deleting the agent never deletes what the user stored.
    async fn on_delete(&self, _agent_id: &AgentId, _kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let d = tempfile::tempdir().unwrap().keep();
        d.join("home")
    }

    #[test]
    fn store_list_unstore_and_unstore_others() {
        let h = home();
        let a = h.parent().unwrap().join("a");
        let b = h.parent().unwrap().join("b");
        let sa = UserStore::new(Ok(h.clone()), a.clone());
        let sb = UserStore::new(Ok(h.clone()), b.clone());
        assert_eq!(sa.list().unwrap()["locations"], json!([]));
        assert!(!h.exists(), "a list creates nothing");
        sa.store_here(&json!({})).unwrap();
        sb.store_here(&json!({"name": "bee"})).unwrap();
        let l = sa.list().unwrap();
        assert_eq!(l["here"]["stored"], true);
        let names: Vec<&str> = l["locations"].as_array().unwrap().iter().map(|x| x["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"a") && names.contains(&"bee"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(locations_dir(&h)).unwrap().permissions().mode() & 0o777, 0o700);
        // a stores again: the name kept
        sb.store_here(&json!({})).unwrap();
        assert!(sb.list().unwrap()["locations"].as_array().unwrap().iter().any(|x| x["name"] == "bee"));
        let gone = sa.unstore_others().unwrap();
        assert_eq!(gone["unstored"], json!([b.display().to_string()]));
        assert_eq!(sa.list().unwrap()["locations"].as_array().unwrap().len(), 1);
        sa.unstore(&json!({})).unwrap();
        assert_eq!(sa.list().unwrap()["here"]["stored"], false);
        assert!(sa.unstore(&json!({})).is_err(), "not stored: refused");
    }

    #[test]
    fn keys_and_templates_are_listed_from_the_ai_agent_dir() {
        let h = home();
        let s = Secrets::new(Ok(h.clone()));
        s.set_mode(crate::secrets::Mode::Plain).unwrap();
        s.remember_ref("vllm/box:8000", "box:8000").unwrap();
        let st = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("p"));
        let l = st.list().unwrap();
        assert_eq!(l["keys"], json!([{"ref": "vllm/box:8000", "host": "box:8000"}]));
        assert!(h.join("agents/ai_agent/keys/refs.json").exists());
    }
}
