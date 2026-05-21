//! Remote `fantastic --port N` lifecycle over SSH.
//!
//! Each agent represents one project on one remote host. Verbs exec
//! `ssh` as a subprocess to control the remote kernel and maintain a
//! local SSH tunnel so the browser / canvas iframe can reach the
//! remote webapp at `http://localhost:<local_port>/`.
//!
//! Pure subprocess SSH (no `paramiko` / `russh`). Authentication is
//! whatever `ssh <host>` works as in the user's shell — keys,
//! `ssh-agent`, and `~/.ssh/config` all apply transparently.
//!
//! ## Record fields
//!
//! | key           | purpose                                                  |
//! |---------------|----------------------------------------------------------|
//! | `host`        | ssh alias / hostname (passed to `ssh <host>`)            |
//! | `remote_path` | project root on the remote box                           |
//! | `remote_cmd`  | absolute path to the remote `fantastic` CLI              |
//! | `remote_port` | port the remote daemon binds (REQUIRED, no default)      |
//! | `local_port`  | local port the SSH tunnel forwards from                  |
//! | `entry_path`  | URL suffix appended to local tunnel for `get_webapp`     |
//!
//! ## Verbs
//!
//! `reflect`, `boot`, `start`, `stop`, `restart`, `status`, `get_webapp`.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle as _;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "ssh_runner.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// How long to wait for the remote `.fantastic/lock.json` to appear
/// after `start`. Matches Python's `REMOTE_LOCK_POLL_TIMEOUT`.
pub const REMOTE_LOCK_POLL_TIMEOUT_SECS: f64 = 30.0;
/// Polling cadence while waiting for `lock.json`. Matches Python's
/// `REMOTE_LOCK_POLL_INTERVAL`.
pub const REMOTE_LOCK_POLL_INTERVAL_MS: u64 = 500;
/// How long [`open_tunnel`] polls the local tunnel port before giving
/// up. Matches Python's `TUNNEL_READY_TIMEOUT`.
pub const TUNNEL_READY_TIMEOUT_SECS: f64 = 5.0;
/// Default ssh subprocess timeout for one-shot remote commands.
pub const SSH_EXEC_TIMEOUT_SECS: f64 = 15.0;

// ── per-agent state ─────────────────────────────────────────────────

static RUNNERS: OnceLockRunnerMap = OnceLockRunnerMap::new();

/// Per-agent runner state — process-memory only.
///
/// The `tunnel_proc` slot holds the live `ssh -L` child (the local
/// port-forward used by `get_webapp` so the canvas can iframe the
/// remote). Fields are intentionally `pub(crate)` — outside callers
/// drive everything through the bundle's verbs.
pub struct RunnerState {
    /// The live `ssh -L` child process (None when no tunnel is open).
    pub(crate) tunnel_proc: tokio::sync::Mutex<Option<tokio::process::Child>>,
    /// Cached pid of the tunnel child for [`reflect_reply`] /
    /// `status` introspection.
    pub(crate) tunnel_pid: tokio::sync::Mutex<Option<u32>>,
}

impl RunnerState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tunnel_proc: tokio::sync::Mutex::new(None),
            tunnel_pid: tokio::sync::Mutex::new(None),
        })
    }
}

struct OnceLockRunnerMap(OnceLock<Mutex<HashMap<AgentId, Arc<RunnerState>>>>);
impl OnceLockRunnerMap {
    const fn new() -> Self {
        Self(OnceLock::new())
    }
    fn get_or_init_for(&self, id: &AgentId) -> Arc<RunnerState> {
        let mut map = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("RUNNERS outer mutex poisoned");
        if let Some(existing) = map.get(id) {
            return Arc::clone(existing);
        }
        let arc = RunnerState::new();
        map.insert(id.clone(), Arc::clone(&arc));
        arc
    }
    fn remove(&self, id: &AgentId) {
        let mut map = self
            .0
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .expect("RUNNERS outer mutex poisoned");
        map.remove(id);
    }
}

// ── bundle impl ─────────────────────────────────────────────────────

/// The ssh_runner bundle.
pub struct SshRunnerBundle;

#[async_trait]
impl Bundle for SshRunnerBundle {
    fn name(&self) -> &str {
        "ssh_runner"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, kernel).await,
            "boot" => Value::Null,
            "start" => start_reply(agent_id, kernel).await,
            "stop" | "shutdown" => stop_reply(agent_id, kernel).await,
            "restart" => {
                let _ = stop_reply(agent_id, kernel).await;
                start_reply(agent_id, kernel).await
            }
            "status" => status_reply(agent_id, kernel).await,
            "get_webapp" => get_webapp_reply(agent_id, kernel),
            other => json!({"error": format!("ssh_runner: unknown type {other:?}")}),
        };
        Ok(Some(reply))
    }

    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let _ = stop_reply(agent_id, kernel).await;
        RUNNERS.remove(agent_id);
        Ok(())
    }
}

// ── meta helpers ────────────────────────────────────────────────────

fn snapshot_meta(agent_id: &AgentId, kernel: &Kernel) -> Map<String, Value> {
    match kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) {
        Some(a) => a.meta.read().expect("meta poisoned").clone(),
        None => Map::new(),
    }
}

fn meta_str<'a>(meta: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    meta.get(key).and_then(Value::as_str)
}

fn meta_u16(meta: &Map<String, Value>, key: &str) -> Option<u16> {
    meta.get(key)
        .and_then(Value::as_u64)
        .filter(|p| *p > 0 && *p <= u16::MAX as u64)
        .map(|p| p as u16)
}

// ── ssh helpers ─────────────────────────────────────────────────────

/// Run `ssh -o BatchMode=yes <host> '<cmd>'` non-interactively.
/// Returns `(exit_code, stdout, stderr)`. On timeout, kills the
/// process and returns `exit_code = -1` with `stderr = "ssh timeout"`.
pub async fn ssh_exec(host: &str, cmd: &str, timeout: Duration) -> (i32, String, String) {
    let child = match tokio::process::Command::new("ssh")
        .args(["-o", "BatchMode=yes", host, cmd])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return (-1, String::new(), format!("ssh spawn: {e}")),
    };
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(out)) => (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        ),
        Ok(Err(e)) => (-1, String::new(), format!("ssh wait: {e}")),
        Err(_) => {
            // We've moved `child` into wait_with_output's future; the
            // future was dropped (or not — wait_with_output consumes
            // child). On Err(_elapsed), the future was cancelled but
            // we cannot reach it. Just return timeout.
            (-1, String::new(), "ssh timeout".to_string())
        }
    }
}

/// Spawn `ssh -L <local>:localhost:<remote> -N <host>` in a fresh
/// session group; poll the local port until it accepts or the
/// 5s deadline trips. Matches the Python bundle's `_open_tunnel`.
pub async fn open_tunnel(
    host: &str,
    local_port: u16,
    remote_port: u16,
) -> Result<tokio::process::Child, String> {
    if which::which("ssh").is_err() {
        return Err("ssh binary not found on PATH".into());
    }

    let mut cmd = tokio::process::Command::new("ssh");
    cmd.args([
        "-N",
        "-L",
        &format!("{local_port}:localhost:{remote_port}"),
        "-o",
        "ExitOnForwardFailure=yes",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "BatchMode=yes",
        host,
    ])
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .kill_on_drop(false);

    // Fresh session group so SIGTERM-on-tunnel kills the ssh subtree
    // without touching the parent's process group.
    #[cfg(unix)]
    {
        // SAFETY: setsid is async-signal-safe and called pre-exec
        // before any I/O occurs in the forked child.
        unsafe {
            cmd.pre_exec(|| {
                let _ = nix::unistd::setsid();
                Ok(())
            });
        }
    }

    let mut child = cmd.spawn().map_err(|e| format!("ssh tunnel spawn: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(TUNNEL_READY_TIMEOUT_SECS);
    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!("ssh tunnel exited early (status {status})"));
        }
        match tokio::time::timeout(
            Duration::from_millis(200),
            tokio::net::TcpStream::connect(("127.0.0.1", local_port)),
        )
        .await
        {
            Ok(Ok(_stream)) => return Ok(child),
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    kill_tunnel(&mut child).await;
    Err(format!(
        "ssh tunnel to {host}:{remote_port} not ready in {}s",
        TUNNEL_READY_TIMEOUT_SECS
    ))
}

/// SIGTERM the tunnel's process group, wait 2s, escalate to SIGKILL
/// if it didn't die. No-op on non-Unix (falls back to Child::kill).
pub async fn kill_tunnel(child: &mut tokio::process::Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        use nix::sys::signal::{killpg, Signal};
        use nix::unistd::Pid;
        if let Some(p) = pid {
            let _ = killpg(Pid::from_raw(p as i32), Signal::SIGTERM);
        }
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            // SIGTERM didn't take — escalate.
            if let Some(p) = pid {
                let _ = killpg(Pid::from_raw(p as i32), Signal::SIGKILL);
            }
            let _ = child.wait().await;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}

/// shell-quote a string for single-quoted POSIX shell context.
/// Matches Python's `shlex.quote` for the strings we feed to remote
/// commands (paths, binary names).
fn shquote(s: &str) -> String {
    if s.is_empty() {
        return "''".into();
    }
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'))
    {
        return s.to_string();
    }
    let escaped = s.replace('\'', "'\\''");
    format!("'{escaped}'")
}

// ── verb implementations ────────────────────────────────────────────

async fn reflect_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let runner = RUNNERS.get_or_init_for(agent_id);
    let (tunnel_alive, tunnel_pid) = {
        let mut slot = runner.tunnel_proc.lock().await;
        let alive = match slot.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        };
        let pid = *runner.tunnel_pid.lock().await;
        (alive, pid)
    };
    json!({
        "id": agent_id.as_str(),
        "sentence": "Remote `fantastic --port N` lifecycle over SSH.",
        "host": meta.get("host").cloned().unwrap_or(Value::Null),
        "remote_path": meta.get("remote_path").cloned().unwrap_or(Value::Null),
        "remote_cmd": meta.get("remote_cmd").cloned().unwrap_or(Value::Null),
        "remote_port": meta.get("remote_port").cloned().unwrap_or(Value::Null),
        "local_port": meta.get("local_port").cloned().unwrap_or(Value::Null),
        "entry_path": meta_str(&meta, "entry_path").unwrap_or(""),
        "tunnel_pid": tunnel_pid,
        "tunnel_alive": tunnel_alive,
        "running": tunnel_alive,
        "verbs": {
            "reflect": "Identity + every record field + live status. No args.",
            "boot": "No-op. ssh_runner does NOT auto-start the remote — `start` is explicit.",
            "start": "No args. SSHs to <host>, runs `cd <remote_path> && nohup <remote_cmd> ...` then opens the local SSH tunnel.",
            "stop": "No args. Kills the local SSH tunnel, then SSHs and SIGTERMs the remote pid recorded in `.fantastic/lock.json`. Idempotent.",
            "restart": "No args. stop + start.",
            "status": "No args. {tunnel_alive, remote_alive, remote_pid}.",
            "get_webapp": "No args. Canvas-facing UI descriptor {url, default_width, default_height, title}.",
        },
    })
}

async fn start_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let Some(host) = meta_str(&meta, "host") else {
        return json!({"error": "ssh_runner.start: host, remote_path, remote_cmd, remote_port, local_port all required"});
    };
    let Some(rp) = meta_str(&meta, "remote_path") else {
        return json!({"error": "ssh_runner.start: host, remote_path, remote_cmd, remote_port, local_port all required"});
    };
    let Some(rcmd) = meta_str(&meta, "remote_cmd") else {
        return json!({"error": "ssh_runner.start: host, remote_path, remote_cmd, remote_port, local_port all required"});
    };
    let Some(rport) = meta_u16(&meta, "remote_port") else {
        return json!({"error": "ssh_runner.start: host, remote_path, remote_cmd, remote_port, local_port all required"});
    };
    let Some(lport) = meta_u16(&meta, "local_port") else {
        return json!({"error": "ssh_runner.start: host, remote_path, remote_cmd, remote_port, local_port all required"});
    };

    // Two-step bootstrap on the remote:
    //   1. one-shot `fantastic core create_agent handler_module=web.tools port=N`
    //      persists the web record (uvicorn task dies with the process,
    //      but the record stays on disk).
    //   2. nohup `fantastic` spawns the daemon — `_default` rehydrates
    //      the persisted web, acquires lock, blocks forever.
    let rp_q = shquote(rp);
    let cmd_q = shquote(rcmd);
    let remote = format!(
        "cd {rp_q} && mkdir -p .fantastic && {cmd_q} core create_agent handler_module=web.tools port={rport} >/dev/null 2>&1 && nohup {cmd_q} > .fantastic/serve.log 2>&1 &"
    );
    let (rc, out, err) = ssh_exec(
        host,
        &remote,
        Duration::from_secs_f64(SSH_EXEC_TIMEOUT_SECS),
    )
    .await;
    if rc != 0 {
        let detail = if err.trim().is_empty() {
            out.trim().to_string()
        } else {
            err.trim().to_string()
        };
        return json!({"error": format!("ssh_runner.start: ssh failed (rc={rc}): {detail}")});
    }

    // Poll the remote lock.json to confirm the serve actually came up.
    let lock_path = format!("{rp_q}/.fantastic/lock.json");
    let deadline = Instant::now() + Duration::from_secs_f64(REMOTE_LOCK_POLL_TIMEOUT_SECS);
    let mut remote_pid: Option<i64> = None;
    while Instant::now() < deadline {
        let (rc2, out2, _) = ssh_exec(
            host,
            &format!("cat {lock_path} 2>/dev/null"),
            Duration::from_secs(5),
        )
        .await;
        if rc2 == 0 && !out2.trim().is_empty() {
            if let Ok(lock) = serde_json::from_str::<Value>(&out2) {
                if let Some(p) = lock.get("pid").and_then(Value::as_i64) {
                    remote_pid = Some(p);
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(REMOTE_LOCK_POLL_INTERVAL_MS)).await;
    }
    let Some(remote_pid) = remote_pid else {
        return json!({"error": "ssh_runner.start: remote serve did not write lock.json in time"});
    };

    // Open local tunnel.
    let runner = RUNNERS.get_or_init_for(agent_id);
    {
        let mut slot = runner.tunnel_proc.lock().await;
        if let Some(c) = slot.as_mut() {
            if matches!(c.try_wait(), Ok(None)) {
                let pid = *runner.tunnel_pid.lock().await;
                return json!({
                    "started": true,
                    "remote_pid": remote_pid,
                    "tunnel_pid": pid,
                    "already_tunneled": true,
                });
            }
        }
    }
    let tunnel = match open_tunnel(host, lport, rport).await {
        Ok(c) => c,
        Err(e) => {
            return json!({
                "error": format!("ssh_runner.start: tunnel failed: {e}"),
                "remote_pid": remote_pid,
            })
        }
    };
    let pid = tunnel.id();
    {
        let mut slot = runner.tunnel_proc.lock().await;
        *slot = Some(tunnel);
        let mut pidslot = runner.tunnel_pid.lock().await;
        *pidslot = pid;
    }
    json!({
        "started": true,
        "remote_pid": remote_pid,
        "tunnel_pid": pid,
    })
}

async fn stop_reply(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let host = meta_str(&meta, "host");
    let rp = meta_str(&meta, "remote_path");
    if host.is_none() || rp.is_none() {
        return json!({"error": "ssh_runner.stop: host + remote_path required"});
    }
    let host = host.unwrap();
    let rp = rp.unwrap();

    // Kill the local tunnel (best effort, idempotent).
    let runner = RUNNERS.get_or_init_for(agent_id);
    {
        let mut slot = runner.tunnel_proc.lock().await;
        if let Some(mut child) = slot.take() {
            kill_tunnel(&mut child).await;
        }
        let mut pidslot = runner.tunnel_pid.lock().await;
        *pidslot = None;
    }

    // Read remote pid + kill it.
    let rp_q = shquote(rp);
    let (rc, out, _) = ssh_exec(
        host,
        &format!("cat {rp_q}/.fantastic/lock.json 2>/dev/null"),
        Duration::from_secs(5),
    )
    .await;
    let mut remote_pid: Option<i64> = None;
    if rc == 0 && !out.trim().is_empty() {
        if let Ok(lock) = serde_json::from_str::<Value>(&out) {
            remote_pid = lock.get("pid").and_then(Value::as_i64);
        }
    }
    if let Some(pid) = remote_pid {
        let _ = ssh_exec(
            host,
            &format!("kill {pid} 2>/dev/null || true"),
            Duration::from_secs(5),
        )
        .await;
    }
    json!({"stopped": true, "remote_pid": remote_pid})
}

async fn status_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let runner = RUNNERS.get_or_init_for(agent_id);
    let tunnel_alive = {
        let mut slot = runner.tunnel_proc.lock().await;
        match slot.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    };

    let mut remote_alive = false;
    let mut remote_pid: Option<i64> = None;
    if let (Some(host), Some(rp)) = (meta_str(&meta, "host"), meta_str(&meta, "remote_path")) {
        let rp_q = shquote(rp);
        let (rc, out, _) = ssh_exec(
            host,
            &format!("cat {rp_q}/.fantastic/lock.json 2>/dev/null"),
            Duration::from_secs(5),
        )
        .await;
        if rc == 0 && !out.trim().is_empty() {
            if let Ok(lock) = serde_json::from_str::<Value>(&out) {
                if let Some(p) = lock.get("pid").and_then(Value::as_i64) {
                    remote_pid = Some(p);
                    let (rc2, _, _) = ssh_exec(
                        host,
                        &format!("kill -0 {p} 2>/dev/null && echo ok"),
                        Duration::from_secs(5),
                    )
                    .await;
                    remote_alive = rc2 == 0;
                }
            }
        }
    }
    json!({
        "tunnel_alive": tunnel_alive,
        "remote_alive": remote_alive,
        "remote_pid": remote_pid,
    })
}

fn get_webapp_reply(agent_id: &AgentId, kernel: &Kernel) -> Value {
    let meta = snapshot_meta(agent_id, kernel);
    let Some(lport) = meta_u16(&meta, "local_port") else {
        return json!({"error": "ssh_runner.get_webapp: local_port required"});
    };
    let entry = meta_str(&meta, "entry_path").unwrap_or("");
    let host = meta_str(&meta, "host").unwrap_or("remote");
    let width = meta
        .get("width")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0 && *v <= u32::MAX as u64)
        .unwrap_or(800);
    let height = meta
        .get("height")
        .and_then(Value::as_u64)
        .filter(|v| *v > 0 && *v <= u32::MAX as u64)
        .unwrap_or(600);
    let title = meta_str(&meta, "display_name")
        .map(str::to_string)
        .unwrap_or_else(|| host.to_string());
    let _ = agent_id;
    json!({
        "url": format!("http://localhost:{lport}/{entry}"),
        "default_width": width,
        "default_height": height,
        "title": title,
    })
}

#[cfg(test)]
mod tests;
