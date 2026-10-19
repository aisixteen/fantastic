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

    fn reflect(&self, agent_id: &AgentId, kernel: &Kernel) -> Value {
        // The home kernel's running dir is always `<home>/app`.
        let home = kernel
            .workdir()
            .ok()
            .and_then(|w| w.parent().map(|h| h.display().to_string()));
        let mode = self.runner.read(|h| h.config().mode);
        json!({
            "id": agent_id.as_str(),
            "sentence": "The home: your fantastics, listed with their status — open a folder, start, stop, remove; the open pages and the selection are kept across restarts. Watch `home` for its `state`.",
            "protocol": PROTOCOL,
            "version": VERSION,
            "mode": mode_name(mode),
            "home": home,
            "poll_ms": POLL.as_millis() as u64,
        })
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

/// Run one verb of the agent (every verb but the lifecycle and `reflect`).
pub async fn call(runner: &Arc<Runner>, payload: &Value) -> Value {
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

/// The docs of every verb, plus the lifecycle.
fn verb_docs_json() -> Value {
    let docs = json!({
        "list": "No args. Returns the state: {type:\"state\", protocol:\"fantastic.home\", version:1, seq, mode, listed, rows:[{id, guid, name, path, last_seen, status, canvas, pid, actions, log}], pages, selection, problem}.",
        "open": "args: paths:[str] (req, ≥1) — absolute folders; a listed row's path as it is. Opens each one's page (the last is selected); a stopped one is started, an unlisted one is started and registered. Returns the state.",
        "register": "args: path:str (req) — an absolute folder the list does not hold. Adds it (as open). Returns the state.",
        "start": "args: id:str (req) — a row whose actions hold `start`. Starts `fantastic canvas` in its folder. Returns the state.",
        "stop": "args: id:str (req) — a row whose actions hold `stop`. Asks the process holding its folder to stop. Returns the state.",
        "remove": "args: id:str (req) — a registry row. Removes it from the list through a running fantastic. Returns the state.",
        "select": "args: id:str|null (key req) — show that row's page (opened if not), or null for the home. Returns the state.",
        "select_page": "args: index:int ≥ 0 (req) — show the open page at index. Returns the state.",
        "close_page": "args: id:str (req) — an open page. Closes it. Returns the state.",
        "dismiss": "No args. Clears the problem. Returns the state.",
        "log": "args: id:str (req). Returns {id, file, tail}: the row's launch log file and its last lines.",
        "focus": "No args. Emits {type:\"focus\"} on home for the host shell (raise its window). Returns the state.",
        "menu": "No args. Returns {type:\"menu_spec\", seq, spec}: the native menus a host shell builds (fantastic_home::menu, version 1), seq the state's. The same payload is emitted on home after every state emit that changed it.",
        "reflect": "No args. Returns {id, sentence, protocol:\"fantastic.home\", version:1, mode, home, poll_ms}.",
        "boot": "Kernel lifecycle. Restores the kept pages and selection from this agent's record, then starts the poll (every 2 s). Idempotent. Refuses a malformed `pages` or `selection`.",
        "shutdown": "Kernel lifecycle — the kernel sends it at a clean stop. Stops the poll; the pages and selection are already on the record. Returns {stopped:true}.",
    });
    debug_assert!(VERBS.iter().all(|v| docs.get(v).is_some()));
    docs
}

#[async_trait]
impl Bundle for HomeBundle {
    fn name(&self) -> &str {
        "home"
    }

    fn verb_docs(&self) -> Value {
        verb_docs_json()
    }

    fn readme(&self) -> Option<&'static str> {
        Some(include_str!("../../docs/home.md"))
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
            "reflect" => self.reflect(agent_id, kernel),
            _ => call(&self.runner, payload).await,
        }))
    }
}

#[cfg(test)]
mod tests;
