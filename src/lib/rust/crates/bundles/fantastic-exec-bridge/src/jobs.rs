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
/// pump reports the raise as the job's stderr (A4.14).
const LINE_LIMIT: usize = 16 * 1024 * 1024; // python raised asyncio's 64 KiB default (2026-09-20)

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
    /// OS pid — `stop`/`interrupt` signal it directly.
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
    /// Process exit code; `None` while it runs. Negative on a signal death.
    pub exit_code: Option<i32>,
    /// Output lines seen so far — ONE counter across stdout and stderr.
    pub lines: u64,
    /// The most recent output line.
    pub last_line: String,
    /// Collected stdout (newlines preserved).
    pub stdout: String,
    /// Collected stderr (newlines preserved).
    pub stderr: String,
    /// The terminal of a PTY job (`start pty={cols,rows}`); `None` for a pipe job.
    pub pty: Option<Arc<crate::pty::Pty>>,
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
/// answers this agent's jobs in START order (A4.12), and `clear` removes with
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
        if j.pty.is_none() {
            v["stdout"] = json!(j.stdout);
            v["stderr"] = json!(j.stderr);
        }
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
/// the pump closed over (A4.15).
#[derive(Default)]
struct Counter {
    n: u64,
}

/// One pipe read into lines, python's way (A4.14 / A4.21).
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
) -> Result<String, ()> {
    let mut rd = BufReader::with_capacity(LINE_LIMIT, reader);
    let mut collected = String::new();
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
            return Ok(collected);
        }
        let text = String::from_utf8_lossy(&raw).into_owned();
        collected.push_str(&text);
        let line = text.trim_end_matches('\n').to_string();
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
            }
        }
        kernel
            .emit(
                &owner,
                json!({"type": "progress", "job_id": job_id.get(), "n": n, "line": line, "stream": stream}),
            )
            .await;
        if eof {
            return Ok(collected);
        }
    }
}

/// Finalise the job row on EVERY way out of the pump — including the one that is
/// not a return.
///
/// `cancel` ABORTS the pump task, which drops its future at the current await
/// point: `child.wait()` never returns and the finalisation below it never runs.
/// The row then kept the label [`teardown`] left on it — `stopping`, with
/// `exit_code: None` — forever, and `clear` refuses `stopping` rows BY DESIGN, so
/// the row was immortal and the job could never be forgotten. Python's pump
/// catches `CancelledError` and finalises to `stopped`; this guard is the same
/// guarantee, spelled as a `Drop`.
pub(crate) struct FinalizeRow {
    key: JobKey,
    armed: bool,
}

impl FinalizeRow {
    /// Armed until the pump writes the real outcome itself.
    pub(crate) fn new(key: JobKey) -> Self {
        Self { key, armed: true }
    }

    /// The pump reached the end and wrote the REAL outcome itself.
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for FinalizeRow {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut g = jobs();
        if let Some(j) = g.get_mut(&self.key) {
            if j.exit_code.is_none() {
                // `on_cancel` SIGKILLed the child before the abort; say so, and
                // leave a label `clear` accepts.
                j.status = "stopped".to_string();
                j.exit_code = Some(-9);
            }
        }
    }
}

/// The job's id, handed to its pump once the row exists (python's `ready`).
///
/// The id IS the pump's own task id, so it exists only after `spawn_task` has
/// forked the pump — and a forked task may already be running on another worker
/// by then. `start` files the row FIRST and only then sends the id, so the pump
/// never looks for a row that is not there yet.
pub type Ready = tokio::sync::oneshot::Receiver<TaskId>;

/// Wait for the job's id. `None` only if `start` dropped the sender without
/// filing the row — it has no refusal path after the fork, so that means the
/// dispatch itself died. There is then no row to report into and nobody who can
/// reach the process, so it is killed here rather than left running unowned
/// (a child is spawned with `kill_on_drop(false)`).
pub(crate) async fn await_id(ready: Ready, child: &tokio::process::Child) -> Option<TaskId> {
    let id = ready.await.ok();
    if id.is_none() {
        if let Some(pid) = child.id() {
            crate::spawn::kill_session(pid as i32);
        }
    }
    id
}

/// The job's task-table body: drain both pipes, await exit, emit `job_done`.
pub async fn pump(
    owner: AgentId,
    ready: Ready,
    mut child: tokio::process::Child,
    kernel: Arc<Kernel>,
) {
    let Some(job_id) = await_id(ready, &child).await else {
        return;
    };
    let mut finalize = FinalizeRow::new((owner.clone(), job_id));
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
                None => Ok(String::new()),
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
                None => Ok(String::new()),
            }
        }
    );
    // py `out, err = await asyncio.gather(...)` under `except Exception as e: err =
    // f"[pump error] {e}"`: a raise from EITHER pipe leaves `out` empty and puts the
    // message in `err`, whatever the other pipe had read (A4.14).
    let (o, e) = match (ro, re) {
        (Ok(o), Ok(e)) => (o, e),
        _ => (String::new(), format!("[pump error] {LINE_LIMIT_ERROR}")),
    };
    // py `code = await process.wait()`: a signal death is `-signal`, not `None`.
    let code = child.wait().await.ok().and_then(exit_code);
    finalize.disarm();
    {
        let mut g = jobs();
        if let Some(j) = g.get_mut(&(owner.clone(), job_id)) {
            j.exit_code = code;
            j.stdout = o.clone();
            j.stderr = e.clone();
            j.status = if j.status == "stopping" {
                "stopped".to_string()
            } else if code == Some(0) {
                "done".to_string()
            } else {
                "failed".to_string()
            };
        }
    }
    kernel
        .emit(
            &owner,
            json!({"type": "job_done", "job_id": job_id.get(), "exit_code": code,
                   "ok": code == Some(0), "stdout": o, "stderr": e}),
        )
        .await;
}

/// python's `Process.returncode`: the exit status, or `-N` when a signal killed it.
///
/// `ExitStatus::code()` answers `None` for a signal death, which read as "still
/// running" everywhere liveness is `exit_code.is_none()` and lost the cause of death
/// from `job_done` (A4.6).
pub(crate) fn exit_code(s: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    s.code().or_else(|| s.signal().map(|n| -n))
}

/// SIGKILL one job: its whole SESSION — the command and everything it launched
/// (a pipe job's `server &`, a PTY shell's background jobs). Every job leads its
/// own session (`spawn`), so this never reaches the kernel. Returns whether the
/// job's own process was signalled.
pub fn kill(j: &Job) -> bool {
    crate::spawn::kill_session(j.pid as i32)
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
