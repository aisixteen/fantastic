//! ONE COMMAND, EXECUTED. The bridge between what a key wanted and what the
//! kernel does.
//!
//! Each command runs in its own task holding an `Arc<Host>`, so a long turn and a
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

use crate::env::Paths;
use crate::ev::{Cmd, Ev, Got};
use crate::kern::host::Host;
use crate::kern::model::KernelErr;
use crate::manifest;

pub struct Runner {
    host: Arc<Host>,
    tx: mpsc::Sender<Ev>,
    paths: Paths,
    /// One forwarding task per watched agent, so `Unwatch` can stop it.
    watches: std::sync::Mutex<std::collections::HashMap<String, tokio::task::JoinHandle<()>>>,
}

/// Spawn the runner. Returns the sender a key handler pushes into.
pub fn spawn(host: Arc<Host>, tx: mpsc::Sender<Ev>, paths: Paths) -> mpsc::Sender<Cmd> {
    // Deep enough that a burst of keys never blocks the render loop; bounded so a
    // wedged handler applies backpressure instead of growing without limit.
    let (ctx, mut crx) = mpsc::channel::<Cmd>(256);
    let r = Arc::new(Runner { host, tx, paths, watches: Default::default() });
    tokio::spawn(async move {
        // KEYSTROKES ARE ORDERED. Every other command runs in its own task, but
        // `write`s to one agent go through that agent's own queue, one at a
        // time, or typed text arrives shuffled (measured: `echo` → `eh ci7h`).
        let mut writers: std::collections::HashMap<String, mpsc::UnboundedSender<Cmd>> =
            std::collections::HashMap::new();
        while let Some(cmd) = crx.recv().await {
            if matches!(cmd, Cmd::Quit) {
                break;
            }
            if let Cmd::Send { target, payload } = &cmd {
                if payload.get("type").and_then(Value::as_str) == Some("write") {
                    let w = writers.entry(target.clone()).or_insert_with(|| {
                        let (wtx, mut wrx) = mpsc::unbounded_channel::<Cmd>();
                        let r = Arc::clone(&r);
                        tokio::spawn(async move {
                            while let Some(c) = wrx.recv().await {
                                r.one(c).await;
                            }
                        });
                        wtx
                    });
                    let _ = w.send(cmd);
                    continue;
                }
            }
            let r = Arc::clone(&r);
            tokio::spawn(async move { r.one(cmd).await });
        }
    });
    ctx
}

impl Runner {
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
                if let Err(e) = Host::read(self.host.operator(agent, payload.clone()).await) {
                    self.refused(&cmd, e).await
                }
            }

            Cmd::OpenTerminal { cols, rows } => {
                match crate::kern::terminal::open(&self.host, *cols, *rows).await {
                    Ok(a) => {
                        // The tree first: it is what gives the pane its panel.
                        if let Some(t) = self.host.tree() {
                            self.got(Got::Tree(t)).await;
                        }
                        self.got(Got::TermAttached { job_id: a.job_id, replay: a.replay }).await;
                    }
                    Err(why) => self.note(why).await,
                }
            }
            Cmd::Backends => self.got(Got::Backends(crate::kern::setup::backends(&self.host))).await,
            Cmd::KeyMode => {
                let r = crate::kern::keys::mode().map(|(m, w)| (m.map(|m| m.name().to_string()), w));
                self.got(Got::KeyMode(r)).await
            }
            Cmd::SetKeyMode { mode } => {
                let r = match fantastic_user_home_store::secrets::Mode::parse(mode) {
                    Some(m) => crate::kern::keys::set_mode(m).map(|w| format!("keys are kept {w}")),
                    None => Err(format!("no key mode {mode:?} — plain or secure")),
                };
                self.got(Got::Setup(r)).await;
                let r = crate::kern::keys::mode().map(|(m, w)| (m.map(|m| m.name().to_string()), w));
                self.got(Got::KeyMode(r)).await;
            }
            Cmd::Templates => self.got(Got::Templates(crate::kern::templates::shelf(&self.host).await)).await,
            Cmd::SaveTemplate { name, record } => {
                let r = crate::kern::templates::save(&self.host, name, record.clone()).await.map(|path| {
                    format!(
                        "saved as template `{name}` at {path} — LOAD TEMPLATE brings it back any time; d on it makes it your default for new AI agents"
                    )
                });
                self.got(Got::Setup(r)).await;
                self.got(Got::Templates(crate::kern::templates::shelf(&self.host).await)).await;
            }
            Cmd::PreferTemplate { name } => {
                let r = crate::kern::templates::prefer(&self.host, name.as_deref()).await.map(|p| match p {
                    Some(n) => format!("`{n}` is your default for new AI agents"),
                    None => "no default AI template — new AI agents start from nothing".to_string(),
                });
                self.got(Got::Setup(r)).await;
                self.got(Got::Templates(crate::kern::templates::shelf(&self.host).await)).await;
            }
            Cmd::WriteAi(rec) => {
                let mut r = crate::kern::setup::write(&self.host, rec).await;
                if r.is_ok() {
                    if let Err(why) = watch_rows(&self.host, rec.id.trim()).await {
                        r = Err(why);
                    }
                }
                if r.is_ok() {
                    if let Some(t) = self.host.tree() {
                        self.got(Got::Tree(t)).await;
                    }
                }
                self.got(Got::Setup(r.map(|p| format!("written to  {p}")))).await;
            }
            Cmd::StoreKey { module, secret } => {
                let r = crate::kern::keys::store(module, secret)
                    .map(|at| format!("key stored in {at} — loaded on the next start"));
                self.got(Got::Setup(r)).await;
            }
            Cmd::CloseTerminal => {
                self.unwatch(&crate::kern::terminal::TERM_ID.into()).await;
                match crate::kern::terminal::close(&self.host).await {
                    Ok(()) => {
                        if let Some(t) = self.host.tree() {
                            self.got(Got::Tree(t)).await;
                        }
                    }
                    Err(why) => self.note(why).await,
                }
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
                self.got(Got::Rows {
                    agent: agent.clone(),
                    rows: self.host.rows(agent, if *full { None } else { *after }).await,
                    full: *full,
                })
                .await
            }

            // READING A SELF-DESCRIPTION IS RENDERING, not acting, so it goes out
            // as operator like every other read this pane does.
            //
            // Measured 2026-09-13: sent as `user` it is refused on INGRESS by the
            // root, because `reflect` is not a kernel verb and the root's
            // born-with `callable_by` covers `$children` only — `user` sits a
            // level deeper. Granting the human `callable_by` on an agent just so
            // the shell can draw its verb list would be a grant that exists for
            // the renderer's convenience, which is the wrong reason to widen
            // anybody's reach.
            Cmd::Reflect { agent } => {
                let out = self
                    .host
                    .operator(agent, json!({"type":"reflect","tree":"none","readme":true}))
                    .await;
                match Host::read(out) {
                    Ok(reply) => {
                        self.got(Got::Reflect {
                            agent: agent.clone(),
                            reply,
                        })
                        .await
                    }
                    Err(e) => self.refused(&cmd, e).await,
                }
            }

            Cmd::Send { target, payload } => {
                let out = self.host.act(target, payload.clone()).await;
                let failed = out.status != fantastic_kernel::Status::Completed;
                match Host::read(out) {
                    Ok(reply) => {
                        self.got(Got::Sent {
                            target: target.clone(),
                            reply,
                        })
                        .await;
                        if failed {
                            self.note(format!("{target}: the call did not complete"))
                                .await;
                        }
                        // The act moved the kernel. Re-read rather than guessing
                        // from the reply: the log is the truth.
                        self.got(Got::Rows {
                            agent: target.clone(),
                            rows: self.host.rows(target, None).await,
                            full: true,
                        })
                        .await;
                        self.got(Got::Ps(self.host.tasks())).await;
                    }
                    Err(e) => self.refused(&cmd, e).await,
                }
            }

            Cmd::Cancel { task_id } => {
                // Through the front door, so `on_cancel` hooks run and the subtree
                // is killed post-order.
                let out = self
                    .host
                    .act(
                        &self.host.root.clone(),
                        json!({"type":"cancel","task_id":task_id}),
                    )
                    .await;
                match Host::read(out) {
                    Ok(v) => {
                        let killed =
                            serde_json::from_value(v.get("killed").cloned().unwrap_or(json!([])))
                                .unwrap_or_default();
                        self.got(Got::Cancelled { killed }).await;
                        self.got(Got::Ps(self.host.tasks())).await;
                    }
                    Err(e) => self.refused(&cmd, e).await,
                }
            }

            // Wiring a ui agent is an EDIT TO THE MANIFEST followed by a boot. The
            // file stays the truth; the boot is what makes the kernel agree.
            Cmd::ManifestPut { id, entry } => {
                let path = self.paths.manifest();
                let r = manifest::read(&path).and_then(|mut e| {
                    manifest::put(&mut e, entry.clone())?;
                    manifest::write(&path, &e)
                });
                match r {
                    Ok(()) => self.boot(&cmd).await,
                    Err(why) => {
                        self.note(format!("{id}: could not write the manifest: {why}"))
                            .await
                    }
                }
            }

            Cmd::RosterBoot => self.boot(&cmd).await,
        }
    }

    /// Boot the loader. OPERATOR, not the human: re-applying the manifest is this
    /// shell's own housekeeping, and it must be able to create agents the human's
    /// own grants do not cover.
    async fn boot(&self, cmd: &Cmd) {
        let out = self
            .host
            .operator(crate::boot::seed::LOADER, json!({"type":"boot"}))
            .await;
        match Host::read(out) {
            Ok(v) => {
                let list = |k: &str| -> Vec<String> {
                    serde_json::from_value(v.get(k).cloned().unwrap_or(json!([])))
                        .unwrap_or_default()
                };
                self.got(Got::RosterBooted {
                    created: list("created"),
                    reapplied: list("reapplied"),
                })
                .await;
                // The roster just changed which agents exist.
                if let Some(t) = self.host.tree() {
                    self.got(Got::Tree(t)).await;
                }
                self.got(Got::Agents {
                    records: self.host.records(),
                })
                .await;
            }
            Err(e) => self.refused(cmd, e).await,
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
        if let Some(refusal) = k.watch(&src, w.clone()).await {
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

    /// A refusal is filed under the command that earned it, so the pane that asked
    /// is the pane that shows the answer.
    async fn refused(&self, cmd: &Cmd, err: KernelErr) {
        self.got(Got::Refused {
            what: cmd.what(),
            err,
        })
        .await;
    }
}

/// PUSH, not poll. The kernel's own row tail says exactly when an agent's
/// EXECUTION rows moved: a call moves its target, a settle the settled agent.
///
/// The tail yields IDS, and they do NOT arrive in id order, so this never
/// filters by a high-water mark. An AI agent's OWN rows (its turns, folds,
/// views) never pass through here — they land in its memory store, and the AI
/// agent announces them itself (`spawn_row_watch`).
pub fn spawn_row_tail(host: Arc<Host>, tx: mpsc::Sender<Ev>, after: i64) {
    tokio::spawn(async move {
        let mut tail = host.kernel.rows.subscribe(after);
        while let Some(id) = tail.recv().await {
            let Some(row) = host.kernel.rows.get_one(id) else { continue };
            let key = if row.get("kind").and_then(Value::as_str) == Some("settle") { "settled" } else { "agent" };
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
            if item.payload.get("type").and_then(Value::as_str) != Some("row") {
                continue;
            }
            let Some(agent) = item.payload.get("source").and_then(Value::as_str) else { continue };
            let ev = Ev::Link(crate::ev::LinkEv::RowsMoved { agent: agent.to_string() });
            if tx.send(ev).await.is_err() {
                return;
            }
        }
    });
    for ai in host.ai_agents() {
        if let Err(why) = watch_rows(&host, &ai).await {
            tracing::warn!("{why}");
        }
    }
}

/// Watch one AI agent's rows (operator-named watcher: the shell's inbox).
pub async fn watch_rows(host: &Host, ai: &str) -> Result<(), String> {
    let r = host
        .operator(ai, json!({"type": "watch", "watcher": row_watcher(host).0}))
        .await
        .result;
    match r.get("error").and_then(Value::as_str) {
        Some(e) => Err(format!("{ai}: watch refused: {e}")),
        None => Ok(()),
    }
}

/// The kernel's state feed. The callback is SYNCHRONOUS and runs inline on the
/// dispatching task under a read lock, so it does the least possible work: one
/// `try_send` into this shell's own channel. A full channel drops the event, which
/// is correct — the row tail and the 60 s reconcile both heal a gap.
pub fn subscribe_state(host: &Arc<Host>, tx: mpsc::Sender<Ev>) {
    // The second argument is a traffic event's raw body. The shell shows no bytes,
    // so it is ignored here.
    let sub: fantastic_kernel::StateSubscriber = Arc::new(move |v: &Value, _body: Option<&(String, Vec<u8>)>| {
        if let Ok(ev) = serde_json::from_value::<crate::kern::model::StateEvent>(v.clone()) {
            let _ = tx.try_send(Ev::Link(crate::ev::LinkEv::State(ev)));
        }
    });
    host.kernel.add_state_subscriber(sub);
}
