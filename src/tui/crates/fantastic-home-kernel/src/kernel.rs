//! THE HOME KERNEL, booted in this process on the caller's runtime.
//!
//! Its folder is `W = <home>/app`, both its workdir and its state dir
//! (`BootstrapOptions::daemon_at(W, W)`: no `.fantastic` inside): everything it
//! writes is under it, in the kernel's standard format —
//! `W/{lock.yaml, agent.yaml, agents/…, home.db, web/…}` and the run file
//! `W/home.yaml` `{pid, port, url}` while it serves.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fantastic_home::{Config as HomeConfig, Mode};
use fantastic_kernel::bootstrap::{self, BootstrapOptions};
use fantastic_kernel::{lock, AgentId, BundleRegistry, Item, Kernel, Status};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::home::{self, sys, HomeBundle, SysOs};
use crate::tree;
use crate::Config;

/// The ledger cap: calls kept before the kernel prunes its store (the CLI's, `kern/host.rs`).
pub const LEDGER_CAP: usize = 100_000;

/// The operator watcher every [`HomeKernel::events`] receiver is fed from.
pub const HOME_EVENTS: &str = "home_events";

/// How many payloads one events receiver holds.
const EVENTS_BOUND: usize = 256;

/// The base records a new home needs before its root boots.
const SEED: &str = include_str!("../seed.yaml");

/// The home kernel's folder under `home`: its workdir and its state dir.
pub fn workdir(home: &Path) -> PathBuf {
    home.join("app")
}

/// The URL of the home page on `port`.
pub fn url(port: u16) -> String {
    format!(
        "http://127.0.0.1:{port}/{}/file/home.html",
        tree::CANVAS_PAGE
    )
}

/// The receivers of every payload on `home`, and the task that feeds them.
#[derive(Default)]
struct Events {
    receivers: Mutex<Vec<mpsc::Sender<Value>>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Events {
    fn senders(&self) -> std::sync::MutexGuard<'_, Vec<mpsc::Sender<Value>>> {
        self.receivers.lock().expect("the events' lock is poisoned")
    }

    /// Stop the feed: every receiver then reads `None`.
    fn close(&self) {
        if let Some(task) = self
            .task
            .lock()
            .expect("the events' lock is poisoned")
            .take()
        {
            task.abort();
        }
        self.senders().clear();
    }
}

/// The home kernel: the kernel, its home, its port and URL, its events.
pub struct HomeKernel {
    kernel: Arc<Kernel>,
    home: PathBuf,
    workdir: PathBuf,
    port: u16,
    url: String,
    events: Arc<Events>,
}

impl std::fmt::Debug for HomeKernel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HomeKernel")
            .field("home", &self.home)
            .field("url", &self.url)
            .finish()
    }
}

/// The host's config, checked.
fn check(cfg: &Config) -> Result<(), String> {
    if !cfg.home.is_absolute() {
        return Err(format!(
            "the home dir must be an absolute path, got {}",
            cfg.home.display()
        ));
    }
    if !cfg.fantastic_bin.is_absolute() {
        return Err(format!(
            "fantastic_bin must be an absolute path, got {}",
            cfg.fantastic_bin.display()
        ));
    }
    if !sys::is_file(&cfg.fantastic_bin) {
        return Err(format!(
            "fantastic_bin {} is not a file",
            cfg.fantastic_bin.display()
        ));
    }
    Ok(())
}

/// Exactly the bundles a home runs; a stray record naming any other weak-loads
/// and is skipped.
pub(crate) fn registry(cfg: &Config, own: String) -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    reg.register(
        fantastic_core::HANDLER_MODULE,
        fantastic_core::KernelStateBundle::default(),
    );
    reg.register(
        fantastic_file::HANDLER_MODULE,
        fantastic_file::FileBundle::default(),
    );
    reg.register(
        fantastic_sqlite_store::HANDLER_MODULE,
        fantastic_sqlite_store::SqliteStoreBundle,
    );
    reg.register(
        fantastic_user::HANDLER_MODULE,
        fantastic_user::UserBundle::default(),
    );
    reg.register(fantastic_web::HANDLER_MODULE, fantastic_web::WebBundle);
    reg.register(
        fantastic_web_ws::HANDLER_MODULE,
        fantastic_web_ws::WebWsBundle,
    );
    reg.register(
        fantastic_canvas_page::HANDLER_MODULE,
        fantastic_canvas_page::CanvasPageBundle,
    );
    reg.register(
        home::HANDLER_MODULE,
        HomeBundle::new(
            Arc::new(SysOs::new(cfg.home.clone(), cfg.fantastic_bin.clone())),
            HomeConfig::new(Mode::Lock).own_folder(own),
        ),
    );
    reg
}

/// A new home's base records (`seed.yaml`), written once: while `W` (the state
/// dir) has no root record.
fn seed(w: &Path) -> Result<(), String> {
    if sys::exists(&w.join("agent.yaml")) {
        return Ok(());
    }
    let doc: serde_yaml::Value =
        serde_yaml::from_str(SEED).map_err(|e| format!("seed.yaml: {e}"))?;
    let agents = doc
        .get("agents")
        .and_then(serde_yaml::Value::as_sequence)
        .ok_or("seed.yaml: `agents` must be a list")?;
    for record in agents {
        let id = record
            .get("id")
            .and_then(serde_yaml::Value::as_str)
            .ok_or("seed.yaml: a record has no `id`")?;
        let text = serde_yaml::to_string(record).map_err(|e| format!("seed.yaml: {id}: {e}"))?;
        sys::write_atomic(
            &w.join("agents").join(id).join("agent.yaml"),
            text.as_bytes(),
        )?;
    }
    Ok(())
}

fn refused(result: &Value) -> Option<String> {
    result
        .get("error")
        .and_then(Value::as_str)
        .map(|e| sys::with_hint(e, result))
}

impl HomeKernel {
    /// Boot the home kernel of `cfg.home` (see the crate docs). A failure after
    /// the bootstrap shuts down what started and releases the lock first.
    pub async fn boot(cfg: Config) -> Result<HomeKernel, String> {
        check(&cfg)?;
        let w = workdir(&cfg.home);
        fantastic_user_home_store::ensure_private_dir(&cfg.home, &w)?;
        // The home's own folder, never listed: `<home>` itself, the parent of `W`.
        let own = sys::folder(
            cfg.home
                .to_str()
                .ok_or_else(|| format!("{} is not UTF-8", cfg.home.display()))?,
        )?;
        match lock::held_at(&w) {
            Ok(Some(pid)) => {
                return Err(format!(
                    "{}: already running (pid {pid})",
                    cfg.home.display()
                ))
            }
            Ok(None) => {}
            Err(e) => return Err(e.to_string()),
        }
        seed(&w)?;
        let booted = bootstrap::bootstrap(registry(&cfg, own), BootstrapOptions::daemon_at(&w, &w))
            .map_err(|e| e.to_string())?;
        let kernel = booted.kernel;
        match Self::start(&kernel, &cfg, &w).await {
            Ok(hk) => Ok(hk),
            Err(e) => {
                kernel.shutdown().await;
                for line in fantastic_sqlite_store::close_all(&kernel) {
                    tracing::warn!("home: a failed boot: {line}");
                }
                if let Err(le) = bootstrap::shutdown_at(&w) {
                    tracing::warn!("home: a failed boot: {le}");
                }
                Err(e)
            }
        }
    }

    /// Steps 7–15 of the boot: everything after the bootstrap.
    async fn start(kernel: &Arc<Kernel>, cfg: &Config, w: &Path) -> Result<HomeKernel, String> {
        kernel.set_ledger_cap(Some(LEDGER_CAP));
        let root = kernel.root().ok_or("the kernel booted with no root")?;
        let names_store = root.record().meta.contains_key("memory_store_id");
        if !names_store {
            tree::operator(
                kernel,
                "kernel",
                json!({"type": "update_agent", "id": root.id.as_str(), "memory_store_id": tree::HOME_MEMORY}),
            )
            .await
            .map_err(|e| format!("could not name the root's memory store: {e}"))?;
        }
        let started = kernel.start(false).await;
        if started.get("started") != Some(&json!(true)) {
            return Err(refused(&started).unwrap_or_else(|| format!("kernel_start: {started}")));
        }
        let done: Vec<String> = started
            .get("booted")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // THE PORT: a new one is set on the recorded host before it listens.
        if let (Some(p), Some(had)) = (cfg.port, tree::recorded_port(kernel)) {
            if p != had {
                tree::operator(
                    kernel,
                    "kernel",
                    json!({"type": "update_agent", "id": tree::WEB, "port": p}),
                )
                .await
                .map_err(|e| format!("port {p}: {e}"))?;
            }
        }
        // Every hydrated agent the start did not boot, in registration order.
        // Every agent here is the home's: a refusal is fatal.
        for id in kernel.agent_ids() {
            if done.contains(&id.0) {
                continue;
            }
            let out = Arc::clone(kernel)
                .send(vec![Item::new(id.clone(), json!({"type": "boot"}))])
                .await
                .remove(0);
            if let Some(e) = refused(&out.result) {
                return Err(e);
            }
            if !matches!(out.status, Status::Completed) {
                return Err(format!(
                    "`{id}` did not boot ({}): {}",
                    out.status.as_str(),
                    out.result
                ));
            }
        }
        let port = tree::ensure(kernel, cfg.port).await?;
        let events = Arc::new(Events::default());
        Self::feed(kernel, &events).await?;
        let url = url(port);
        let run =
            fantastic_kernel::yaml_io::dump(&json!({"pid": sys::pid(), "port": port, "url": url}));
        if let Err(e) = sys::write_atomic(&w.join("home.yaml"), run.as_bytes()) {
            events.close();
            kernel.remove_inbox(&AgentId::from(HOME_EVENTS));
            return Err(e);
        }
        Ok(HomeKernel {
            kernel: Arc::clone(kernel),
            home: cfg.home.clone(),
            workdir: w.to_path_buf(),
            port,
            url,
            events,
        })
    }

    /// The one operator watcher of `home`, and the task that hands each payload
    /// to every live receiver in turn (a dropped one is removed at its next send).
    async fn feed(kernel: &Arc<Kernel>, events: &Arc<Events>) -> Result<(), String> {
        let watcher = AgentId::from(HOME_EVENTS);
        let mut inbox = kernel.ensure_inbox(&watcher);
        if let Some(r) = Arc::clone(kernel)
            .watch(&AgentId::from(tree::HOME), watcher.clone())
            .await
        {
            kernel.remove_inbox(&watcher);
            return Err(refused(&r).unwrap_or_else(|| r.to_string()));
        }
        let fed = Arc::clone(events);
        let task = tokio::spawn(async move {
            while let Some(item) = inbox.recv().await {
                let receivers: Vec<mpsc::Sender<Value>> = fed.senders().clone();
                let mut gone = Vec::new();
                for tx in receivers {
                    if tx.send(item.payload.clone()).await.is_err() {
                        gone.push(tx);
                    }
                }
                if !gone.is_empty() {
                    fed.senders()
                        .retain(|tx| !gone.iter().any(|g| g.same_channel(tx)));
                }
            }
        });
        *events.task.lock().expect("the events' lock is poisoned") = Some(task);
        Ok(())
    }

    /// `http://127.0.0.1:<port>/canvas_page/file/home.html`.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Send `payload` to `home` the way a page does: `user.forward`, so the
    /// grants apply. A refusal is `Err("<error> — <hint>")`.
    pub async fn call(&self, payload: Value) -> Result<Value, String> {
        let forward = json!({"type": "forward", "target": tree::HOME, "payload": payload});
        let out = Arc::clone(&self.kernel)
            .send(vec![Item::new(AgentId::from(tree::USER), forward)])
            .await
            .remove(0);
        match refused(&out.result) {
            Some(e) => Err(e),
            None => Ok(out.result),
        }
    }

    /// Every payload sent or emitted on `home` from now on: the `state` emits,
    /// the `focus` emits and the calls other clients make, mirrored.
    pub fn events(&self) -> mpsc::Receiver<Value> {
        let (tx, rx) = mpsc::channel(EVENTS_BOUND);
        self.events.senders().push(tx);
        rx
    }

    /// Stop in order: the kernel (its drain, `home`'s poll, the records and the
    /// last execution rows, the teardown walk), the store's WAL, the run file,
    /// then the lock. The fantastics it started keep running.
    pub async fn shutdown(self) -> Result<(), String> {
        self.kernel.shutdown().await;
        self.events.close();
        self.kernel.remove_inbox(&AgentId::from(HOME_EVENTS));
        let mut errors: Vec<String> = fantastic_sqlite_store::close_all(&self.kernel);
        if let Err(e) = sys::remove_file(&self.workdir.join("home.yaml")) {
            errors.push(e);
        }
        if let Err(e) = bootstrap::shutdown_at(&self.workdir) {
            errors.push(e.to_string());
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}
