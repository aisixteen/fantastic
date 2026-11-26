//! The job table and the pump.
//!
//! A job OUTLIVES the `start` dispatch that made it, so it is registered in the kernel
//! TASK TABLE: it shows in `ps`, and `cancel` reaches it through `on_cancel`, which
//! kills the real OS process. A subprocess nobody can reach is the orphan bug.

use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, OnceLock};

use fantastic_kernel::{AgentId, Kernel, TaskId};
use indexmap::IndexMap;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};

/// asyncio's `StreamReader` default buffer bound. python reads every pipe through
/// one, so a line longer than this is not truncated — `readline` RAISES, and the
/// pump reports the raise as the job's stderr.
const LINE_LIMIT: usize = 16 * 1024 * 1024; // python raises asyncio's 64 KiB default to this

/// The text `asyncio.StreamReader.readline` raises past [`LINE_LIMIT`], verbatim.
const LINE_LIMIT_ERROR: &str = "Separator is not found, and chunk exceed the limit";

/// One in-flight (or finished) program.
///
/// Keyed in [`jobs`] by its id — which IS the kernel TASK id of its pump, minted
/// by the one counter when `start` forks it (python `_start`). The row carries no
/// second copy of that number: the key is the job's id in `status`, its row in
/// `ps`, and what `cancel task_id=` names.
pub struct Job {
    /// The agent that owns it. Every verb filters on this, so a known job_id from
    /// another agent is still "no job".
    pub owner: AgentId,
    /// OS pid — what `stop` kills the job's session (Job Object) by and
    /// `interrupt` addresses.
    pub pid: u32,
    /// The relay task serving this job's control socket — its row in the kernel
    /// task table, like the pump above. It is real work (a send per frame the
    /// program writes), so it is visible to `ps` and reachable by `cancel`.
    pub ctrl: TaskId,
    /// The relay's STOP SIGNAL — raised by [`teardown`] instead of aborting
    /// `ctrl`. An abort drops `serve` at its await point, so the release code
    /// that runs AFTER its read loop never runs: every proxy the job held would
    /// stay in `kernel.inboxes` and in each watched source's watcher set, with
    /// orphan drain tasks writing into a closed socket. `serve` selects this
    /// against its read and falls through to its own cleanup.
    pub stop: Arc<tokio::sync::Notify>,
    /// The exact invocation — the program/binary. Reported by `status` so a caller
    /// that wants to re-run a job re-issues these verbatim.
    pub command: String,
    /// The exact invocation — the argv passed to `command`.
    pub args: Vec<String>,
    /// The working directory the job ran in, exactly as `start` resolved it
    /// (payload, else record, else null) — reported raw by `status`.
    pub cwd: Value,
    /// `running` | `stopping` | `done` | `failed` | `stopped`.
    pub status: String,
    /// Unix epoch seconds at spawn.
    pub started_at: f64,
    /// Process exit code; `None` while it runs. Negative on a signal death (unix).
    pub exit_code: Option<i32>,
    /// Output lines seen so far — ONE counter across stdout and stderr.
    pub lines: u64,
    /// The most recent output line.
    pub last_line: String,
    /// THE LIVE LINE RING: every line a pipe job prints, as
    /// `(n, stream, line)`, the last `LINE_RING` bytes — what `tail`/`grep` read,
    /// running or ended. The job's only copy of its output.
    pub line_ring: std::collections::VecDeque<(u64, String, String)>,
    /// Bytes held in `line_ring` (each line + its newline).
    pub line_ring_bytes: usize,
    /// Lines the ring already dropped from its front.
    pub line_ring_dropped: u64,
    /// The terminal of a PTY job (`start pty={cols,rows}`); `None` for a pipe job.
    pub pty: Option<Arc<crate::pty::Pty>>,
    /// Set when the pump told this job's monitors it ended (python
    /// `monitors_closed`). `monitor` refuses the job from then on, even while its
    /// process still reads alive — a monitor filed after the end would never end.
    pub monitors_closed: bool,
    /// A `run` whose reply is the job's END: its deferred settle, taken once when
    /// the job ends ([`settle_run`]).
    pub run_end: Option<RunEnd>,
}

/// The `run` call waiting for its job's end: the execution that settles then, how
/// many of the last lines its reply carries, and the kernel it settles on.
pub struct RunEnd {
    pub(crate) pid: TaskId,
    pub(crate) tail: u64,
    pub(crate) kernel: Arc<Kernel>,
}

/// `run`'s reply: the job's last `tail` lines, its status and its exit code.
pub(crate) fn run_reply_of(owner: &AgentId, job_id: TaskId, tail: u64) -> Value {
    let jid = crate::JobRef::of(job_id.get());
    let (status, exit_code) = {
        let g = jobs();
        match jid.find(&g, owner) {
            Some(j) => (j.status.clone(), j.exit_code),
            None => ("running".to_string(), None),
        }
    };
    let mut reply = crate::tail::tail_reply(owner, &jid, tail, crate::verbs::Stream::Both);
    fantastic_kernel::refusal::with(&mut reply, "status", json!(status));
    fantastic_kernel::refusal::with(&mut reply, "exit_code", json!(exit_code));
    reply
}

/// THE JOB ENDED: a `run` waiting for it settles now with its output, as a message
/// to its caller (the kernel delivers it). No lock is held while it settles.
pub(crate) fn settle_run(owner: &AgentId, job_id: TaskId) {
    let waiting = jobs()
        .get_mut(&(owner.clone(), job_id))
        .and_then(|j| j.run_end.take());
    let Some(end) = waiting else { return };
    let reply = run_reply_of(owner, job_id, end.tail);
    let out = fantastic_kernel::Outcome::new(
        Some(end.pid),
        fantastic_kernel::Status::Completed,
        reply,
    );
    end.kernel.settle_deferred(end.pid, out);
}

/// A job's key: its OWNER and its id (the pump's kernel task id).
///
/// Within one kernel the task id alone is unique — python keys `_jobs` by it — but
/// this table is process-global, and a process that hosts several kernels (the
/// test binary does, in parallel) has several counters minting the same small
/// numbers. Every verb already looks a job up for ONE owner, so the owner is part
/// of the key: a job of another agent — or of another kernel's agent — is simply
/// not found, exactly as before.
pub type JobKey = (AgentId, TaskId);

static JOBS: OnceLock<Mutex<IndexMap<JobKey, Job>>> = OnceLock::new();

/// The job table. Module-global, shared across every exec_bridge agent in the
/// process; every operation filters by `owner`.
///
/// INSERTION-ORDERED, because python's is a `dict`: `status` with no `job_id`
/// answers this agent's jobs in START order, and `clear` removes with
/// `shift_remove` so the survivors keep it.
pub fn jobs() -> MutexGuard<'static, IndexMap<JobKey, Job>> {
    JOBS.get_or_init(|| Mutex::new(IndexMap::new()))
        .lock()
        .expect("JOBS poisoned")
}

/// THE definition of a live job: its OS process has not exited (py `_alive`).
///
/// ONE definition for everything that asks — mail, reflect's count,
/// `stop`/`interrupt`, `teardown`. The status STRING lags reality (a killed-but-
/// unreaped job reads `stopping` while its process is very much alive), and `clear`
/// is the only caller that legitimately wants the label.
#[must_use]
pub fn alive(j: &Job) -> bool {
    j.exit_code.is_none()
}

/// Any live process of this owner (py `_has_running_job`).
pub fn has_running_job(owner: &AgentId) -> bool {
    jobs().values().any(|j| &j.owner == owner && alive(j))
}

/// Any live process of this owner beside job `except`.
pub fn has_other_running_job(owner: &AgentId, except: TaskId) -> bool {
    jobs()
        .iter()
        .any(|((o, id), j)| o == owner && *id != except && alive(j))
}

/// A `status` snapshot. `full` adds the collected output.
///
/// `job_id` is the job's kernel TASK id, an INT — `cancel task_id=<job_id>` takes
/// the same number — so there is no separate `task_id` field to disagree with it.
pub fn snap(job_id: TaskId, j: &Job, full: bool) -> Value {
    let mut v = json!({
        "job_id": job_id.get(),
        "status": j.status,
        "exit_code": j.exit_code,
        "started_at": j.started_at,
        "lines": j.lines,
        "last_line": j.last_line,
        // WHAT ran — so a caller can re-issue this job verbatim.
        "command": j.command,
        "args": j.args,
        "pty": j.pty.is_some(),
    });
    if let Some(p) = &j.pty {
        let (cols, rows) = p.dims();
        v["cols"] = json!(cols);
        v["rows"] = json!(rows);
    }
    if full {
        v["cwd"] = j.cwd.clone();
    }
    v
}

/// Read the rest of a pipe and throw it away — see the overrun branch in
/// [`read_stream`].
async fn drain_to_eof<R: tokio::io::AsyncRead + Unpin>(rd: &mut BufReader<R>) {
    loop {
        let n = match rd.fill_buf().await {
            Ok(c) if !c.is_empty() => c.len(),
            _ => return,
        };
        rd.consume(n);
    }
}

/// The pump's own line counter, shared by the job's two pipes.
///
/// It lives HERE and not on the job row because python's counter lives on the dict
/// the pump closed over.
#[derive(Default)]
struct Counter {
    n: u64,
}

/// One pipe read into lines, python's way.
///
/// Chunk-by-chunk, not a line iterator: python's `raw` KEEPS its newline and is
/// appended verbatim to the collected text, a lone `\r` is data (only `\n` is
/// stripped for `line`/`last_line`), and a final line with no newline is collected
/// with no newline added. Invalid UTF-8 is REPLACED, not dropped, and the pipe keeps
/// draining past it.
///
/// The buffer is [`LINE_LIMIT`]-sized on purpose: asyncio re-scans for the separator
/// once per transport read and raises only when a scan finds MORE than the limit
/// buffered with no separator — so the read granularity is part of where the raise
/// falls, and a 64 KiB granularity is what a pipe gives python. `Err(())` is that
/// raise, which the pump turns into the `[pump error]` stderr.
async fn read_stream<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    owner: AgentId,
    job_id: TaskId,
    stream: &'static str,
    kernel: Arc<Kernel>,
    counter: Arc<Mutex<Counter>>,
) -> Result<(), ()> {
    let mut rd = BufReader::with_capacity(LINE_LIMIT, reader);
    loop {
        // One line, assembled a buffered chunk at a time so the LIMIT is checked as
        // the data arrives — the whole point of it is that an unterminated stream is
        // never buffered without bound.
        let mut raw: Vec<u8> = Vec::new();
        let mut eof = false;
        loop {
            let chunk = match rd.fill_buf().await {
                Ok(c) => c,
                Err(_) => &[][..],
            };
            if chunk.is_empty() {
                eof = true;
                break;
            }
            let sep = chunk.iter().position(|b| *b == b'\n');
            let take = sep.map_or(chunk.len(), |i| i + 1);
            raw.extend_from_slice(&chunk[..take]);
            rd.consume(take);
            if sep.is_some() {
                break;
            }
            if raw.len() > LINE_LIMIT {
                // The raise. KEEP DRAINING the pipe, discarding: python's transport
                // goes on feeding the (now cleared) StreamReader buffer that nobody
                // reads, and a pipe left unread blocks the child's next write, so
                // the process never exits and `wait()` never returns.
                drain_to_eof(&mut rd).await;
                return Err(());
            }
        }
        if raw.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        let line = text.trim_end_matches('\n').to_string();
        crate::monitor::feed_line(&owner, job_id, &line);
        let n = {
            let mut c = counter.lock().expect("line counter poisoned");
            c.n += 1;
            c.n
        };
        // Touch the table only to WRITE. Reading our own row here would race the
        // `start` that is still holding the lock while spawn_task begins running us.
        {
            let mut g = jobs();
            if let Some(j) = g.get_mut(&(owner.clone(), job_id)) {
                j.lines = n;
                j.last_line = line.clone();
                crate::tail::line_ring_add(j, n, stream, &line);
            }
        }
        kernel
            .emit(
                &owner,
                json!({"type": "progress", "job_id": job_id.get(), "n": n, "line": line, "stream": stream}),
            )
            .await;
        if eof {
            return Ok(());
        }
    }
}

/// The pump's end of `start`'s hand-off — and the job's END GUARD, held by the
/// pump's future FROM ITS FORK.
///
/// The job's id is the pump's own task id, so it exists only after `spawn_task` has
/// forked the pump — and a forked task may already be running on another worker by
/// then. `start` files the row FIRST, announces the job, and only then hands the id
/// over ([`Filed`]), so the pump never looks for a row that is not there yet (python's
/// `ready`) and never emits before `job_started`.
///
/// On EVERY way out of the pump — a return, an abort at an await point, or a cancel
/// that lands before the pump ever RAN (the kernel's task body takes a pending
/// cancel first and drops the future unpolled) — this guard is dropped, and once the
/// row was filed it tells the job's monitors the job ended and, unless the pump wrote
/// the real outcome itself ([`Ready::disarm`]), finalises the row. Without it the row
/// kept the label [`teardown`] left on it — `stopping`, with `exit_code: None` —
/// forever, and `clear` refuses `stopping` rows BY DESIGN, so the row was immortal and
/// the job could never be forgotten. Python's pump catches `CancelledError` and
/// finalises to `stopped`; this guard is the same guarantee, spelled as a `Drop`.
/// It lives in the pump's ARGUMENTS, never in its body: a body that never ran made
/// no guard.
pub struct Ready {
    rx: Option<tokio::sync::oneshot::Receiver<TaskId>>,
    id: Arc<OnceLock<TaskId>>,
    owner: AgentId,
    armed: bool,
}

/// `start`'s end of the hand-off: the id, given once the row is filed. Dropped
/// before [`Filed::go`] (the `start` dispatch died), the pump kills its child and
/// ends — finalising the row when [`Filed::filed`] named it.
pub(crate) struct Filed {
    tx: tokio::sync::oneshot::Sender<TaskId>,
    id: Arc<OnceLock<TaskId>>,
}

/// The hand-off between `start` and the pump of `owner`'s next job.
pub(crate) fn hand_off(owner: &AgentId) -> (Filed, Ready) {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let id = Arc::new(OnceLock::new());
    (
        Filed {
            tx,
            id: Arc::clone(&id),
        },
        Ready {
            rx: Some(rx),
            id,
            owner: owner.clone(),
            armed: true,
        },
    )
}

impl Filed {
    /// The row `job_id` exists: the pump's end guard covers it from here.
    pub(crate) fn filed(&self, job_id: TaskId) {
        let _ = self.id.set(job_id);
    }

    /// The pump may run now: it looks for the row [`Filed::filed`] named.
    pub(crate) fn go(self) {
        if let Some(&id) = self.id.get() {
            let _ = self.tx.send(id);
        }
    }
}

impl Ready {
    /// Wait for the job's id. `None` only if `start` dropped its [`Filed`] before
    /// [`Filed::go`] — it has no refusal path after the fork, so that means the
    /// dispatch itself died. Nobody may then hold the process's id, so it is killed
    /// here rather than left running unowned (a child is spawned with
    /// `kill_on_drop(false)`); a row already filed is finalised by this guard's drop.
    pub(crate) async fn id(&mut self, child: &crate::sys::Child) -> Option<TaskId> {
        let id = if let Some(rx) = self.rx.take() {
            rx.await.ok()
        } else {
            None
        };
        if id.is_none() {
            if let Some(pid) = child.id() {
                crate::spawn::kill_session(pid as i32);
            }
        }
        id
    }

    /// The pump reached the end and wrote the REAL outcome itself.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Ready {
    fn drop(&mut self) {
        let Some(&job_id) = self.id.get() else {
            return; // no row was filed: nothing to finalise, no monitor to tell
        };
        crate::monitor::job_ended(&self.owner, job_id);
        if !self.armed {
            return;
        }
        let key = (self.owner.clone(), job_id);
        {
            let mut g = jobs();
            if let Some(j) = g.get_mut(&key) {
                if j.exit_code.is_none() {
                    // `on_cancel` SIGKILLed the child before the abort; say so, and
                    // leave a label `clear` accepts.
                    j.status = "stopped".to_string();
                    j.exit_code = Some(crate::sys::KILLED_EXIT_CODE);
                }
            }
        }
        // a `run` of the job is told it ended
        settle_run(&key.0, key.1);
    }
}

/// The job's task-table body: drain both pipes, await exit, emit `job_done`.
pub async fn pump(owner: AgentId, mut ready: Ready, mut child: crate::sys::Child, kernel: Arc<Kernel>) {
    let Some(job_id) = ready.id(&child).await else {
        return;
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // ONE counter across both pipes, held by the PUMP: `n` is a single monotonic
    // sequence over stdout and stderr.
    let counter = Arc::new(Mutex::new(Counter::default()));
    let (ro, re) = tokio::join!(
        async {
            match stdout {
                Some(s) => {
                    read_stream(
                        s,
                        owner.clone(),
                        job_id,
                        "stdout",
                        Arc::clone(&kernel),
                        Arc::clone(&counter),
                    )
                    .await
                }
                None => Ok(()),
            }
        },
        async {
            match stderr {
                Some(s) => {
                    read_stream(
                        s,
                        owner.clone(),
                        job_id,
                        "stderr",
                        Arc::clone(&kernel),
                        Arc::clone(&counter),
                    )
                    .await
                }
                None => Ok(()),
            }
        }
    );
    // py `except Exception as e: fault = f"[pump error] {e}"`: a raise from EITHER
    // pipe is the job's `error`.
    let fault = match (ro, re) {
        (Ok(()), Ok(())) => None,
        _ => Some(format!("[pump error] {LINE_LIMIT_ERROR}")),
    };
    // py `code = await process.wait()`: a signal death is `-signal`, not `None`.
    let code = child.wait().await.ok().and_then(exit_code);
    ready.disarm();
    let (lines, last_line) = {
        let mut g = jobs();
        match g.get_mut(&(owner.clone(), job_id)) {
            None => (0, String::new()),
            Some(j) => {
                j.exit_code = code;
                j.status = if j.status == "stopping" {
                    "stopped".to_string()
                } else if code == Some(0) {
                    "done".to_string()
                } else {
                    "failed".to_string()
                };
                (j.lines, j.last_line.clone())
            }
        }
    };
    settle_run(&owner, job_id);
    let mut done = json!({"type": "job_done", "job_id": job_id.get(), "exit_code": code,
                          "ok": code == Some(0), "lines": lines, "last_line": last_line});
    if let Some(f) = fault {
        done["error"] = json!(f);
    }
    kernel.emit(&owner, done).await;
}

/// python's `Process.returncode`: the exit status, or `-N` when a signal killed it.
///
/// `ExitStatus::code()` answers `None` for a signal death, which read as "still
/// running" everywhere liveness is `exit_code.is_none()` and lost the cause of death
/// from `job_done`.
pub fn exit_code(s: std::process::ExitStatus) -> Option<i32> {
    crate::sys::exit_code(s)
}

/// SIGKILL one job: its whole SESSION — the command and everything it launched
/// (a pipe job's `server &`, a PTY shell's background jobs). Every job leads its
/// own session (`spawn`), so this never reaches the kernel. Returns whether the
/// job's own process was signalled.
pub fn kill(j: &Job) -> bool {
    crate::spawn::kill_session(j.pid as i32)
}

/// Interrupt one job — its Ctrl-C (python `proc.send_signal(SIGINT)`): SIGINT to
/// its process on unix; on Windows a Ctrl-C typed at a PTY job's console, a
/// CTRL_BREAK to a pipe job's process group. Returns whether it was delivered.
pub fn interrupt(j: &Job) -> bool {
    crate::sys::interrupt(j.pid, j.pty.as_deref().map(crate::pty::Pty::master))
}

/// SIGKILL the job's whole session and ask the relay to stop.
/// Idempotent; every death path (`stop`, `cancel`, `on_delete`) goes through here.
pub fn teardown(j: &mut Job) {
    if alive(j) {
        j.status = "stopping".to_string();
        // Best effort: an already-reaped pid is not an error.
        kill(j);
    }
    // SIGNAL, never `ctrl.abort()`. Aborting drops `serve` at its await point, so
    // the proxy/watcher release that runs after its read loop is skipped and every
    // `_xbw_*`/`_xbself_*` id the job held leaks — watcher rows, live mpsc senders
    // in `kernel.inboxes`, and detached drain tasks. `notify_one` leaves a permit
    // if the relay is mid-`send`, so the signal is never missed.
    j.stop.notify_one();
}

/// TEST ONLY: what a job printed on `stream`, from its line ring — one line per
/// entry, each with its newline (the ring keeps lines, not raw text).
#[cfg(test)]
pub(crate) fn printed(owner: &AgentId, done: &Value, stream: &str) -> String {
    let jid = TaskId(done["job_id"].as_i64().expect("job_done names its job"));
    jobs()
        .get(&(owner.clone(), jid))
        .map(|j| {
            j.line_ring
                .iter()
                .filter(|(_, s, _)| s == stream)
                .map(|(_, _, l)| format!("{l}\n"))
                .collect()
        })
        .unwrap_or_default()
}
