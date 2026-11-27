//! ONE COMMAND, EXECUTED. The bridge between what a key wanted and what the
//! kernel does.
//!
//! Each command runs in its own task holding an `Arc<Host>`, so a long pass and a
//! fast tree read never queue behind each other, and the render loop awaits none
//! of it.
//!
//! Reads never act: tasks and the tree are field accesses; an agent's rows are a
//! `read` on the memory store made as the OPERATOR, its admin (`Host::rows`).
//! There is no cursor to get wrong. Acts go through `user`, so the gate and the
//! ledger both name a person.

use std::sync::Arc;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::ev::{Cmd, Ev, Got};
use crate::kern::cps::{done, Done};
use crate::kern::host::Host;

pub struct Runner {
    host: Arc<Host>,
    tx: mpsc::Sender<Ev>,
    /// One forwarding task per watched agent, so `Unwatch` can stop it.
    watches: std::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
    /// The typed writes waiting per agent ([`Runner::type_in`]).
    typing: std::sync::Mutex<std::collections::HashMap<String, Typist>>,
}

/// One agent's typed writes: the ones waiting, and whether one is out.
#[derive(Default)]
struct Typist {
    queue: std::collections::VecDeque<Cmd>,
    busy: bool,
}

/// Spawn the runner. Returns the sender a key handler pushes into.
pub fn spawn(host: Arc<Host>, tx: mpsc::Sender<Ev>) -> mpsc::Sender<Cmd> {
    // Deep enough that a burst of keys never blocks the render loop; bounded so a
    // wedged handler applies backpressure instead of growing without limit.
    let (ctx, mut crx) = mpsc::channel::<Cmd>(256);
    let r = Arc::new(Runner { host, tx, watches: Default::default(), typing: Default::default() });
    tokio::spawn(async move {
        while let Some(cmd) = crx.recv().await {
            if matches!(cmd, Cmd::Quit) {
                break;
            }
            if matches!(cmd, Cmd::Typed { .. }) {
                Runner::type_in(&r, cmd);
                continue;
            }
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.one(cmd).await });
        }
    });
    ctx
}

impl Runner {
    /// KEYSTROKES ARE ORDERED. Every other command runs in its own task, but the
    /// `write`s to one agent go one at a time, or typed text arrives shuffled
    /// (measured: `echo` → `eh ci7h`). The kernel orders no calls (#795), so each
    /// write is sent when the previous one SETTLED: chained on its settle, nothing
    /// waits for it.
    fn type_in(r: &Arc<Self>, cmd: Cmd) {
        let Cmd::Typed { target, .. } = &cmd else { return };
        let first = {
            let mut typing = r.typing.lock().expect("typing poisoned");
            let t = typing.entry(target.to_string()).or_default();
            if t.busy {
                t.queue.push_back(cmd);
                None
            } else {
                t.busy = true;
                Some(cmd)
            }
        };
        if let Some(cmd) = first {
            let r = Arc::clone(r);
            tokio::spawn(async move { Runner::write(r, cmd).await });
        }
    }

    /// One typed write; its settle (or its launch's refusal) sends the agent's next one.
    async fn write(r: Arc<Self>, cmd: Cmd) {
        let Cmd::Typed { target, job_id, data } = &cmd else { return };
        let payload = json!({"type":"write","job_id":job_id,"data":data});
        let (host, tx, what, to) = (Arc::clone(&r.host), r.tx.clone(), cmd.what(), target.clone());
        let next = Arc::clone(&r);
        r.host
            .operator_then(target, payload, move |out| async move {
                let key = to.to_string();
                settled(host, tx, what, to, out).await;
                Runner::typed(&next, &key);
            })
            .await;
    }

    /// A typed write to `target` settled: its next one goes out, or the agent is idle.
    fn typed(r: &Arc<Self>, target: &str) {
        let next = {
            let mut typing = r.typing.lock().expect("typing poisoned");
            let Some(t) = typing.get_mut(target) else { return };
            let next = t.queue.pop_front();
            if next.is_none() {
                t.busy = false;
            }
            next
        };
        if let Some(cmd) = next {
            let r = Arc::clone(r);
            tokio::spawn(async move { Runner::write(r, cmd).await });
        }
    }

    async fn got(&self, g: Got) {
        // A closed channel means the shell is already exiting; there is nobody
        // left to tell.
        let _ = self.tx.send(Ev::Got(g)).await;
    }

    async fn note(&self, s: String) {
        let _ = self.tx.send(Ev::Note(s)).await;
    }

    async fn one(&self, cmd: Cmd) {
        match &cmd {
            Cmd::Quit | Cmd::Jump { .. } => {}

            Cmd::Watch(agent) => self.watch(agent).await,
            Cmd::Unwatch(agent) => self.unwatch(agent).await,

            Cmd::Stream { agent, payload } => {
                let (tx, what) = (self.tx.clone(), cmd.what());
                self.host
                    .operator_then(agent, payload.clone(), move |out| async move {
                        if let Err(err) = Host::read(out) {
                            let _ = tx.send(Ev::Got(Got::Refused { what, err })).await;
                        }
                    })
                    .await;
            }

            Cmd::OpenTerminal { cols, rows } => {
                let (host, tx) = (Arc::clone(&self.host), self.tx.clone());
                crate::kern::terminal::open(
                    &self.host.clone(),
                    *cols,
                    *rows,
                    done(move |r: Result<crate::kern::terminal::Attach, String>| async move {
                        match r {
                            Ok(a) => {
                                // The tree first: it is what gives the pane its panel.
                                if let Some(t) = host.tree() {
                                    let _ = tx.send(Ev::Got(Got::Tree(t))).await;
                                }
                                let _ = tx.send(Ev::Got(Got::TermAttached { job_id: a.job_id, replay: a.replay })).await;
                            }
                            Err(why) => {
                                let _ = tx.send(Ev::Note(why)).await;
                            }
                        }
                    }),
                )
                .await;
            }
            Cmd::Backends => self.got(Got::Backends(crate::kern::setup::backends(&self.host))).await,
            Cmd::KeyMode => {
                let r = crate::kern::keys::mode().map(|(m, w)| (m.map(|m| m.name().to_string()), w));
                self.got(Got::KeyMode(r)).await
            }
            Cmd::Keys => self.got(Got::Keys(crate::kern::keys::stored())).await,
            Cmd::Saved => send_shelf(Arc::clone(&self.host), self.tx.clone(), false).await,
            Cmd::Prefer { name } => {
                let (host, tx) = (Arc::clone(&self.host), self.tx.clone());
                crate::kern::saved::prefer(
                    &self.host.clone(),
                    name.as_deref(),
                    done(move |r: Result<Option<String>, String>| async move {
                        let r = r.map(|p| match p {
                            Some(n) => format!("`{n}` is your default for new AI agents"),
                            None => "no default AI set-up — new AI agents start from nothing".to_string(),
                        });
                        let _ = tx.send(Ev::Got(Got::Setup(r))).await;
                        send_shelf(host, tx, false).await;
                    }),
                )
                .await;
            }
            Cmd::WizardCreate { name, module, texts, key } => {
                let (host, tx) = (Arc::clone(&self.host), self.tx.clone());
                crate::kern::wizard::create(
                    &self.host.clone(),
                    name,
                    module,
                    texts,
                    key.as_ref(),
                    done(move |r: Result<String, String>| async move { made(host, tx, r).await }),
                )
                .await;
            }
            Cmd::WizardFromSaved { name } => {
                let (host, tx) = (Arc::clone(&self.host), self.tx.clone());
                crate::kern::wizard::from_saved(
                    &self.host.clone(),
                    name,
                    done(move |r: Result<String, String>| async move { made(host, tx, r).await }),
                )
                .await;
            }
            Cmd::WizardSaveEdit { name, module, texts, key } => {
                let (host, tx, name2) = (Arc::clone(&self.host), self.tx.clone(), name.clone());
                crate::kern::wizard::save_edit(
                    &self.host.clone(),
                    name,
                    module,
                    texts,
                    key.as_ref(),
                    done(move |r: Result<(), String>| async move {
                        let r = r.map(|()| format!("saved `{name2}` — agents made from it from now on use these values"));
                        let _ = tx.send(Ev::Got(Got::Setup(r))).await;
                        send_shelf(host, tx, true).await;
                    }),
                )
                .await;
            }
            Cmd::CloseTerminal => {
                self.unwatch(&crate::kern::terminal::TERM_ID.into()).await;
                let (host, tx) = (Arc::clone(&self.host), self.tx.clone());
                crate::kern::terminal::close(
                    &self.host.clone(),
                    done(move |r: Result<(), String>| async move {
                        match r {
                            Ok(()) => {
                                if let Some(t) = host.tree() {
                                    let _ = tx.send(Ev::Got(Got::Tree(t))).await;
                                }
                            }
                            Err(why) => {
                                let _ = tx.send(Ev::Note(why)).await;
                            }
                        }
                    }),
                )
                .await;
            }

            // ── reads: no verb, no gate, no cursor ────────────────────────
            Cmd::RootReflect => {
                self.got(Got::RootReflect {
                    id: self.host.root.clone(),
                    runtime: "rust".into(),
                    // `None` here means NO RECORD PROVIDER IS WIRED: the records
                    // stay in RAM, and the statusline says so rather than
                    // implying the tree survives a restart.
                    provider: if self.host.volatile() {
                        None
                    } else {
                        Some("store".into())
                    },
                    guid: None,
                })
                .await
            }

            Cmd::Tree => match self.host.tree() {
                Some(t) => self.got(Got::Tree(t)).await,
                None => self.note("the kernel has no root".into()).await,
            },

            Cmd::Agents => {
                self.got(Got::Agents {
                    records: self.host.records(),
                })
                .await
            }

            Cmd::Ps => self.got(Got::Ps(self.host.tasks())).await,

            Cmd::Rows { agent, full, after } => {
                // `full` re-reads everything and rebuilds the order; otherwise
                // only what came after the shell's newest row, merged in.
                let (tx, who, full) = (self.tx.clone(), agent.clone(), *full);
                self.host
                    .rows(
                        agent,
                        if full { None } else { *after },
                        done(move |(rows, loaded): (Vec<crate::kern::model::Row>, Option<bool>)| async move {
                            let _ = tx.send(Ev::Got(Got::Rows { agent: who, rows, full, loaded })).await;
                        }),
                    )
                    .await;
            }

            // READING A SELF-DESCRIPTION IS RENDERING: the shell is the operator,
            // like every other read this pane does. HOW TO CALL it is in the same
            // reflect (`tier`: its verbs' docs), which the pane lists beside the
            // self-description.
            Cmd::Reflect { agent } => {
                let (tx, what, who) = (self.tx.clone(), cmd.what(), agent.clone());
                self.host
                    .operator_then(agent, json!({"type":"reflect","tree":"none","tier":"detailed"}), move |out| async move {
                        let _ = tx
                            .send(Ev::Got(match Host::read(out) {
                                Ok(reply) => Got::Reflect { agent: who, reply },
                                Err(err) => Got::Refused { what, err },
                            }))
                            .await;
                    })
                    .await;
            }

            Cmd::OpenView {
                bridge,
                from,
                kind,
                path,
            } => {
                let payload = json!({"type":"open_view","kind":kind,"path":path});
                let (host, tx, what, from) = (Arc::clone(&self.host), self.tx.clone(), cmd.what(), from.clone());
                self.host
                    .operator_then(bridge, payload, move |out| async move {
                        match Host::read(out) {
                            Ok(reply) => {
                                // a view made just now is in the tree only once it is read again
                                if reply.get("error").is_none() {
                                    if let Some(t) = host.tree() {
                                        let _ = tx.send(Ev::Got(Got::Tree(t))).await;
                                    }
                                }
                                let _ = tx.send(Ev::Got(Got::Opened { from, reply })).await;
                            }
                            Err(err) => {
                                let _ = tx.send(Ev::Got(Got::Refused { what, err })).await;
                            }
                        }
                    })
                    .await;
            }

            Cmd::Send { target, payload } => self.act_send(&cmd, target, payload.clone()).await,
            // typed writes are chained per agent ([`Runner::type_in`]), never run here
            Cmd::Typed { .. } => {}

            Cmd::Cancel { task_id } => {
                // Through the front door, so `on_cancel` hooks run and the subtree
                // is killed post-order.
                let (host, tx, what) = (Arc::clone(&self.host), self.tx.clone(), cmd.what());
                self.host
                    .operator_then(&self.host.root.clone(), json!({"type":"cancel","task_id":task_id}), move |out| async move {
                        match Host::read(out) {
                            Ok(v) => {
                                let killed = serde_json::from_value(v.get("killed").cloned().unwrap_or(json!([]))).unwrap_or_default();
                                let _ = tx.send(Ev::Got(Got::Cancelled { killed })).await;
                                let _ = tx.send(Ev::Got(Got::Ps(host.tasks()))).await;
                            }
                            Err(err) => {
                                let _ = tx.send(Ev::Got(Got::Refused { what, err })).await;
                            }
                        }
                    })
                    .await;
            }
        }
    }

    /// The synthesized reader for `agent`'s mirror. Named for the human, so a
    /// trace shows whose screen was reading.
    fn watcher_of(&self, agent: &str) -> fantastic_kernel::AgentId {
        fantastic_kernel::AgentId::from(format!("{}#watch:{agent}", self.host.cfg.user).as_str())
    }

    async fn watch(&self, agent: &crate::kern::model::AgentId) {
        if self.watches.lock().expect("watches").contains_key(agent) {
            return;
        }
        let k = Arc::clone(&self.host.kernel);
        let w = self.watcher_of(agent);
        let mut rx = k.ensure_inbox(&w);
        let src = fantastic_kernel::AgentId::from(agent.as_str());
        // the shell is the in-process operator
        if let Some(refusal) = fantastic_kernel::send::as_operator(k.watch(&src, w.clone())).await {
            k.remove_inbox(&w);
            self.note(format!("{agent}: watch refused: {refusal}")).await;
            return;
        }
        let tx = self.tx.clone();
        let who = agent.clone();
        let task = tokio::spawn(async move {
            while let Some(item) = rx.recv().await {
                let ev = Ev::Link(crate::ev::LinkEv::AgentEvent { agent: who.clone(), payload: item.payload });
                if tx.send(ev).await.is_err() {
                    break;
                }
            }
        });
        self.watches.lock().expect("watches").insert(agent.clone(), task);
    }

    async fn unwatch(&self, agent: &crate::kern::model::AgentId) {
        let Some(task) = self.watches.lock().expect("watches").remove(agent) else {
            return;
        };
        task.abort();
        let w = self.watcher_of(agent);
        self.host.kernel.unwatch(&fantastic_kernel::AgentId::from(agent.as_str()), &w).await;
        self.host.kernel.remove_inbox(&w);
    }

    /// One act (#795): the call goes out from the shell, the operator, and is ACKED at
    /// once; its settle comes later as a message to the shell ([`settled`]), and the
    /// agent's log is re-read then. A refusal at launch is the answer now.
    async fn act_send(&self, cmd: &Cmd, target: &crate::kern::model::AgentId, payload: Value) {
        let (host, tx, what, to) = (Arc::clone(&self.host), self.tx.clone(), cmd.what(), target.clone());
        self.host
            .operator_then(target, payload, move |out| async move {
                settled(host, tx, what, to, out).await;
            })
            .await;
    }
}

/// AN ACT'S SETTLE, delivered to the shell (#795): the reply to the pane that asked,
/// and — the act moved the kernel — the agent's log re-read rather than guessed from
/// the reply (the log is the truth), and `ps`.
async fn settled(
    host: Arc<Host>,
    tx: mpsc::Sender<Ev>,
    what: crate::ev::What,
    target: crate::kern::model::AgentId,
    out: fantastic_kernel::Outcome,
) {
    let failed = out.status != fantastic_kernel::Status::Completed;
    match Host::read(out) {
        Ok(reply) => {
            let _ = tx.send(Ev::Got(Got::Sent { target: target.clone(), reply })).await;
            if failed {
                let _ = tx.send(Ev::Note(format!("{target}: the call did not complete"))).await;
            }
            // the log is read again, then the tasks: the rows' own calls settle first
            let (h2, tx2) = (Arc::clone(&host), tx.clone());
            host.rows(
                &target.clone(),
                None,
                done(move |(rows, loaded): (Vec<crate::kern::model::Row>, Option<bool>)| async move {
                    let _ = tx2.send(Ev::Got(Got::Rows { agent: target, rows, full: true, loaded })).await;
                    let _ = tx2.send(Ev::Got(Got::Ps(h2.tasks()))).await;
                }),
            )
            .await;
        }
        Err(err) => {
            let _ = tx.send(Ev::Got(Got::Refused { what, err })).await;
        }
    }
}

/// The wizard made an AI agent (or why not): its rows watched first (#795: the watch call
/// settles), then the tree and the shelf fresh, then `Created`.
async fn made(host: Arc<Host>, tx: mpsc::Sender<Ev>, r: Result<String, String>) {
    match r {
        Ok(id) => {
            let (h, tx2, id2) = (Arc::clone(&host), tx.clone(), id.clone());
            watch_rows(&host, &id, done(move |w: Result<(), String>| async move { made_shelf(h, tx2, w.map(|()| id2)).await })).await;
        }
        Err(e) => made_shelf(host, tx, Err(e)).await,
    }
}

/// The tree, then the shelf and the keys, then `Created`: what the app needs to select the new agent.
async fn made_shelf(host: Arc<Host>, tx: mpsc::Sender<Ev>, r: Result<String, String>) {
    if r.is_ok() {
        if let Some(t) = host.tree() {
            let _ = tx.send(Ev::Got(Got::Tree(t))).await;
        }
    }
    let h = Arc::clone(&host);
    crate::kern::saved::shelf(
        &h,
        done(move |s: Result<crate::kern::saved::Shelf, String>| async move {
            let _ = tx.send(Ev::Got(Got::Saved(s))).await;
            let _ = tx.send(Ev::Got(Got::Keys(crate::kern::keys::stored()))).await;
            let _ = tx.send(Ev::Got(Got::Created(r))).await;
        }),
    )
    .await;
}

/// The shelf, read (a call), then told to the shell; the keys with it when `keys`.
async fn send_shelf(host: Arc<Host>, tx: mpsc::Sender<Ev>, keys: bool) {
    crate::kern::saved::shelf(
        &host,
        done(move |s: Result<crate::kern::saved::Shelf, String>| async move {
            let _ = tx.send(Ev::Got(Got::Saved(s))).await;
            if keys {
                let _ = tx.send(Ev::Got(Got::Keys(crate::kern::keys::stored()))).await;
            }
        }),
    )
    .await;
}

/// PUSH, not poll. The kernel's own row tail says exactly when an agent's
/// EXECUTION rows moved: a call moves its target, a settle the settled agent.
///
/// The tail yields IDS, and they do NOT arrive in id order, so this never
/// filters by a high-water mark. An AI agent's OWN rows (its messages, folds,
/// views) never pass through here — they land in its memory store, and the AI
/// agent announces them itself (`spawn_row_watch`).
pub fn spawn_row_tail(host: Arc<Host>, tx: mpsc::Sender<Ev>, after: i64) {
    tokio::spawn(async move {
        let mut tail = host.kernel.rows.subscribe(after);
        // THIS shell's own reads write rows too (a call row, then its settle):
        // they never move anything a view shows, and following them would loop
        let mut own: std::collections::VecDeque<i64> = std::collections::VecDeque::new();
        while let Some(id) = tail.recv().await {
            let Some(row) = host.kernel.rows.get_one(id) else { continue };
            let kind = row.get("kind").and_then(Value::as_str);
            if kind == Some("call")
                && row.get("caller").is_none_or(Value::is_null)
                && row.get("verb").and_then(Value::as_str).is_some_and(|v| ["read", "rows", "reflect"].contains(&v))
            {
                if let Some(pid) = row.get("id").and_then(Value::as_i64) {
                    own.push_back(pid);
                    if own.len() > 512 {
                        own.pop_front();
                    }
                }
                continue;
            }
            if kind == Some("settle") && row.get("parent").and_then(Value::as_i64).is_some_and(|p| own.contains(&p)) {
                continue;
            }
            let key = if kind == Some("settle") { "settled" } else { "agent" };
            let Some(agent) = row.get(key).and_then(Value::as_str) else { continue };
            let ev = Ev::Link(crate::ev::LinkEv::RowsMoved { agent: agent.to_string() });
            if tx.send(ev).await.is_err() {
                return;
            }
        }
    });
}

/// The inbox every AI agent's row events land in: one per shell, named for
/// the human.
pub fn row_watcher(host: &Host) -> fantastic_kernel::AgentId {
    fantastic_kernel::AgentId::from(format!("{}#rows", host.cfg.user).as_str())
}

/// THE ROW EVENT. Every AI agent announces the rows it appends to its memory
/// store — a digest `{id, kind, role, bytes}`, never a body — to the watchers
/// it was given (`watch`, operator-named here). Each one moves that agent's
/// list. Registers the inbox, forwards it, and watches every AI agent in the
/// tree now; one created later is watched by [`watch_rows`].
pub async fn spawn_row_watch(host: Arc<Host>, tx: mpsc::Sender<Ev>) {
    let w = row_watcher(&host);
    let mut rx = host.kernel.ensure_inbox(&w);
    tokio::spawn(async move {
        while let Some(item) = rx.recv().await {
            let Some(agent) = item.payload.get("source").and_then(Value::as_str) else { continue };
            let ev = Ev::Link(crate::ev::LinkEv::RowsMoved { agent: agent.to_string() });
            if tx.send(ev).await.is_err() {
                return;
            }
        }
    });
    for ai in host.ai_agents() {
        watch_rows(
            &host,
            &ai,
            done(|r: Result<(), String>| async move {
                if let Err(why) = r {
                    tracing::warn!("{why}");
                }
            }),
        )
        .await;
    }
}

/// Watch one AI agent's rows (operator-named watcher: the shell's inbox); `done` gets the
/// answer with the watch call's settle.
pub async fn watch_rows(host: &Host, ai: &str, done: Done<Result<(), String>>) {
    let ai = ai.to_string();
    host.operator_then(&ai.clone(), json!({"type": "watch", "watcher": row_watcher(host).0}), move |out| async move {
        done(match out.result.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("{ai}: watch refused: {e}")),
            None => Ok(()),
        })
        .await;
    })
    .await;
}

/// The kernel's state feed. The callback is SYNCHRONOUS and runs inline on the
/// dispatching task under a read lock, so it does the least possible work: one
/// `try_send` into this shell's own channel. A full channel drops the event, which
/// is correct — the row tail and the 60 s reconcile both heal a gap.
pub fn subscribe_state(host: &Arc<Host>, tx: mpsc::Sender<Ev>) {
    // The second argument is a traffic event's raw body. The shell shows no bytes,
    // so it is ignored here.
    let sub: fantastic_kernel::StateSubscriber = Arc::new(move |v: &Value, _body: Option<&fantastic_kernel::Blob>| {
        if let Ok(ev) = serde_json::from_value::<crate::kern::model::StateEvent>(v.clone()) {
            let _ = tx.try_send(Ev::Link(crate::ev::LinkEv::State(ev)));
        }
    });
    host.kernel.add_state_subscriber(sub);
}
