//! THE `home` AGENT (`home.tools`): the manager's verbs on the kernel protocol.
//!
//! Every check runs before any event is reduced, so a refused call changes
//! nothing. Every intent replies with the state right after its event; the
//! effects run afterwards, and what they change arrives as `state` emits.
//!
//! The verb logic is [`call`], which needs no kernel: tests and the Phase 5
//! contract replay run it directly. [`HomeBundle::handle`] adds the lifecycle
//! (`boot`, `shutdown`) and `reflect`.

use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use fantastic_home::{Config as HomeConfig, Event, Intent, Mode, Row};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{missing, refusal, with, BAD_ARG, NOT_FOUND, UNHANDLED};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use super::os::HomeOs;
use super::runner::{Runner, POLL};
use super::wire::{mode_name, status_name, PROTOCOL, VERSION};
use super::{ID, VERBS};

/// The kernel the agent emits on and keeps its record in, from its first
/// `boot`, and the emitter's queue until then.
struct Link {
    kernel: Mutex<Weak<Kernel>>,
    queue: Mutex<Option<mpsc::UnboundedReceiver<Value>>>,
}

/// The bundle: one runner, one home.
#[derive(Clone)]
pub struct HomeBundle {
    runner: Arc<Runner>,
    link: Arc<Link>,
}

impl std::fmt::Debug for HomeBundle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let config = self.runner.read(|h| h.config().clone());
        f.debug_struct("HomeBundle")
            .field("config", &config)
            .field("polling", &self.runner.polling())
            .finish()
    }
}

impl HomeBundle {
    /// The bundle over `os`. Its emits go to one emitter task, in order, once
    /// the first `boot` names the kernel; `keep` patches `home`'s own record.
    pub fn new(os: Arc<dyn HomeOs>, config: HomeConfig) -> HomeBundle {
        let (tx, rx) = mpsc::unbounded_channel::<Value>();
        let link = Arc::new(Link {
            kernel: Mutex::new(Weak::new()),
            queue: Mutex::new(Some(rx)),
        });
        let emit = Box::new(move |payload: Value| {
            let _ = tx.send(payload);
        });
        let kept = Arc::clone(&link);
        let keep = Box::new(move |pages: Vec<String>, selection: Option<String>| {
            let Some(kernel) = kept.kernel.lock().expect("the link is poisoned").upgrade() else {
                return;
            };
            let mut patch = Map::new();
            patch.insert("pages".into(), json!(pages));
            patch.insert("selection".into(), json!(selection));
            kernel.update(&AgentId::from(ID), patch);
        });
        HomeBundle {
            runner: Runner::new(os, config, emit, keep),
            link,
        }
    }

    /// The runner, for the tests.
    pub fn runner(&self) -> &Arc<Runner> {
        &self.runner
    }

    /// `boot`: the kernel is named, the emitter starts, the kept pages and
    /// selection are restored from the record, and the poll starts. Idempotent.
    fn boot(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        *self.link.kernel.lock().expect("the link is poisoned") = Arc::downgrade(kernel);
        if let Some(mut queue) = self.link.queue.lock().expect("the link is poisoned").take() {
            let weak = Arc::downgrade(kernel);
            tokio::spawn(async move {
                while let Some(payload) = queue.recv().await {
                    let Some(kernel) = weak.upgrade() else { break };
                    kernel.emit(&AgentId::from(ID), payload).await;
                }
            });
        }
        if self.runner.polling() {
            return json!({"booted": true});
        }
        let meta = kernel
            .agents
            .get(agent_id)
            .map(|a| a.value().record().meta.clone())
            .unwrap_or_default();
        let (pages, selection) = match kept_of(&meta) {
            Ok(kept) => kept,
            Err(r) => return r,
        };
        self.runner.set_kept(pages.clone(), selection.clone());
        self.runner.apply(Event::Restore { pages, selection });
        self.runner.start_poll();
        json!({"booted": true})
    }

    fn reflect(&self, agent_id: &AgentId, kernel: &Kernel, payload: &Value) -> Value {
        // The home kernel's running dir is always `<home>/app`.
        let home = kernel
            .workdir()
            .ok()
            .and_then(|w| w.parent().map(|h| h.display().to_string()));
        reflect_call(&self.runner, agent_id.as_str(), home, payload)
    }
}

/// `reflect`, its `version` checked first.
pub(crate) fn reflect_call(
    runner: &Runner,
    id: &str,
    home: Option<String>,
    payload: &Value,
) -> Value {
    match version_of(payload) {
        Ok(()) => reflection(runner, id, home),
        Err(refused) => refused,
    }
}

/// `reflect`'s answer for the agent `id` of the home dir `home`.
pub(crate) fn reflection(runner: &Runner, id: &str, home: Option<String>) -> Value {
    let mode = runner.read(|h| h.config().mode);
    json!({
        "id": id,
        "sentence": "The home: your fantastics, listed with their status — open a folder, start, stop, remove; the open pages and the selection are kept across restarts. Watch `home` for its `state`.",
        "protocol": PROTOCOL,
        "version": VERSION,
        "mode": mode_name(mode),
        "home": home,
        "poll_ms": POLL.as_millis() as u64,
    })
}

/// The protocol version a call asks for: absent (or null) is 1; any other
/// than [`VERSION`] is refused, naming it.
pub(crate) fn version_of(payload: &Value) -> Result<(), Value> {
    match payload.get("version") {
        None | Some(Value::Null) => Ok(()),
        Some(v) if v.as_u64() == Some(u64::from(VERSION)) => Ok(()),
        Some(v) => {
            let asked = match v.as_i64() {
                Some(n) => n.to_string(),
                None => v.to_string(),
            };
            Err(bad(
                format!("home speaks {PROTOCOL} v{VERSION}; this client asked for v{asked}"),
                "version",
            ))
        }
    }
}

/// The record's `pages` and `selection`, as kept; a malformed one is refused.
fn kept_of(meta: &Map<String, Value>) -> Result<(Vec<String>, Option<String>), Value> {
    let pages = match meta.get("pages") {
        None => Vec::new(),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        Some(v) => {
            let mut r = refusal(
                BAD_ARG,
                format!("home: its record's `pages` must be a list of ids, got {v}"),
            );
            with(&mut r, "arg", json!("pages"));
            return Err(r);
        }
    };
    let selection = match meta.get("selection") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(v) => {
            let mut r = refusal(
                BAD_ARG,
                format!("home: its record's `selection` must be an id or null, got {v}"),
            );
            with(&mut r, "arg", json!("selection"));
            return Err(r);
        }
    };
    Ok((pages, selection))
}

fn bad(text: String, arg: &str) -> Value {
    let mut r = refusal(BAD_ARG, text);
    with(&mut r, "arg", json!(arg));
    r
}

fn not_found(text: String, arg: &str) -> Value {
    let mut r = refusal(NOT_FOUND, text);
    with(&mut r, "arg", json!(arg));
    r
}

/// A required string argument.
fn string_arg<'a>(payload: &'a Value, verb: &str, arg: &str) -> Result<&'a str, Value> {
    match payload.get(arg) {
        None | Some(Value::Null) => Err(missing(&format!("home: {verb}"), arg)),
        Some(Value::String(s)) => Ok(s),
        Some(v) => Err(bad(
            format!("home: {verb}: {arg} must be a string, got {v}"),
            arg,
        )),
    }
}

fn row_of(rows: &[Row], id: &str) -> Result<Row, Value> {
    rows.iter()
        .find(|r| r.id == id)
        .cloned()
        .ok_or_else(|| not_found(format!("home: no fantastic \"{id}\" in the list"), "id"))
}

/// One folder for `open` or `register`: a listed row's path as it is, else an
/// absolute path resolved to the folder a kernel there would name — never the
/// home's own.
fn folder_of(
    runner: &Runner,
    rows: &[Row],
    verb: &str,
    arg: &str,
    path: &str,
) -> Result<String, Value> {
    if rows.iter().any(|r| r.path == path) {
        return Ok(path.to_string());
    }
    if !Path::new(path).is_absolute() {
        return Err(bad(
            format!("home: {verb}: \"{path}\" is not an absolute path"),
            arg,
        ));
    }
    let folder = runner.os().folder(path).map_err(|why| {
        let mut r = bad(format!("home: {verb}: \"{path}\" is not a folder"), arg);
        with(&mut r, "hint", json!(why));
        r
    })?;
    let own = runner.read(|h| h.config().own_folder.clone());
    if own.as_deref() == Some(folder.as_str()) {
        return Err(bad(
            format!("home: {verb}: \"{path}\" is the home's own folder — it is never listed"),
            arg,
        ));
    }
    Ok(folder)
}

/// Run one verb of the agent (every verb but the lifecycle and `reflect`),
/// once its `version` is checked.
pub async fn call(runner: &Arc<Runner>, payload: &Value) -> Value {
    if let Err(refused) = version_of(payload) {
        return refused;
    }
    match intent(runner, payload) {
        Ok(Some(event)) => runner.apply(event),
        Ok(None) => runner.state(),
        Err(reply) => reply,
    }
}

/// The checks of one verb, and the event it reduces (`None`: it reduces none).
/// `Err` is the reply: a refusal, or what a reader verb answers.
fn intent(runner: &Arc<Runner>, payload: &Value) -> Result<Option<Event>, Value> {
    let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
    let (rows, mode) = runner.read(|h| (h.rows(runner.os().now_ms()), h.config().mode));
    match verb {
        "list" => Ok(None),
        "open" | "start" | "stop" | "remove" | "select" | "select_page" | "close_page" => {
            let typed = typed(payload, verb)?;
            checked(runner, &rows, mode, typed).map(|i| Some(i.event()))
        }
        "register" => {
            let path = string_arg(payload, "register", "path")?;
            let folder = folder_of(runner, &rows, "register", "path", path)?;
            if let Some(r) = rows.iter().find(|r| r.path == folder) {
                return Err(bad(
                    format!("home: {} is in the list already — open it", r.name),
                    "path",
                ));
            }
            Ok(Some(Event::Open {
                paths: vec![folder],
                now_ms: 0,
            }))
        }
        "dismiss" => Ok(Some(Event::Dismiss)),
        "log" => {
            let id = string_arg(payload, "log", "id")?;
            let row = row_of(&rows, id)?;
            let os = runner.os();
            let Some(file) = os.log_file(&row.path) else {
                return Err(not_found(
                    format!("home: no launch log for {}", row.name),
                    "id",
                ));
            };
            Err(json!({"id": id, "file": file, "tail": os.log_tail(&row.path)}))
        }
        "focus" => Err(runner.focus()),
        "menu" => Err(runner.menu_spec()),
        _ => {
            let mut r = refusal(UNHANDLED, format!("home: unknown type \"{verb}\""));
            with(&mut r, "verb", json!(verb));
            Err(r)
        }
    }
}

/// One of the user's intents, parsed from its verb's payload: every argument
/// present and of its type (the refusal names the one that is not).
fn typed(payload: &Value, verb: &str) -> Result<Intent, Value> {
    let id = || string_arg(payload, verb, "id").map(str::to_string);
    Ok(match verb {
        "open" => {
            let paths = match payload.get("paths") {
                None | Some(Value::Null) => return Err(missing("home: open", "paths")),
                Some(Value::Array(items)) if items.is_empty() => {
                    return Err(missing("home: open", "paths"))
                }
                Some(Value::Array(items)) => items,
                Some(v) => {
                    return Err(bad(
                        format!("home: open: paths must be a list of folder paths, got {v}"),
                        "paths",
                    ))
                }
            };
            let mut out = Vec::with_capacity(paths.len());
            for p in paths {
                let Some(p) = p.as_str() else {
                    return Err(bad(
                        format!("home: open: paths must hold strings, got {p}"),
                        "paths",
                    ));
                };
                out.push(p.to_string());
            }
            Intent::Open { paths: out }
        }
        "start" => Intent::Start { id: id()? },
        "stop" => Intent::Stop { id: id()? },
        "remove" => Intent::Forget { id: id()? },
        "select" => match payload.get("id") {
            None => return Err(missing("home: select", "id")),
            Some(Value::Null) => Intent::Select { id: None },
            Some(Value::String(id)) => Intent::Select {
                id: Some(id.clone()),
            },
            Some(v) => {
                return Err(bad(
                    format!("home: select: id must be a string or null, got {v}"),
                    "id",
                ))
            }
        },
        "select_page" => {
            let index = match payload.get("index") {
                None | Some(Value::Null) => return Err(missing("home: select_page", "index")),
                Some(v) => v.as_u64().ok_or_else(|| {
                    bad(
                        format!("home: select_page: index must be an integer ≥ 0, got {v}"),
                        "index",
                    )
                })?,
            };
            match usize::try_from(index) {
                Ok(index) => Intent::SelectPage { index },
                Err(_) => return Err(bad(format!("home: select_page: no page {index}"), "index")),
            }
        }
        "close_page" => Intent::ClosePage { id: id()? },
        other => unreachable!("`{other}` is not an intent verb"),
    })
}

/// `intent` checked against the home now; the intent to reduce (an `open`'s
/// paths resolved to the folders they name).
fn checked(
    runner: &Arc<Runner>,
    rows: &[Row],
    mode: Mode,
    intent: Intent,
) -> Result<Intent, Value> {
    match intent {
        Intent::Open { paths } => {
            let mut folders = Vec::with_capacity(paths.len());
            for p in &paths {
                folders.push(folder_of(runner, rows, "open", "paths", p)?);
            }
            Ok(Intent::Open { paths: folders })
        }
        Intent::Start { ref id } | Intent::Stop { ref id } | Intent::Forget { ref id } => {
            let verb = intent.verb();
            let row = row_of(rows, id)?;
            if !row.actions(mode).contains(&verb) {
                return Err(bad(
                    format!(
                        "home: {} cannot {verb} now — it is {}",
                        row.name,
                        status_name(&row.status)
                    ),
                    "id",
                ));
            }
            Ok(intent)
        }
        Intent::Select { id: Some(ref id) } => {
            row_of(rows, id)?;
            Ok(intent)
        }
        Intent::Select { id: None } => Ok(intent),
        Intent::SelectPage { index } => {
            let count = runner.read(|h| h.pages().len());
            if index < count {
                Ok(intent)
            } else {
                Err(bad(
                    format!("home: select_page: no page {index} (there are {count})"),
                    "index",
                ))
            }
        }
        Intent::ClosePage { ref id } => {
            if !runner.read(|h| h.pages().iter().any(|p| p == id)) {
                return Err(not_found(format!("home: no page \"{id}\" is open"), "id"));
            }
            Ok(intent)
        }
    }
}

/// THE COMPATIBILITY RULE of protocol v1 (`docs/home.md`, "Versioning").
pub const COMPAT: &str = "v1 never removes or renames a verb, an argument, a state field, an emitted payload type, a status or an action, and never changes a field's type or meaning. v1 may add an optional argument, a state field, a status-independent field, an emitted payload type or a verb; a v1 client ignores what it does not know and keeps only the payload types it reads. Anything else is v2.";

/// The docs of every verb, plus the lifecycle: the signature first (the
/// `oneliners` tier), then the reply, then each refusal.
fn verb_docs_json() -> Value {
    let docs = json!({
        "list": "args: version:int?. Returns the state (see emits.state). Refuses: bad_arg (version).",
        "open": "args: paths:[str] (req, ≥ 1), version:int?. Opens each folder's page, the last one shown: a listed row's path as it is, else an absolute folder; a stopped one is started, an unlisted one is started and added to the list. Returns the state. Refuses: missing_arg (paths absent or empty), bad_arg (paths not a list of strings, a path not absolute, not a folder, or the home's own folder), bad_arg (version).",
        "register": "args: path:str (req), version:int?. Adds an absolute folder the list does not hold, as `open` does. Returns the state. Refuses: missing_arg (path), bad_arg (path not a string, not absolute, not a folder, the home's own folder, or listed already), bad_arg (version).",
        "start": "args: id:str (req), version:int?. Starts the stopped fantastic `id` (`fantastic canvas` in its folder). Returns the state. Refuses: missing_arg (id), not_found (no such id), bad_arg (its actions lack start — it is <status>), bad_arg (version).",
        "stop": "args: id:str (req), version:int?. Asks the process holding the folder of `id` to stop. Returns the state. Refuses: missing_arg (id), not_found (no such id), bad_arg (its actions lack stop — it is <status>), bad_arg (version).",
        "remove": "args: id:str (req), version:int?. Removes the registry row `id` from the list, through its running fantastic's kernel (or a running one's, when it is stopped). Returns the state. Refuses: missing_arg (id), not_found (no such id), bad_arg (its actions lack remove — a folder being added), bad_arg (version).",
        "select": "args: id:str|null (key req), version:int?. Shows the page of row `id` (opened if it is not), or the Home for null. Returns the state. Refuses: missing_arg (id), not_found (no such id), bad_arg (id neither a string nor null), bad_arg (version).",
        "select_page": "args: index:int (req, ≥ 0), version:int?. Shows the open page at `index`. Returns the state. Refuses: missing_arg (index), bad_arg (index not an integer ≥ 0, or no page there), bad_arg (version).",
        "close_page": "args: id:str (req), version:int?. Closes the open page `id`; its fantastic keeps running. Returns the state. Refuses: missing_arg (id), not_found (no such page open), bad_arg (version).",
        "dismiss": "args: version:int?. Clears the problem. Returns the state. Refuses: bad_arg (version).",
        "log": "args: id:str (req), version:int?. Returns {id, file, tail}: the launch log file of row `id` and its last lines (tail null when unreadable). Refuses: missing_arg (id), not_found (no such id, or no launch log), bad_arg (version).",
        "focus": "args: version:int?. Emits {type:\"focus\"} on home for the host shell (raise its window); nothing else changes. Returns the state. Refuses: bad_arg (version).",
        "menu": "args: version:int?. Returns {type:\"menu_spec\", seq, spec}: the native menus a host shell builds (fantastic_home::menu, version 1), seq the state's. The same payload is emitted on home after every state emit that changed it. Refuses: bad_arg (version).",
        "reflect": "args: version:int?. Returns {id, sentence, protocol:\"fantastic.home\", version:1, mode, home, poll_ms}. Refuses: bad_arg (version).",
        "boot": "Kernel lifecycle, never a client's. Restores the kept pages and selection from this agent's record, then starts the poll (every 2 s). Idempotent. Refuses: bad_arg (a malformed `pages` or `selection` on the record).",
        "shutdown": "Kernel lifecycle, never a client's: the kernel sends it at a clean stop. Stops the poll; the pages and selection are already on the record. Returns {stopped:true}.",
    });
    debug_assert!(VERBS.iter().all(|v| docs.get(v).is_some()));
    docs
}

/// What `home` emits, by payload type.
fn emits_json() -> Value {
    json!({
        "state": "{type:\"state\", protocol:\"fantastic.home\", version:1, seq:int, mode:\"lock\"|\"canvas\", listed:bool, rows:[{id:str (a guid, or \"adding:<path>\"), guid:str|null, name:str, path:str, last_seen:int|null (seconds), status:str, canvas:{url, port}|null (set while running), pid:int|null, actions:[str], log:str|null (the launch log file)}], pages:[id], selection:id|null (null: the Home), problem:str|null}. status is one of stopped, starting, stopping, running, no_canvas, unverified, missing; actions hold, in this order, start (stopped) or stop (a process holds it, not stopping), then open, then remove (a registry row). Emitted on every change with seq rising. Watch home (dial /home/ws) first, then call list, and drop any state whose seq ≤ the list's.",
        "focus": "{type:\"focus\"}: emitted by the focus verb for the host shell. A watcher also sees the mirrored focus call, which has the same type.",
        "menu_spec": "{type:\"menu_spec\", seq, spec}: the native menus a host shell builds (fantastic_home::menu, version 1), emitted right after a state emit that changed them, seq that state's. A page keeps only state payloads.",
    })
}

/// The detailed tier's notes: the protocol, where it is served, and the rule.
fn notes_json() -> Value {
    json!({
        "protocol": PROTOCOL,
        "version": VERSION,
        "host": "the desktop app hosts it in-process; while it runs, <home>/app/home.yaml is {pid, port, url}",
        "calls": "ws://127.0.0.1:<port>/user/ws — {type:\"call\", target:\"user\", payload:{type:\"forward\", target:\"home\", payload}, id}",
        "events": "ws://127.0.0.1:<port>/home/ws — {type:\"event\", payload:{type:\"state\"|\"focus\"|\"menu_spec\",…}}; other payloads are the calls others sent, mirrored",
        "origin": "a browser Origin other than the host's own is refused; a client with no Origin header is admitted",
        "compat": COMPAT,
        "doc": "docs/home.md in fantastic-home-kernel",
    })
}

/// The protocol reference: `readme()`, and the `protocol` docs topic.
const PROTOCOL_DOC: &str = include_str!("../../docs/home.md");

#[async_trait]
impl Bundle for HomeBundle {
    fn name(&self) -> &str {
        "home"
    }

    fn verb_docs(&self) -> Value {
        verb_docs_json()
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(emits_json())
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(notes_json())
    }

    fn readme(&self) -> Option<&'static str> {
        Some(PROTOCOL_DOC)
    }

    fn docs(&self) -> &'static [(&'static str, &'static str)] {
        &[("protocol", PROTOCOL_DOC)]
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(Some(match verb {
            "boot" => self.boot(agent_id, kernel),
            "shutdown" => {
                self.runner.stop_poll();
                json!({"stopped": true})
            }
            "reflect" => self.reflect(agent_id, kernel, payload),
            _ => call(&self.runner, payload).await,
        }))
    }
}

#[cfg(test)]
mod tests;
