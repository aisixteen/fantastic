//! The job table and the pump.
//!
//! A job OUTLIVES the `start` dispatch that made it, so it is registered in the kernel
//! TASK TABLE: it shows in `ps`, and `cancel` reaches it through `on_cancel`, which
//! kills the real OS process. A subprocess nobody can reach is the orphan bug.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard, OnceLock};

use fantastic_kernel::{AgentId, Kernel, TaskId};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::JoinHandle;

/// One in-flight (or finished) program.
pub struct Job {
    /// The agent that owns it. Every verb filters on this, so a known job_id from
    /// another agent is still "no job".
    pub owner: AgentId,
    /// OS pid — `stop`/`interrupt` signal it directly.
    pub pid: u32,
    /// The PUMP's row in the kernel task table. Liveness is derived from this and
    /// nothing else, so there is no second flag to drift.
    pub task: TaskId,
    /// The relay task serving this job's control socket.
    pub ctrl: JoinHandle<()>,
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
}

static JOBS: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();

/// The job table. Module-global, shared across every exec_bridge agent in the
/// process; every operation filters by `owner`.
pub fn jobs() -> MutexGuard<'static, HashMap<String, Job>> {
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("JOBS poisoned")
}

/// Any live process of this owner. The task table is the oracle.
pub fn has_running_job(owner: &AgentId, kernel: &Arc<Kernel>) -> bool {
    jobs()
        .values()
        .any(|j| &j.owner == owner && kernel.tasks.contains(&j.task))
}

/// A `status` snapshot. `full` adds the collected output.
pub fn snap(job_id: &str, j: &Job, full: bool) -> Value {
    let mut v = json!({
        "job_id": job_id,
        "task_id": j.task.to_string(),
        "status": j.status,
        "exit_code": j.exit_code,
        "started_at": j.started_at,
        "lines": j.lines,
        "last_line": j.last_line,
    });
    if full {
        v["stdout"] = json!(j.stdout);
        v["stderr"] = json!(j.stderr);
    }
    v
}

/// Drain one pipe line-by-line: a `progress` event per line, and collect the text.
///
/// The line counter is SHARED across stdout and stderr (it lives on the job row), so
/// `n` is one monotonic sequence over both — python's behaviour, and what makes an
/// interleaved log reconstructable.
async fn read_stream<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    owner: AgentId,
    job_id: String,
    stream: &'static str,
    kernel: Arc<Kernel>,
) -> String {
    let mut lines = BufReader::new(reader).lines();
    let mut collected = String::new();
    while let Ok(Some(line)) = lines.next_line().await {
        collected.push_str(&line);
        collected.push('\n');
        // Touch the table only to WRITE. Reading our own row here would race the
        // `start` that is still holding the lock while spawn_task begins running us.
        let n = {
            let mut g = jobs();
            match g.get_mut(&job_id) {
                Some(j) => {
                    j.lines += 1;
                    j.last_line = line.clone();
                    j.lines
                }
                None => 0, // cleared or deleted mid-flight — still emit, just don't book
            }
        };
        kernel
            .emit(
                &owner,
                json!({"type": "progress", "job_id": job_id, "n": n, "line": line, "stream": stream}),
            )
            .await;
    }
    collected
}

/// The job's task-table body: drain both pipes, await exit, emit `job_done`.
pub async fn pump(
    owner: AgentId,
    job_id: String,
    mut child: tokio::process::Child,
    kernel: Arc<Kernel>,
) {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (o, e) = tokio::join!(
        async {
            match stdout {
                Some(s) => {
                    read_stream(
                        s,
                        owner.clone(),
                        job_id.clone(),
                        "stdout",
                        Arc::clone(&kernel),
                    )
                    .await
                }
                None => String::new(),
            }
        },
        async {
            match stderr {
                Some(s) => {
                    read_stream(
                        s,
                        owner.clone(),
                        job_id.clone(),
                        "stderr",
                        Arc::clone(&kernel),
                    )
                    .await
                }
                None => String::new(),
            }
        }
    );
    let code = child.wait().await.ok().and_then(|s| s.code());
    {
        let mut g = jobs();
        if let Some(j) = g.get_mut(&job_id) {
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
            json!({"type": "job_done", "job_id": job_id, "exit_code": code,
                   "ok": code == Some(0), "stdout": o, "stderr": e}),
        )
        .await;
}

/// SIGKILL the process and drop the relay. Idempotent; used by every death path.
pub fn teardown(j: &mut Job) {
    if j.exit_code.is_none() {
        j.status = "stopping".to_string();
        // Best effort: an already-reaped pid is not an error.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(j.pid as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    j.ctrl.abort();
}
