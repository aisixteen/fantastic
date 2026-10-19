//! user_store — THE USER STORE (`user_store.tools`, owner 2026-10-12): what the
//! user keeps across every fantastic they run, in the home dir:
//!
//! ```text
//!   <home>/
//!   ├── user/keys/                 the stored keys (refs + hosts shown, never values)
//!   ├── user/setups/ai_agent/      the saved set-ups (names shown)
//!   └── fantastics/                0700   one FOLDER per registered fantastic
//!       └── <guid | name>/         0700   the kernel's guid, or the name given it
//!           └── fantastic.yaml     0600   {guid, name, path, pid, canvas, stored_at, last_seen}
//! ```
//!
//! A folder is found by the `guid` in its `fantastic.yaml`, never by its name, so
//! a rename keeps it. This is what a local manager of fantastics reads: `path`
//! (the project dir), `pid` (the process that wrote it last), `canvas` (the URL
//! this run serves, `null` when none — set by the binary when it serves one).
//!
//! THIS fantastic's folder follows ONE switch, the record's `register_home_folder`
//! (absent = off): `true` writes the folder, `false` removes it. THE RECORD IS THE
//! TRUTH and lands on disk FIRST (the record writer drained), the home after it;
//! every boot makes the home match the record — on, the folder is written again
//! (`last_seen` and `pid` move, a folder removed meanwhile comes back); off, a
//! folder of this guid left behind is removed. A crash between the two writes
//! is healed by the next boot. Its NAME is the record's `home_folder_name` (absent = the guid);
//! naming it renames the folder. At a clean stop (the kernel's `shutdown`) an
//! existing folder's `pid` and `canvas` are set to `null`: a stopped fantastic
//! reads as stopped from the file alone (a killed one still names its last pid).
//!
//! Verbs: `list`; `register_home_folder {value}`; `name_home_folder {name}`;
//! `set_canvas {url}`; `unstore {guid}` (another fantastic), `unstore_others`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{missing, refusal, refusal_hint, with, BAD_ARG, NOT_FOUND, UNHANDLED};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::{private, private_dirs, read_private, secrets::Secrets, saved_dir, valid_name, write_private};

/// The `handler_module` this bundle answers to.
pub const HANDLER_MODULE: &str = "user_store.tools";

/// The record field holding the switch; absent or anything but `true` = off.
pub const SWITCH: &str = "register_home_folder";

/// The record field holding this fantastic's folder name; absent = its guid.
pub const NAME: &str = "home_folder_name";

/// The file in each fantastic's folder.
pub const INFO: &str = "fantastic.yaml";

/// The fantastics dir under the home.
pub fn fantastics_dir(home: &Path) -> PathBuf {
    home.join("fantastics")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn io(p: &Path) -> impl Fn(std::io::Error) -> Value + '_ {
    move |e| refusal(BAD_ARG, format!("user_store: {}: {e}", p.display()))
}

/// This fantastic as the kernel says it is, read per call: the root's `guid`,
/// and the switch and name off the agent's own record.
struct Me {
    guid: Option<String>,
    on: bool,
    name: Option<String>,
}

impl Me {
    fn of(agent_id: &AgentId, kernel: &Kernel) -> Self {
        let guid = kernel
            .root()
            .and_then(|r| r.record().meta.get("guid").and_then(Value::as_str).map(str::to_string))
            .filter(|g| !g.is_empty());
        let meta = kernel.agents.get(agent_id).map(|a| a.value().record().meta.clone()).unwrap_or_default();
        Self {
            guid,
            on: meta.get(SWITCH) == Some(&Value::Bool(true)),
            name: meta.get(NAME).and_then(Value::as_str).filter(|n| !n.is_empty()).map(str::to_string),
        }
    }

    fn guid(&self) -> Result<&str, Value> {
        self.guid.as_deref().ok_or_else(|| refusal(NOT_FOUND, "user_store: the root carries no guid — nothing names this fantastic's folder"))
    }

    /// The folder's name: the one given, else the guid.
    fn folder(&self) -> Result<String, Value> {
        Ok(self.name.clone().unwrap_or(self.guid()?.to_string()))
    }
}

/// The store. `here` is this fantastic's project dir, fixed by the binary;
/// `canvas` the URL this run serves, set by the binary (RAM: a run's, never a record's).
pub struct UserStore {
    home: Result<PathBuf, String>,
    here: PathBuf,
    canvas: Mutex<Option<String>>,
}

impl UserStore {
    /// `home`: the resolved home dir (or why there is none); `here`: this fantastic's dir.
    pub fn new(home: Result<PathBuf, String>, here: PathBuf) -> Self {
        Self { home, here, canvas: Mutex::new(None) }
    }

    fn home(&self) -> Result<&Path, Value> {
        self.home.as_deref().map_err(|e| refusal(NOT_FOUND, format!("user_store: {e}")))
    }

    /// Every fantastic's folder with its `fantastic.yaml`, oldest stored first. A
    /// folder without one is not a fantastic's.
    fn folders(&self) -> Result<Vec<(PathBuf, Value)>, Value> {
        let dir = fantastics_dir(self.home()?);
        if !dir.exists() {
            return Ok(Vec::new());
        }
        private(&dir, true)?;
        let mut out = Vec::new();
        for e in fs::read_dir(&dir).map_err(io(&dir))?.flatten() {
            let p = e.path();
            if !p.is_dir() {
                continue;
            }
            let Some(bytes) = read_private(&p.join(INFO))? else { continue };
            let Ok(v) = fantastic_kernel::yaml_io::load(&String::from_utf8_lossy(&bytes)) else { continue };
            out.push((p, v));
        }
        out.sort_by_key(|(_, v)| v["stored_at"].as_u64().unwrap_or(0));
        Ok(out)
    }

    /// The folder whose `fantastic.yaml` names `guid`.
    fn find(&self, guid: &str) -> Result<Option<PathBuf>, Value> {
        Ok(self.folders()?.into_iter().find(|(_, v)| v["guid"] == guid).map(|(p, _)| p))
    }

    fn locations(&self, me: &Me) -> Result<Vec<Value>, Value> {
        Ok(self
            .folders()?
            .into_iter()
            .map(|(p, mut v)| {
                let path = v["path"].as_str().unwrap_or("").to_string();
                let here = me.guid.is_some() && v["guid"].as_str() == me.guid.as_deref();
                if let Some(o) = v.as_object_mut() {
                    o.insert("folder".into(), json!(p.file_name().and_then(|s| s.to_str()).unwrap_or("")));
                    o.insert("here".into(), json!(here));
                    o.insert("exists".into(), json!(Path::new(&path).join(".fantastic").is_dir()));
                }
                v
            })
            .collect())
    }

    fn list(&self, me: &Me) -> Result<Value, Value> {
        let home = self.home()?;
        let keys: Vec<Value> = Secrets::new(Ok(home.to_path_buf()))
            .refs()
            .map_err(|e| refusal(BAD_ARG, format!("user_store: {e}")))?
            .into_iter()
            .map(|(r, h)| json!({"ref": r, "host": h}))
            .collect();
        let tdir = saved_dir(home);
        let mut saved: Vec<String> = fs::read_dir(&tdir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".yaml")).map(str::to_string))
                    .filter(|n| valid_name(n))
                    .collect()
            })
            .unwrap_or_default();
        saved.sort();
        let locations = self.locations(me)?;
        let folder = locations.iter().find(|l| l["here"] == true).map(|l| l["folder"].clone());
        Ok(json!({
            "home": home.display().to_string(),
            "here": {"path": self.here.display().to_string(), "guid": me.guid, "name": me.name,
                     "folder": folder, "stored": folder.is_some(), SWITCH: me.on},
            "keys": keys,
            "saved": saved,
            "locations": locations,
        }))
    }

    /// Write this fantastic's folder: found by its guid and renamed to its name
    /// (or guid) when that differs, created when missing; a folder of that name
    /// that is another's is refused. `stored_at` kept; `last_seen`, `pid` and
    /// `canvas` now.
    fn write_here(&self, me: &Me) -> Result<Value, Value> {
        let (target, at) = self.place(me)?;
        let (home, guid, name) = (self.home()?, me.guid()?, me.folder()?);
        if let Some(at) = at.filter(|a| *a != target) {
            fs::rename(&at, &target).map_err(io(&target))?;
        }
        private_dirs(home, &target)?;
        let file = target.join(INFO);
        let was = read_private(&file)?.and_then(|b| fantastic_kernel::yaml_io::load(&String::from_utf8_lossy(&b)).ok());
        let t = now();
        let stored_at = was.as_ref().and_then(|w| w["stored_at"].as_u64()).unwrap_or(t);
        let canvas = self.canvas.lock().expect("canvas poisoned").clone();
        let info = json!({
            "guid": guid, "name": me.name, "path": self.here.display().to_string(),
            "pid": std::process::id(), "canvas": canvas, "stored_at": stored_at, "last_seen": t,
        });
        write_private(&file, fantastic_kernel::yaml_io::dump(&info).as_bytes())?;
        let mut out = info;
        with(&mut out, "folder", json!(name));
        Ok(out)
    }

    /// Where this fantastic's folder goes, and where it is now — refused, touching
    /// nothing, when there is no home or guid, or the name is another's folder.
    fn place(&self, me: &Me) -> Result<(PathBuf, Option<PathBuf>), Value> {
        let name = me.folder()?;
        let target = fantastics_dir(self.home()?).join(&name);
        let at = self.find(me.guid()?)?;
        if target.exists() && at.as_ref() != Some(&target) {
            let mut r = refusal(BAD_ARG, format!("user_store: the folder {name:?} is another fantastic's (or not one) — pick another name"));
            with(&mut r, "arg", json!("name"));
            return Err(r);
        }
        Ok((target, at))
    }

    /// THE RECORD FIRST, ON DISK: `key` patched and the record writer drained, so
    /// the switch or name has landed before the home is touched. A crash after it
    /// leaves a record the next boot makes the home match.
    async fn persist(agent_id: &AgentId, kernel: &Arc<Kernel>, key: &str, value: Value) {
        let mut patch = Map::new();
        patch.insert(key.to_string(), value);
        kernel.update(agent_id, patch);
        kernel.flush_records().await;
    }

    /// Remove a fantastic's folder: its `fantastic.yaml`, then the folder — one
    /// holding anything else is refused, never emptied.
    fn remove_folder(dir: &Path) -> Result<(), Value> {
        let file = dir.join(INFO);
        let others: Vec<String> = fs::read_dir(dir)
            .map_err(io(dir))?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != INFO)
            .collect();
        if !others.is_empty() {
            return Err(refusal(BAD_ARG, format!("user_store: {} holds more than {INFO} ({}) — left as it is", dir.display(), others.join(", "))));
        }
        fs::remove_file(&file).map_err(io(&file))?;
        fs::remove_dir(dir).map_err(io(dir))
    }

    /// The switch: checked against the home first (no home, no guid, a taken name:
    /// refused, nothing touched), then the RECORD lands, then the home follows —
    /// a home write that fails puts the switch back.
    async fn register_home_folder(&self, agent_id: &AgentId, kernel: &Arc<Kernel>, me: &Me, payload: &Value) -> Result<Value, Value> {
        let on = match payload.get("value") {
            None | Some(Value::Null) => return Err(missing(SWITCH, "value")),
            Some(Value::Bool(b)) => *b,
            Some(v) => {
                let mut r = refusal(BAD_ARG, format!("{SWITCH}: value must be true or false, got {v}"));
                with(&mut r, "arg", json!("value"));
                return Err(r);
            }
        };
        let set = Me { guid: me.guid.clone(), on, name: me.name.clone() };
        let at = if on { self.place(&set)?.1 } else { self.find(set.guid()?)? };
        Self::persist(agent_id, kernel, SWITCH, Value::Bool(on)).await;
        let done = if on {
            self.write_here(&set).map(|s| json!({"stored": s}))
        } else {
            at.as_deref().map_or(Ok(()), Self::remove_folder).map(|()| json!({"unstored": at.is_some()}))
        };
        match done {
            Ok(mut out) => {
                with(&mut out, SWITCH, Value::Bool(on));
                Ok(out)
            }
            Err(e) => {
                Self::persist(agent_id, kernel, SWITCH, Value::Bool(me.on)).await;
                Err(e)
            }
        }
    }

    /// The name: with the switch on, checked against the home first (a taken name:
    /// refused, nothing touched), then the RECORD lands, then the folder is
    /// renamed — a rename that fails puts the name back. `null` (or "") = the guid.
    async fn name_home_folder(&self, agent_id: &AgentId, kernel: &Arc<Kernel>, me: &Me, payload: &Value) -> Result<Value, Value> {
        let name = match payload.get("name") {
            None => return Err(missing("name_home_folder", "name")),
            Some(Value::Null) => None,
            Some(Value::String(n)) if n.is_empty() => None,
            Some(Value::String(n)) if valid_name(n) => Some(n.clone()),
            Some(v) => {
                let mut r = refusal(
                    BAD_ARG,
                    format!("name_home_folder: name {v} is not a folder name — 1–64 of letters, digits, '-' and '_', starting with a letter or digit; null for the guid"),
                );
                with(&mut r, "arg", json!("name"));
                return Err(r);
            }
        };
        let named = Me { guid: me.guid.clone(), on: me.on, name: name.clone() };
        let mut out = json!({"name": name, "folder": named.folder()?});
        if me.on {
            self.place(&named)?;
        }
        Self::persist(agent_id, kernel, NAME, name.map_or(Value::Null, Value::String)).await;
        if me.on {
            match self.write_here(&named) {
                Ok(s) => with(&mut out, "stored", s),
                Err(e) => {
                    Self::persist(agent_id, kernel, NAME, me.name.clone().map_or(Value::Null, Value::String)).await;
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    /// The canvas this run serves (`null`: none): kept for the run, and with the
    /// switch on written into the folder at once.
    fn set_canvas(&self, me: &Me, payload: &Value) -> Result<Value, Value> {
        let url = match payload.get("url") {
            None => return Err(missing("set_canvas", "url")),
            Some(Value::Null) => None,
            Some(Value::String(u)) if !u.is_empty() => Some(u.clone()),
            Some(v) => {
                let mut r = refusal(BAD_ARG, format!("set_canvas: url must be a non-empty string or null, got {v}"));
                with(&mut r, "arg", json!("url"));
                return Err(r);
            }
        };
        let was = std::mem::replace(&mut *self.canvas.lock().expect("canvas poisoned"), url.clone());
        let mut out = json!({"canvas": url});
        if me.on {
            match self.write_here(me) {
                Ok(s) => with(&mut out, "stored", s),
                Err(e) => {
                    *self.canvas.lock().expect("canvas poisoned") = was;
                    return Err(e);
                }
            }
        }
        Ok(out)
    }

    fn unstore(&self, me: &Me, payload: &Value) -> Result<Value, Value> {
        let guid = payload.get("guid").and_then(Value::as_str).filter(|g| !g.is_empty()).ok_or_else(|| missing("unstore", "guid"))?;
        if me.guid.as_deref() == Some(guid) {
            let mut r = refusal_hint(
                BAD_ARG,
                format!("unstore: {guid} is this fantastic — its folder follows {SWITCH}"),
                format!("send {{\"type\": \"{SWITCH}\", \"value\": false}}"),
            );
            with(&mut r, "arg", json!("guid"));
            return Err(r);
        }
        let Some(dir) = self.find(guid)? else {
            let mut r = refusal(NOT_FOUND, format!("unstore: no fantastic {guid} is stored"));
            with(&mut r, "arg", json!("guid"));
            return Err(r);
        };
        Self::remove_folder(&dir)?;
        Ok(json!({"unstored": guid}))
    }

    fn unstore_others(&self, me: &Me) -> Result<Value, Value> {
        let mut gone = Vec::new();
        for (dir, v) in self.folders()? {
            if me.guid.is_some() && v["guid"].as_str() == me.guid.as_deref() {
                continue;
            }
            Self::remove_folder(&dir)?;
            gone.push(v["guid"].clone());
        }
        Ok(json!({"unstored": gone}))
    }

    /// At a clean stop, with the switch on: an existing folder says the run is
    /// over — `pid` and `canvas` null, `last_seen` now. A folder removed meanwhile
    /// stays removed. The run's canvas is forgotten.
    fn shutdown(&self, me: &Me) -> Result<Value, Value> {
        *self.canvas.lock().expect("canvas poisoned") = None;
        if !me.on {
            return Ok(json!({"stopped": false}));
        }
        let Some(dir) = self.find(me.guid()?)? else { return Ok(json!({"stopped": false})) };
        let file = dir.join(INFO);
        let Some(bytes) = read_private(&file)? else { return Ok(json!({"stopped": false})) };
        let mut info = fantastic_kernel::yaml_io::load(&String::from_utf8_lossy(&bytes))
            .map_err(|e| refusal(BAD_ARG, format!("user_store: {}: {e}", file.display())))?;
        if let Some(o) = info.as_object_mut() {
            o.insert("pid".into(), Value::Null);
            o.insert("canvas".into(), Value::Null);
            o.insert("last_seen".into(), json!(now()));
        }
        write_private(&file, fantastic_kernel::yaml_io::dump(&info).as_bytes())?;
        Ok(json!({"stopped": true}))
    }

    /// On boot, with the switch on: this fantastic's folder written again. The
    /// reply is the boot's outcome — a home that cannot be written answers its
    /// refusal.
    ///
    /// Off, a folder of this guid left by a stop between the record and the home
    /// is removed — the record is the truth; every boot makes the home match it.
    /// Off with no such folder (or no home, no guid), a boot touches nothing.
    fn boot(&self, me: &Me) -> Reply {
        if !me.on {
            let at = match (self.home(), me.guid()) {
                (Ok(_), Ok(g)) => self.find(g),
                _ => return None,
            };
            return match at {
                Ok(None) => None,
                Ok(Some(d)) => Some(Self::remove_folder(&d).map_or_else(|r| r, |()| json!({"unstored": true}))),
                Err(r) => Some(r),
            };
        }
        Some(self.write_here(me).map_or_else(|r| r, |info| json!({"stored": info})))
    }

    fn reflect(&self, me: &Me) -> Value {
        json!({
            "handler_module": HANDLER_MODULE,
            "sentence": "The user store: the keys and set-ups kept in the user's home, and a folder per registered fantastic — register this one (kept on every start while on), name its folder, unstore another, or unstore every other.",
            "home": self.home.as_ref().map(|h| h.display().to_string()).ok(),
            "here": self.here.display().to_string(),
            SWITCH: me.on,
            NAME: me.name,
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
            "list": "No args. Returns {home, here: {path, guid, name, folder, stored, register_home_folder}, keys: [{ref, host}] (never values), saved: [name], locations: [{folder, guid, name, path, pid, canvas, stored_at, last_seen, here, exists}]}.",
            "register_home_folder": "args: value:bool. On: records the switch and writes this fantastic's folder (<home>/fantastics/<name or guid>/fantastic.yaml), written again on every start while on; off: records it and removes the folder. Default off. Returns {register_home_folder, stored|unstored}.",
            "name_home_folder": "args: name:str|null (letters, digits, '-', '_'; null = the guid). Records the folder's name; with the switch on the folder is renamed now. Returns {name, folder, stored?}.",
            "set_canvas": "args: url:str|null. The canvas this run serves, written into the folder (with the switch on). Returns {canvas, stored?}.",
            "unstore": "args: guid:str (another fantastic's; this one follows register_home_folder). Removes its folder. Returns {unstored}.",
            "unstore_others": "No args. Removes every fantastic's folder but this one's. Returns {unstored: [guid]}.",
            "shutdown": "Kernel lifecycle — the kernel sends it at a clean stop. With the switch on, this fantastic's folder gets pid and canvas null. Returns {stopped}.",
        })
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let me = Me::of(agent_id, kernel);
        let out = match verb {
            "boot" => return Ok(self.boot(&me)),
            "reflect" => return Ok(Some(self.reflect(&me))),
            "list" => self.list(&me),
            SWITCH => self.register_home_folder(agent_id, kernel, &me, payload).await,
            "name_home_folder" => self.name_home_folder(agent_id, kernel, &me, payload).await,
            "set_canvas" => self.set_canvas(&me, payload),
            "unstore" => self.unstore(&me, payload),
            "unstore_others" => self.unstore_others(&me),
            "shutdown" => self.shutdown(&me),
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
    use fantastic_kernel::agent::Agent;

    use super::*;

    fn home() -> PathBuf {
        let d = tempfile::tempdir().unwrap().keep();
        d.join("home")
    }

    fn agent(id: &str, hm: Option<&str>, parent: Option<&str>, meta: Value) -> Arc<Agent> {
        Agent::new(AgentId::from(id), hm.map(str::to_string), parent.map(AgentId::from), meta.as_object().cloned().unwrap_or_default(), PathBuf::new(), false)
    }

    /// One fantastic: a root carrying `guid` (`None`: no guid) and a `user_store`
    /// whose record is `meta` — what a boot hydrates from disk.
    fn fantastic(guid: Option<&str>, meta: Value) -> (Arc<Kernel>, AgentId) {
        let k = Arc::new(Kernel::new());
        let root = agent("kernel_state", None, None, guid.map_or(json!({}), |g| json!({"guid": g})));
        k.register(Arc::clone(&root));
        k.set_root(root);
        let id = AgentId::from("user_store");
        k.register(agent("user_store", Some(HANDLER_MODULE), Some("kernel_state"), meta));
        (k, id)
    }

    async fn call(s: &UserStore, k: &Arc<Kernel>, id: &AgentId, p: Value) -> Value {
        s.handle(id, &p, k).await.unwrap().unwrap_or(Value::Null)
    }

    fn info(h: &Path, folder: &str) -> Option<Value> {
        let f = fantastics_dir(h).join(folder).join(INFO);
        f.exists().then(|| fantastic_kernel::yaml_io::load(&fs::read_to_string(f).unwrap()).unwrap())
    }

    fn folders(h: &Path) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(fantastics_dir(h)).map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
        v.sort();
        v
    }

    fn meta(k: &Kernel, id: &AgentId) -> Map<String, Value> {
        k.agents.get(id).unwrap().record().meta.clone()
    }

    #[tokio::test]
    async fn off_by_default_a_boot_writes_nothing() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("p"));
        let (k, id) = fantastic(Some("g-1"), json!({}));
        assert_eq!(call(&s, &k, &id, json!({"type": "boot"})).await, Value::Null);
        assert!(!h.exists(), "an off boot creates nothing in the home");
        let r = call(&s, &k, &id, json!({"type": "reflect"})).await;
        assert_eq!((r[SWITCH].clone(), r[NAME].clone()), (json!(false), Value::Null));
        let l = call(&s, &k, &id, json!({"type": "list"})).await;
        assert_eq!(l["here"]["guid"], "g-1");
        assert_eq!((l["here"]["stored"].clone(), l["here"]["folder"].clone()), (json!(false), Value::Null));
        assert!(!h.exists(), "a list creates nothing");
    }

    #[tokio::test]
    async fn on_writes_a_folder_named_by_the_guid_off_removes_it() {
        let h = home();
        let here = h.parent().unwrap().join("proj");
        let s = UserStore::new(Ok(h.clone()), here.clone());
        let (k, id) = fantastic(Some("g-1"), json!({}));
        let r = call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        assert_eq!(r[SWITCH], true, "{r}");
        assert_eq!(r["stored"]["folder"], "g-1");
        assert_eq!(folders(&h), vec!["g-1"], "one folder, named by the guid");
        let i = info(&h, "g-1").expect("fantastic.yaml in it");
        assert_eq!(i["guid"], "g-1");
        assert_eq!(i["name"], Value::Null);
        assert_eq!(i["path"], json!(here.display().to_string()));
        assert_eq!(i["pid"], json!(std::process::id()));
        assert_eq!(i["canvas"], Value::Null, "no canvas served");
        assert!(i["stored_at"].as_u64().is_some() && i["last_seen"].as_u64().is_some());
        for (p, dir) in [(fantastics_dir(&h), true), (fantastics_dir(&h).join("g-1"), true), (fantastics_dir(&h).join("g-1").join(INFO), false)] {
            assert_eq!(crate::sys::mode_of(&p).unwrap(), if dir { 0o700 } else { 0o600 }, "{}", p.display());
        }
        assert_eq!(meta(&k, &id)[SWITCH], true, "the switch is on the record");
        let l = call(&s, &k, &id, json!({"type": "list"})).await;
        assert_eq!(l["here"]["folder"], "g-1");
        assert_eq!(l["locations"][0]["here"], true);

        let r = call(&s, &k, &id, json!({"type": SWITCH, "value": false})).await;
        assert_eq!(r, json!({"unstored": true, SWITCH: false}));
        assert_eq!(meta(&k, &id)[SWITCH], false);
        assert!(folders(&h).is_empty(), "off removes the folder");
        assert_eq!(call(&s, &k, &id, json!({"type": SWITCH, "value": false})).await["unstored"], false);
    }

    #[tokio::test]
    async fn a_boot_with_the_switch_on_writes_the_folder_again() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        // a restart: the record comes back from disk with the switch on and a name
        let (k, id) = fantastic(Some("g-1"), json!({SWITCH: true, NAME: "work"}));
        let r = call(&s, &k, &id, json!({"type": "boot"})).await;
        assert_eq!(r["stored"]["folder"], "work", "{r}");
        let first = info(&h, "work").expect("written on boot, under its name");
        assert_eq!(first["name"], "work");
        call(&s, &k, &id, json!({"type": "boot"})).await;
        let again = info(&h, "work").unwrap();
        assert_eq!(again["stored_at"], first["stored_at"], "stored_at kept");
        assert!(again["last_seen"].as_u64() >= first["last_seen"].as_u64());
        fs::remove_dir_all(fantastics_dir(&h).join("work")).unwrap();
        call(&s, &k, &id, json!({"type": "boot"})).await;
        assert!(info(&h, "work").is_some(), "a removed folder comes back while the switch is on");
    }

    #[tokio::test]
    async fn naming_renames_the_folder_and_null_gives_the_guid_back() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        let (k, id) = fantastic(Some("g-1"), json!({}));
        call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        let stored_at = info(&h, "g-1").unwrap()["stored_at"].clone();
        let r = call(&s, &k, &id, json!({"type": "name_home_folder", "name": "my-app"})).await;
        assert_eq!((r["name"].clone(), r["folder"].clone()), (json!("my-app"), json!("my-app")), "{r}");
        assert_eq!(folders(&h), vec!["my-app"], "renamed, not copied");
        let i = info(&h, "my-app").unwrap();
        assert_eq!((i["name"].clone(), i["guid"].clone()), (json!("my-app"), json!("g-1")));
        assert_eq!(i["stored_at"], stored_at, "the same folder, moved");
        assert_eq!(meta(&k, &id)[NAME], "my-app");
        assert_eq!(call(&s, &k, &id, json!({"type": "reflect"})).await[NAME], "my-app");

        let r = call(&s, &k, &id, json!({"type": "name_home_folder", "name": null})).await;
        assert_eq!(r["folder"], "g-1", "{r}");
        assert_eq!(folders(&h), vec!["g-1"]);
        assert_eq!(info(&h, "g-1").unwrap()["name"], Value::Null);
        assert_eq!(meta(&k, &id)[NAME], Value::Null);
    }

    #[tokio::test]
    async fn naming_while_off_only_records_the_name_switching_on_uses_it() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        let (k, id) = fantastic(Some("g-1"), json!({}));
        let r = call(&s, &k, &id, json!({"type": "name_home_folder", "name": "later"})).await;
        assert_eq!(r, json!({"name": "later", "folder": "later"}));
        assert!(!h.exists(), "off: nothing written");
        call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        assert_eq!(folders(&h), vec!["later"]);
    }

    #[tokio::test]
    async fn a_bad_or_taken_name_is_refused_and_nothing_moves() {
        let h = home();
        let (ka, a) = fantastic(Some("g-a"), json!({}));
        let (kb, b) = fantastic(Some("g-b"), json!({}));
        let sa = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("a"));
        let sb = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("b"));
        call(&sa, &ka, &a, json!({"type": SWITCH, "value": true})).await;
        call(&sb, &kb, &b, json!({"type": SWITCH, "value": true})).await;
        call(&sb, &kb, &b, json!({"type": "name_home_folder", "name": "bee"})).await;
        for bad in [json!("../up"), json!("a b"), json!("-x"), json!(7), json!("x".repeat(65))] {
            let r = call(&sa, &ka, &a, json!({"type": "name_home_folder", "name": bad})).await;
            assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some(BAD_ARG), Some("name")), "{bad}: {r}");
        }
        let r = call(&sa, &ka, &a, json!({"type": "name_home_folder"})).await;
        assert_eq!(r["reason"], "missing_arg");
        // another fantastic's folder, by name or by guid
        for taken in ["bee", "g-b"] {
            fs::create_dir_all(fantastics_dir(&h).join("g-b")).ok();
            let r = call(&sa, &ka, &a, json!({"type": "name_home_folder", "name": taken})).await;
            assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some(BAD_ARG), Some("name")), "{taken}: {r}");
            let _ = fs::remove_dir(fantastics_dir(&h).join("g-b"));
        }
        assert_eq!(folders(&h), vec!["bee", "g-a"], "nothing moved");
        assert!(!meta(&ka, &a).contains_key(NAME), "the record untouched");
    }

    #[tokio::test]
    async fn the_folder_is_found_by_its_guid_not_its_name() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        let (k, id) = fantastic(Some("g-1"), json!({SWITCH: true}));
        call(&s, &k, &id, json!({"type": "boot"})).await;
        // renamed by hand: the next write finds it by guid and puts it back
        fs::rename(fantastics_dir(&h).join("g-1"), fantastics_dir(&h).join("moved")).unwrap();
        call(&s, &k, &id, json!({"type": "boot"})).await;
        assert_eq!(folders(&h), vec!["g-1"]);
    }

    #[tokio::test]
    async fn the_canvas_is_the_runs_and_written_with_the_switch_on() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        let (k, id) = fantastic(Some("g-1"), json!({}));
        let url = "http://127.0.0.1:8123/canvas_page/file/index.html";
        assert_eq!(call(&s, &k, &id, json!({"type": "set_canvas", "url": url})).await, json!({"canvas": url}));
        assert!(!h.exists(), "off: kept for the run, nothing written");
        call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        assert_eq!(info(&h, "g-1").unwrap()["canvas"], url);
        let r = call(&s, &k, &id, json!({"type": "set_canvas", "url": null})).await;
        assert_eq!(r["stored"]["canvas"], Value::Null, "{r}");
        assert_eq!(info(&h, "g-1").unwrap()["canvas"], Value::Null);
        assert!(!meta(&k, &id).contains_key("canvas"), "never on the record");
        for bad in [json!({"type": "set_canvas"}), json!({"type": "set_canvas", "url": ""}), json!({"type": "set_canvas", "url": 1})] {
            assert_eq!(call(&s, &k, &id, bad.clone()).await["arg"], "url", "{bad}");
        }
    }

    #[tokio::test]
    async fn unstore_removes_anothers_folder_never_this_ones_nor_a_full_one() {
        let h = home();
        let (ka, a) = fantastic(Some("g-a"), json!({}));
        let sa = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("a"));
        for g in ["g-b", "g-c", "g-d"] {
            let (k, id) = fantastic(Some(g), json!({}));
            let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join(g));
            call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        }
        call(&sa, &ka, &a, json!({"type": SWITCH, "value": true})).await;
        assert_eq!(call(&sa, &ka, &a, json!({"type": "unstore", "guid": "g-b"})).await, json!({"unstored": "g-b"}));
        assert_eq!(call(&sa, &ka, &a, json!({"type": "unstore", "guid": "g-b"})).await["reason"], NOT_FOUND);
        assert_eq!(call(&sa, &ka, &a, json!({"type": "unstore"})).await["arg"], "guid");
        let r = call(&sa, &ka, &a, json!({"type": "unstore", "guid": "g-a"})).await;
        assert!(r["hint"].as_str().unwrap().contains(SWITCH), "{r}");
        // a folder holding something else is refused, never emptied
        fs::write(fantastics_dir(&h).join("g-c").join("notes.txt"), "mine").unwrap();
        assert_eq!(call(&sa, &ka, &a, json!({"type": "unstore", "guid": "g-c"})).await["reason"], BAD_ARG);
        assert!(fantastics_dir(&h).join("g-c").join("notes.txt").exists());
        assert!(info(&h, "g-c").is_some(), "its fantastic.yaml untouched too");
        fs::remove_file(fantastics_dir(&h).join("g-c").join("notes.txt")).unwrap();
        let mut gone = call(&sa, &ka, &a, json!({"type": "unstore_others"})).await["unstored"].as_array().unwrap().clone();
        gone.sort_by_key(|g| g.to_string());
        assert_eq!(gone, vec![json!("g-c"), json!("g-d")]);
        assert_eq!(folders(&h), vec!["g-a"]);
    }

    #[tokio::test]
    async fn a_clean_stop_nulls_pid_and_canvas_and_the_next_boot_fills_them() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        let (k, id) = fantastic(Some("g-1"), json!({SWITCH: true}));
        call(&s, &k, &id, json!({"type": "set_canvas", "url": "http://127.0.0.1:9/canvas_page/file/index.html"})).await;
        let before = info(&h, "g-1").unwrap();
        assert!(before["pid"].is_u64() && before["canvas"].is_string());
        // the kernel's stop tells it: the bundle advertises the verb
        assert!(s.verb_docs().as_object().unwrap().contains_key("shutdown"));
        assert_eq!(call(&s, &k, &id, json!({"type": "shutdown"})).await, json!({"stopped": true}));
        let after = info(&h, "g-1").unwrap();
        assert_eq!((after["pid"].clone(), after["canvas"].clone()), (Value::Null, Value::Null));
        assert_eq!((after["guid"].clone(), after["stored_at"].clone(), after["path"].clone()), (before["guid"].clone(), before["stored_at"].clone(), before["path"].clone()));
        // the next start fills the pid again; no canvas until one is served
        call(&s, &k, &id, json!({"type": "boot"})).await;
        let next = info(&h, "g-1").unwrap();
        assert_eq!((next["pid"].clone(), next["canvas"].clone()), (json!(std::process::id()), Value::Null));
        // a folder removed meanwhile stays removed; off touches nothing
        fs::remove_dir_all(fantastics_dir(&h).join("g-1")).unwrap();
        assert_eq!(call(&s, &k, &id, json!({"type": "shutdown"})).await, json!({"stopped": false}));
        assert!(folders(&h).is_empty());
        let (ko, ido) = fantastic(Some("g-2"), json!({}));
        assert_eq!(call(&s, &ko, &ido, json!({"type": "shutdown"})).await, json!({"stopped": false}));
        assert!(folders(&h).is_empty());
    }

    #[tokio::test]
    async fn an_off_boot_removes_a_folder_left_by_a_stop_between_the_two_writes() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        // the record says off, but a folder of this guid is in the home
        let (on, on_id) = fantastic(Some("g-1"), json!({SWITCH: true, NAME: "left"}));
        call(&s, &on, &on_id, json!({"type": "boot"})).await;
        let other = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("other"));
        let (ko, ido) = fantastic(Some("g-2"), json!({SWITCH: true}));
        call(&other, &ko, &ido, json!({"type": "boot"})).await;
        let (k, id) = fantastic(Some("g-1"), json!({SWITCH: false}));
        assert_eq!(call(&s, &k, &id, json!({"type": "boot"})).await, json!({"unstored": true}));
        assert_eq!(folders(&h), vec!["g-2"], "only this guid's folder goes");
        assert_eq!(call(&s, &k, &id, json!({"type": "boot"})).await, Value::Null, "nothing left: touches nothing");
    }

    #[tokio::test]
    async fn a_home_write_that_fails_puts_the_switch_and_the_name_back() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        let (k, id) = fantastic(Some("g-1"), json!({}));
        // the fantastics dir exists, private, but cannot be written into
        crate::private_dirs(&h, &fantastics_dir(&h)).unwrap();
        let ro = |mode| crate::sys::set_mode(&fantastics_dir(&h), mode).unwrap();
        ro(0o500);
        let r = call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        assert_eq!(r["reason"], BAD_ARG, "{r}");
        assert_eq!(meta(&k, &id)[SWITCH], false, "the switch is put back");
        ro(0o700);
        call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        ro(0o500);
        let r = call(&s, &k, &id, json!({"type": "name_home_folder", "name": "nope"})).await;
        assert_eq!(r["reason"], BAD_ARG, "{r}");
        assert_eq!(meta(&k, &id)[NAME], Value::Null, "the name is put back");
        ro(0o700);
        assert_eq!(folders(&h), vec!["g-1"]);
    }

    #[tokio::test]
    async fn a_bad_value_no_home_or_no_guid_leaves_the_switch_as_it_was() {
        let h = home();
        let here = h.parent().unwrap().join("proj");
        let s = UserStore::new(Ok(h.clone()), here.clone());
        let (k, id) = fantastic(Some("g-1"), json!({}));
        let r = call(&s, &k, &id, json!({"type": SWITCH})).await;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("missing_arg"), Some("value")), "{r}");
        let r = call(&s, &k, &id, json!({"type": SWITCH, "value": "yes"})).await;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some(BAD_ARG), Some("value")), "{r}");
        let none = UserStore::new(Err("neither FANTASTIC_HOME nor HOME is set".into()), here);
        assert_eq!(call(&none, &k, &id, json!({"type": SWITCH, "value": true})).await["reason"], NOT_FOUND);
        let (kn, idn) = fantastic(None, json!({}));
        let r = call(&s, &kn, &idn, json!({"type": SWITCH, "value": true})).await;
        assert!(r["error"].as_str().unwrap().contains("guid"), "{r}");
        assert!(!h.exists());
        assert!(!meta(&k, &id).contains_key(SWITCH) && !meta(&kn, &idn).contains_key(SWITCH), "the records untouched");
        // a boot with the switch on and no home answers the refusal
        let (k2, id2) = fantastic(Some("g-2"), json!({SWITCH: true}));
        assert_eq!(call(&none, &k2, &id2, json!({"type": "boot"})).await["reason"], NOT_FOUND);
    }

    #[tokio::test]
    async fn the_switch_and_the_name_persist_as_updated_record_events() {
        let h = home();
        let s = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("proj"));
        let (k, id) = fantastic(Some("g-1"), json!({}));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let tap = Arc::clone(&seen);
        k.add_state_subscriber(Arc::new(move |ev: &Value, _| tap.lock().unwrap().push(ev.clone())));
        call(&s, &k, &id, json!({"type": SWITCH, "value": true})).await;
        call(&s, &k, &id, json!({"type": "name_home_folder", "name": "n"})).await;
        let changed: Vec<Value> = seen.lock().unwrap().iter().filter(|e| e["kind"] == "updated" && e["agent_id"] == "user_store").map(|e| e["changed"].clone()).collect();
        assert_eq!(changed, vec![json!([SWITCH]), json!([NAME])], "the loader persists each changed field");
    }

    #[test]
    fn keys_and_saved_setups_are_listed_from_the_user_dir() {
        let h = home();
        let s = Secrets::new(Ok(h.clone()));
        s.set_mode(crate::secrets::Mode::Plain).unwrap();
        s.remember_ref("vllm/box:8000", "box:8000").unwrap();
        let st = UserStore::new(Ok(h.clone()), h.parent().unwrap().join("p"));
        let me = Me { guid: Some("g".into()), on: false, name: None };
        let l = st.list(&me).unwrap();
        assert_eq!(l["keys"], json!([{"ref": "vllm/box:8000", "host": "box:8000"}]));
        assert!(h.join("user/keys/refs.json").exists());
    }
}
