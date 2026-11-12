//! THE `home` AGENT (`home.tools`): the manager's verbs on the kernel protocol.
//!
//! Every check runs before any event is reduced, so a refused call changes
//! nothing. Every intent replies with the state right after its event; the
//! effects run afterwards, and what they change arrives as `state` emits.
//!
//! The verbs are declared in [`super::verbs`] (`reflect` and `docs` render them); the
//! agent's entry has checked a call before [`HomeBundle::handle`] sees it. The verb
//! logic is [`call`], which needs no kernel: tests and the contract replay run it
//! directly on a typed call. [`HomeBundle::handle`] adds the lifecycle (`boot`,
//! `shutdown`) and `reflect`.

use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use fantastic_home::{Config as HomeConfig, Event, Intent, Mode, Opened, Row};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::refusal::{refusal, with, BAD_ARG, NOT_FOUND};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

use super::os::HomeOs;
use super::runner::{Runner, POLL};
use super::verbs::{self, Call};
use super::wire::{mode_name, status_name, PROTOCOL};
use super::{record, ID};

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
        let Some(meta) = kernel.agents.get(agent_id).map(|a| a.value().record().meta.clone()) else {
            return not_found(format!("home: boot: no agent {agent_id:?}"), "id");
        };
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
        // The home kernel's state folder is always `<home>/app`.
        let home = kernel
            .state_dir()
            .ok()
            .and_then(|w| w.parent().map(|h| h.display().to_string()));
        reflection(&self.runner, agent_id.as_str(), home)
    }
}

/// `reflect`'s answer for the agent `id` of the home dir `home`.
pub(crate) fn reflection(runner: &Runner, id: &str, home: Option<String>) -> Value {
    let mode = runner.read(|h| h.config().mode);
    json!({
        "id": id,
        "sentence": MAN,
        "protocol": PROTOCOL,
        "mode": mode_name(mode),
        "home": home,
        "poll_ms": POLL.as_millis() as u64,
    })
}

/// The record's `pages` and `selection`, as kept; a record that holds them as
/// anything else is refused.
fn kept_of(meta: &Map<String, Value>) -> Result<(Vec<String>, Option<String>), Value> {
    let kept: record::Record = serde_json::from_value(Value::Object(meta.clone()))
        .map_err(|e| bad(format!("home: its record's `pages` or `selection` is malformed: {e}"), "pages"))?;
    Ok((kept.pages, kept.selection))
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

fn row_of(rows: &[Row], id: &str) -> Result<Row, Value> {
    rows.iter()
        .find(|r| r.id == id)
        .cloned()
        .ok_or_else(|| not_found(format!("home: no fantastic \"{id}\" in the list"), "id"))
}

/// One folder for `open` or `register`: a listed row's path (its state folder)
/// as it is, else an absolute path resolved to the picked folder — its state
/// folder `<folder>/.fantastic`, its work folder itself — never the home's own.
fn folder_of(
    runner: &Runner,
    rows: &[Row],
    verb: &str,
    arg: &str,
    path: &str,
) -> Result<Opened, Value> {
    if rows.iter().any(|r| r.path == path) {
        return Ok(Opened {
            path: path.to_string(),
            workdir: None,
        });
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
    let state = fantastic_kernel::bootstrap::state_dir_for(None, Some(Path::new(&folder)))
        .map_err(|e| bad(format!("home: {verb}: \"{path}\": {e}"), arg))?;
    Ok(Opened {
        path: state.display().to_string(),
        workdir: Some(folder),
    })
}

/// Run one verb of the agent (every verb but the lifecycle and `reflect`): its
/// arguments are the declared ones, already checked by the agent's entry.
pub async fn call(runner: &Arc<Runner>, call: Call) -> Value {
    match intent(runner, call) {
        Ok(Some(event)) => runner.apply(event),
        Ok(None) => runner.state(),
        Err(reply) => reply,
    }
}

/// The checks of one verb, and the event it reduces (`None`: it reduces none).
/// `Err` is the reply: a refusal, or what a reader verb answers.
fn intent(runner: &Arc<Runner>, call: Call) -> Result<Option<Event>, Value> {
    let (rows, mode) = runner.read(|h| (h.rows(runner.os().now_ms()), h.config().mode));
    let checked_event = |intent: Intent| checked(runner, &rows, mode, intent).map(|i| Some(i.event()));
    match call {
        Call::List(_) => Ok(None),
        Call::Open(a) => checked_event(Intent::Open {
            paths: a
                .paths
                .into_iter()
                .map(|path| Opened {
                    path,
                    workdir: None,
                })
                .collect(),
        }),
        Call::Start(a) => checked_event(Intent::Start { id: a.id }),
        Call::Stop(a) => checked_event(Intent::Stop { id: a.id }),
        Call::Remove(a) => checked_event(Intent::Forget { id: a.id }),
        Call::Select(a) => checked_event(Intent::Select { id: a.id }),
        Call::SelectPage(a) => match usize::try_from(a.index) {
            Ok(index) => checked_event(Intent::SelectPage { index }),
            Err(_) => Err(bad(format!("home: select_page: no page {}", a.index), "index")),
        },
        Call::ClosePage(a) => checked_event(Intent::ClosePage { id: a.id }),
        Call::Register(a) => {
            let folder = folder_of(runner, &rows, "register", "path", &a.path)?;
            if let Some(r) = rows.iter().find(|r| r.path == folder.path) {
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
        Call::Dismiss(_) => Ok(Some(Event::Dismiss)),
        Call::Log(a) => {
            let row = row_of(&rows, &a.id)?;
            let os = runner.os();
            let Some(file) = os.log_file(&row.path) else {
                return Err(not_found(
                    format!("home: no launch log for {}", row.name),
                    "id",
                ));
            };
            Err(json!({"id": a.id, "file": file, "tail": os.log_tail(&row.path)}))
        }
        Call::Focus(_) => Err(runner.focus()),
        Call::Menu(_) => Err(runner.menu_spec()),
        Call::Boot(_) | Call::Shutdown(_) | Call::Reflect(_) => {
            unreachable!("the lifecycle and reflect are answered by the bundle")
        }
    }
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
                folders.push(folder_of(runner, rows, "open", "paths", &p.path)?);
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

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "The home: your fantastics, listed with their status. Open a folder, start, stop, remove; the open pages and the selection are kept across restarts. It emits its state on every change.";

#[async_trait]
impl Handler for HomeBundle {
    type Call = Call;

    fn name(&self) -> &str {
        "home"
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

    async fn handle(
        &self,
        agent_id: &AgentId,
        call_: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(match call_ {
            Call::Boot(_) => self.boot(agent_id, kernel),
            Call::Shutdown(_) => {
                self.runner.stop_poll();
                json!({"stopped": true})
            }
            Call::Reflect(_) => self.reflect(agent_id, kernel),
            other => call(&self.runner, other).await,
        }))
    }
}

#[cfg(test)]
mod tests;
