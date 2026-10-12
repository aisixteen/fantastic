//! py_agent — the BASE a folder bundle with a program extends (owner, 2026-10-08).
//! Python `py_agent.tools`, text for text.
//!
//! A folder bundle `<root>/bundles/<name>/bundle.yaml` with `base: py_agent` names
//! its program (`command`, run with cwd = the folder) and its verbs (`verbs`: verb
//! → doc, the one truth `docs` and `reflect bundles=all` read). Each instance
//! (`create_agent handler_module=<name>`) is a FULL agent: the kernel starts the
//! program when the instance boots and kills it when the instance is deleted or the
//! kernel stops; callers reach it only through verbs — there are no process verbs.
//!
//! THE WIRE is the exec_bridge connector channel — the one relay
//! ([`fantastic_exec_bridge::relay::serve`]) with two more ops: child → kernel
//! `{op:"hello", verbs}` first (must equal `bundle.yaml` `verbs`), kernel → child
//! `{op:"call", rid, payload}`, child → kernel `{op:"result", rid, result}`.
//! Nothing respawns. SECURITY: the program runs with the user's OS rights.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use fantastic_bundle::{py_repr, py_repr_value};
use fantastic_exec_bridge::relay::{serve, Extra};
use fantastic_exec_bridge::spawn::{kill_session, spawn_job, NoPath};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{
    refusal, refusal_hint, with, MISSING_ARG, TRANSPORT_CLOSED, TRANSPORT_ERROR, UNHANDLED,
};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, oneshot, watch, Notify};

/// The base's handler module.
pub const HANDLER_MODULE: &str = "py_agent.tools";
/// The base's name — what a `bundle.yaml` writes as `base`.
pub const NAME: &str = "py_agent";
/// Said in every instance's reflect and in the docs notes.
pub const SECURITY: &str = "a py_agent program runs with the user's OS rights — its own \
                            subprocesses, files and network never pass a gate";
/// Chars of the program's stderr kept for its instance's reflect.
pub const STDERR_TAIL: usize = 2000;
/// Verbs the kernel and this base answer; a bundle cannot declare them.
const RESERVED: [&str; 3] = ["reflect", "boot", "shutdown"];

const NOTES: &str = "A folder bundle `<root>/bundles/<name>/bundle.yaml` with `base: py_agent`: \
                     `sentence`, `command` (argv, cwd = the folder), `verbs` (verb → doc), optional \
                     `emits`, `notes`, `record` (defaults under each instance). ";

// ─── the base itself ────────────────────────────────────────────────

/// The `py_agent` bundle: a base only — an agent is created of a folder bundle
/// that extends it.
pub struct PyAgentBundle;

#[async_trait]
impl Bundle for PyAgentBundle {
    fn name(&self) -> &str {
        NAME
    }

    fn folder_base(&self) -> bool {
        true
    }

    fn folder_bundle(
        &self,
        name: &str,
        folder: &Path,
        spec: &Value,
    ) -> Result<Arc<dyn Bundle>, String> {
        FolderBundle::build(name, folder, spec).map(|b| Arc::new(b) as Arc<dyn Bundle>)
    }

    fn validate_meta(
        &self,
        _meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        Some(
            "py_agent is a base — create an agent of a bundle folder whose bundle.yaml says \
             `base: py_agent` (handler_module = the folder name)"
                .to_string(),
        )
    }

    fn verb_docs(&self) -> Value {
        json!({"reflect": "No args. → {id, sentence}."})
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(json!(format!("{NOTES}{SECURITY}.")))
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let t = payload.get("type").cloned().unwrap_or(Value::Null);
        if t.as_str() == Some("reflect") {
            return Ok(Some(json!({
                "id": agent_id.as_str(),
                "sentence": "The base a folder bundle with a program extends — create an \
                             agent of the bundle, not of py_agent.",
            })));
        }
        let mut r = refusal(
            UNHANDLED,
            format!("py_agent: no verb {}", py_repr_value(&t)),
        );
        with(&mut r, "verb", t);
        Ok(Some(r))
    }
}

// ─── a folder bundle that extends it ────────────────────────────────

/// One folder bundle (`bundles/<name>/`): its program and its verbs.
pub struct FolderBundle {
    name: String,
    folder: PathBuf,
    command: Vec<String>,
    verbs: Map<String, Value>,
    sentence: String,
    emits: Option<Value>,
    notes: String,
    record: Map<String, Value>,
}

impl FolderBundle {
    /// Python `folder_bundle`: the bundle a folder runs as, or the refusal text
    /// naming the field.
    fn build(name: &str, folder: &Path, spec: &Value) -> Result<Self, String> {
        let command: Option<Vec<String>> =
            spec.get("command").and_then(Value::as_array).and_then(|a| {
                a.iter()
                    .map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_string))
                    .collect()
            });
        let Some(command) = command.filter(|c| !c.is_empty()) else {
            return Err(
                "command: required — the program's argv as a list of strings, \
                        e.g. [python3, agent.py] (run with cwd = the bundle folder)"
                    .to_string(),
            );
        };
        let verbs = spec
            .get("verbs")
            .and_then(Value::as_object)
            .filter(|m| !m.is_empty() && m.iter().all(|(k, v)| !k.is_empty() && v.is_string()));
        let Some(verbs) = verbs.cloned() else {
            return Err(
                "verbs: required — a map of verb → its doc (the first sentence is the \
                        signature), e.g. {search: 'args: q:str (req). Returns {hits}.'}"
                    .to_string(),
            );
        };
        let taken: Vec<&str> = verbs
            .keys()
            .map(String::as_str)
            .filter(|v| RESERVED.contains(v))
            .collect();
        if !taken.is_empty() {
            return Err(format!(
                "verbs: {} — answered by the kernel; name the program's verbs otherwise",
                taken.join(", ")
            ));
        }
        let record = match spec.get("record") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => {
                return Err(
                    "record: must be a mapping of defaults for each instance's record".to_string(),
                )
            }
        };
        let notes = match spec
            .get("notes")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(n) => format!("{n}\n\n{SECURITY}."),
            None => format!("{SECURITY}."),
        };
        Ok(Self {
            name: name.to_string(),
            folder: folder.to_path_buf(),
            command,
            verbs,
            sentence: spec
                .get("sentence")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            emits: spec.get("emits").filter(|v| !v.is_null()).cloned(),
            notes,
            record,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Starting,
    Running,
    Stopped,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Starting => "starting",
            Phase::Running => "running",
            Phase::Stopped => "stopped",
        }
    }
}

/// One instance's running program.
struct Run {
    phase: watch::Sender<Phase>,
    error: Mutex<Option<String>>,
    code: Mutex<Option<i32>>,
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    rid: AtomicU64,
    out: mpsc::UnboundedSender<Value>,
    pid: i32,
    stop: Arc<Notify>,
    task_ids: Mutex<HashSet<i64>>,
    /// The last [`STDERR_TAIL`] chars of the program's stderr — said in reflect.
    stderr: Mutex<String>,
}

/// instance id → its running program. One kernel per process, as python's `_RUN`.
fn runs() -> &'static Mutex<HashMap<String, Arc<Run>>> {
    static RUNS: OnceLock<Mutex<HashMap<String, Arc<Run>>>> = OnceLock::new();
    RUNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_of(id: &AgentId) -> Option<Arc<Run>> {
    runs()
        .lock()
        .expect("py_agent runs")
        .get(id.as_str())
        .cloned()
}

impl FolderBundle {
    fn instance_reflect(&self, id: &AgentId) -> Value {
        let run = run_of(id);
        let mut out = json!({
            "id": id.as_str(),
            "sentence": self.sentence,
            "base": NAME,
            "bundle": self.name,
            "folder": self.folder.display().to_string(),
            "program": run.as_ref().map_or("not started", |r| r.phase.borrow().as_str()),
            "security": SECURITY,
        });
        if let Some(r) = &run {
            if let Some(c) = *r.code.lock().expect("code") {
                out["exit_code"] = json!(c);
            }
            if let Some(e) = r.error.lock().expect("error").clone() {
                out["program_error"] = json!(e);
            }
            let tail = r.stderr.lock().expect("stderr").clone();
            if !tail.is_empty() {
                out["stderr_tail"] = json!(tail);
            }
        }
        out
    }

    /// Spawn the instance's program (idempotent while it runs).
    async fn start(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        if let Some(r) = run_of(id) {
            if *r.phase.borrow() != Phase::Stopped {
                return Value::Null;
            }
        }
        let mut record = self.record.clone();
        if let Some(a) = kernel.agents.get(id) {
            for (k, v) in a.meta.read().expect("meta").iter() {
                record.insert(k.clone(), v.clone());
            }
        }
        let env = fantastic_exec_bridge::child_env(&record);
        let cwd = self.folder.to_string_lossy().to_string();
        let spawned = match spawn_job(&self.command, id.as_str(), &env, &cwd) {
            Ok(s) => s,
            Err(e) if e.get_ref().is_some_and(|r| r.is::<NoPath>()) => {
                let mut v = refusal_hint(
                    MISSING_ARG,
                    format!(
                        "{}: {} is a bare command and the program gets no PATH to find it in \
                         — the kernel's own PATH is not the child's",
                        self.name,
                        py_repr(&self.command[0])
                    ),
                    "bundle.yaml record: {inherit_env: [PATH]} (or an absolute command)",
                );
                with(&mut v, "arg", json!("PATH"));
                return v;
            }
            Err(e) => {
                return refusal(
                    TRANSPORT_ERROR,
                    format!(
                        "{}: spawn failed: {}",
                        self.name,
                        fantastic_bundle::io_error_str(&e, Path::new(&self.command[0]))
                    ),
                );
            }
        };
        let fantastic_exec_bridge::spawn::Spawned { mut child, ctrl } = spawned;
        let pid = child.id().map_or(-1, |p| p as i32);
        let (rd, wr) = ctrl.into_split();
        let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Value>();
        let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
        let (phase, _) = watch::channel(Phase::Starting);
        let stop = Arc::new(Notify::new());
        let run = Arc::new(Run {
            phase,
            error: Mutex::new(None),
            code: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            rid: AtomicU64::new(0),
            out: out_tx,
            pid,
            stop: Arc::clone(&stop),
            task_ids: Mutex::new(HashSet::new()),
            stderr: Mutex::new(String::new()),
        });
        runs()
            .lock()
            .expect("py_agent runs")
            .insert(id.as_str().to_string(), Arc::clone(&run));

        let relay_id = TaskId(kernel.mint_id());
        let relay = serve(
            rd,
            wr,
            id.clone(),
            relay_id,
            Arc::clone(kernel),
            stop,
            Some(Extra {
                inbound: in_tx,
                outbound: out_rx,
            }),
        );
        // the ops this base adds to the relay: hello, result
        let (k, me, r) = (Arc::clone(kernel), id.clone(), Arc::clone(&run));
        let declared: HashSet<String> = self.verbs.keys().cloned().collect();
        let name = self.name.clone();
        let ops = async move {
            while let Some(m) = in_rx.recv().await {
                match m.get("op").and_then(Value::as_str) {
                    Some("hello") => {
                        let served = m.get("verbs").and_then(Value::as_array).map(|a| {
                            a.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect::<Vec<_>>()
                        });
                        let ok = served.as_ref().is_some_and(|s| {
                            s.len() == m["verbs"].as_array().map_or(0, Vec::len)
                                && s.iter().cloned().collect::<HashSet<_>>() == declared
                        });
                        if !ok {
                            let got = match &served {
                                Some(s) => {
                                    let mut s = s.clone();
                                    s.sort();
                                    py_repr_value(&json!(s))
                                }
                                None => py_repr_value(m.get("verbs").unwrap_or(&Value::Null)),
                            };
                            let mut want: Vec<&String> = declared.iter().collect();
                            want.sort();
                            stop_run(
                                &me,
                                &r,
                                format!(
                                    "{name}: the program serves {got} but bundle.yaml declares {} \
                                     — they must match",
                                    py_repr_value(&json!(want))
                                ),
                                false,
                            );
                            continue;
                        }
                        r.phase.send_replace(Phase::Running);
                        k.emit(&me, json!({"type": "status", "phase": "running"}))
                            .await;
                    }
                    Some("result") => {
                        let rid = m
                            .get("rid")
                            .map(fantastic_bundle::py_str)
                            .unwrap_or_default();
                        if let Some(tx) = r.pending.lock().expect("pending").remove(&rid) {
                            let _ = tx.send(m.get("result").cloned().unwrap_or(Value::Null));
                        }
                    }
                    _ => {}
                }
            }
        };
        // stdout/stderr are PIPES: unread, a chatty program blocks once one fills.
        // stdout is the program's own; stderr's tail is kept for reflect (a crash's
        // traceback is the one thing that says why it died).
        let (mut so, mut se) = (child.stdout.take(), child.stderr.take());
        let r3 = Arc::clone(&run);
        let output = async move {
            use tokio::io::AsyncReadExt;
            let out = async {
                if let Some(o) = so.as_mut() {
                    let mut buf = vec![0u8; 65536];
                    while matches!(o.read(&mut buf).await, Ok(n) if n > 0) {}
                }
            };
            let err = async {
                if let Some(e) = se.as_mut() {
                    let mut buf = vec![0u8; 65536];
                    while let Ok(n) = e.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        let mut t = r3.stderr.lock().expect("stderr");
                        t.push_str(&String::from_utf8_lossy(&buf[..n]));
                        let extra = t.chars().count().saturating_sub(STDERR_TAIL);
                        if extra > 0 {
                            *t = t.chars().skip(extra).collect();
                        }
                    }
                }
            };
            tokio::join!(out, err);
        };
        let (k2, me2, r2) = (Arc::clone(kernel), id.clone(), Arc::clone(&run));
        let wait = async move {
            let code = child
                .wait()
                .await
                .ok()
                .and_then(fantastic_exec_bridge::jobs::exit_code);
            if *r2.phase.borrow() != Phase::Stopped {
                *r2.code.lock().expect("code") = code;
                let shown = code.map_or_else(|| "None".to_string(), |c| c.to_string());
                stop_run(
                    &me2,
                    &r2,
                    format!("{}: its program exited (code {shown})", me2.as_str()),
                    true,
                );
                k2.emit(
                    &me2,
                    json!({"type": "status", "phase": "stopped", "reason": "exited", "code": code}),
                )
                .await;
            }
        };
        let tasks = [
            kernel.spawn_task(id, "relay", relay),
            kernel.spawn_task(id, "ops", ops),
            kernel.spawn_task(id, "program", wait),
            kernel.spawn_task(id, "output", output),
        ];
        run.task_ids
            .lock()
            .expect("task ids")
            .extend(tasks.iter().map(|t| t.0));
        Value::Null
    }

    async fn call(&self, id: &AgentId, payload: &Value) -> Value {
        let run = run_of(id);
        let Some(run) = run.filter(|r| *r.phase.borrow() != Phase::Stopped) else {
            let why = run_of(id)
                .and_then(|r| r.error.lock().expect("error").clone())
                .unwrap_or_else(|| format!("{}: its program is not running", id.as_str()));
            return refusal_hint(
                TRANSPORT_CLOSED,
                why,
                "fix the bundle, then the root's reload_bundle restarts it",
            );
        };
        let mut rx = run.phase.subscribe();
        let _ = rx.wait_for(|p| *p != Phase::Starting).await;
        if *run.phase.borrow() != Phase::Running {
            let why = run
                .error
                .lock()
                .expect("error")
                .clone()
                .unwrap_or_else(|| format!("{}: its program stopped", id.as_str()));
            return refusal(TRANSPORT_CLOSED, why);
        }
        let rid = (run.rid.fetch_add(1, Ordering::SeqCst) + 1).to_string();
        let (tx, rx) = oneshot::channel();
        run.pending.lock().expect("pending").insert(rid.clone(), tx);
        let _ = run
            .out
            .send(json!({"op": "call", "rid": rid, "payload": payload}));
        match rx.await {
            Ok(v) => v,
            Err(_) => refusal(
                TRANSPORT_CLOSED,
                format!("{}: its program stopped", id.as_str()),
            ),
        }
    }
}

/// Stop the program (kill its session unless it already exited), settle every
/// in-flight call with `why`, release the waiters. Python `_stop`.
fn stop_run(id: &AgentId, run: &Run, why: String, exited: bool) {
    let _ = id;
    if *run.phase.borrow() == Phase::Stopped {
        return;
    }
    *run.error.lock().expect("error") = Some(why.clone());
    run.phase.send_replace(Phase::Stopped);
    if !exited && run.pid > 0 {
        kill_session(run.pid);
    }
    for (_, tx) in run.pending.lock().expect("pending").drain() {
        let _ = tx.send(refusal(TRANSPORT_CLOSED, why.clone()));
    }
    run.stop.notify_one();
}

fn stop_by_id(id: &AgentId, why: String) {
    if let Some(r) = run_of(id) {
        stop_run(id, &r, why, false);
    }
}

#[async_trait]
impl Bundle for FolderBundle {
    fn name(&self) -> &str {
        &self.name
    }

    fn verb_docs(&self) -> Value {
        Value::Object(self.verbs.clone())
    }

    fn emits_doc(&self) -> Option<Value> {
        self.emits.clone()
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(json!(self.notes))
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let t = payload.get("type").cloned().unwrap_or(Value::Null);
        let verb = t.as_str().unwrap_or("");
        let reply = match verb {
            "reflect" => self.instance_reflect(agent_id),
            "boot" => self.start(agent_id, kernel).await,
            "shutdown" => {
                // a reload (`reload_bundle`: shutdown, then boot) or a stop
                let reason = payload
                    .get("reason")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("the kernel stopped");
                stop_by_id(agent_id, format!("{}: {reason}", agent_id.as_str()));
                Value::Null
            }
            v if self.verbs.contains_key(v) => self.call(agent_id, payload).await,
            _ => {
                let names: Vec<&str> = self.verbs.keys().map(String::as_str).collect();
                let mut r = refusal_hint(
                    UNHANDLED,
                    format!("{}: no verb {}", self.name, py_repr_value(&t)),
                    format!("its verbs: {}", names.join(", ")),
                );
                with(&mut r, "verb", t);
                r
            }
        };
        if reply.is_null() {
            return Ok(None);
        }
        Ok(Some(reply))
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        stop_by_id(agent_id, format!("{}: deleted", agent_id.as_str()));
        Ok(())
    }

    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        stop_by_id(
            agent_id,
            format!("{}: the kernel stopped", agent_id.as_str()),
        );
        Ok(())
    }

    async fn on_cancel(
        &self,
        agent_id: &AgentId,
        task_id: &TaskId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        // the program's own task (relay / ops / wait) cancelled: the process goes too
        if let Some(r) = run_of(agent_id) {
            if r.task_ids.lock().expect("task ids").contains(&task_id.0) {
                stop_run(
                    agent_id,
                    &r,
                    format!("{}: cancelled", agent_id.as_str()),
                    false,
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
