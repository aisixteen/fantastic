//! SSH+WS transport — spawn `ssh -L local:localhost:remote` then
//! tunnel a [`WsTransport`] over the local port.
//!
//! Mirrors Python's `kernel_bridge._transport`'s `ssh+ws` variant:
//!
//! 1. `ssh -N -L <local>:localhost:<remote_port> -o ExitOnForwardFailure=yes
//!    -o ServerAliveInterval=15 -o ServerAliveCountMax=3 <host>` runs as
//!    a child process in its own session group. `-N` means "no remote
//!    command, just the tunnel"; `ExitOnForwardFailure=yes` fails fast
//!    on a local-port collision; the keepalives keep stateful firewalls
//!    from silently dropping the forward.
//! 2. We poll `127.0.0.1:<local_port>` with `tokio::net::TcpStream::connect`
//!    every 100ms until the socket accepts OR we hit the 5s deadline.
//!    Early-exit if the ssh child has already terminated (auth/route
//!    failure → ssh prints to stderr and dies).
//! 3. Once the local port is live, build a `ws://127.0.0.1:<local_port>/<peer_id>/ws`
//!    URL and hand off to the existing [`WsTransport`] for actual
//!    framing — the SSH layer is purely transport-of-transport.
//! 4. `close` tries SIGTERM → wait 2s → SIGKILL on the ssh child, then
//!    closes the inner WS. On Windows (no `nix` syscall surface) we
//!    fall back to `Child::kill().await`, which sends a hard kill.
//!
//! Gated by `feature = "full"` because it spawns the `ssh` binary as
//! a subprocess. Embedded builds (iOS) skip it.

use super::{BridgeTransport, TransportError};
use crate::transport::ws::WsTransport;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use tokio::process::Child;
use tokio::sync::Mutex;

/// How long [`SshTransport::open`] polls the local tunnel port before
/// giving up. Matches Python's `TUNNEL_READY_TIMEOUT`.
pub const TUNNEL_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// SSH+WS transport. Wraps a [`WsTransport`] whose URL points at a
/// local port that an `ssh -L` child forwards to a remote.
///
/// Construct via [`SshTransport::open`]; the spawn-and-wait dance lives
/// there so callers can `?` on the failure modes.
pub struct SshTransport {
    /// Inner WS transport, addressed at `ws://127.0.0.1:<local_port>/<peer_id>/ws`.
    ws_inner: Arc<WsTransport>,
    /// The `ssh -L` child. Held in a mutex so [`SshTransport::close`]
    /// can drive it through SIGTERM → SIGKILL without contending with
    /// any read path (we never touch it from `send_frame`/`recv_frame`).
    tunnel_child: Mutex<Option<Child>>,
    /// Cached pid of the ssh child for `reflect`-style introspection.
    tunnel_pid: Option<u32>,
    /// Loopback port the tunnel binds.
    local_port: u16,
    /// SSH alias / host as passed to `ssh <host>`.
    host: String,
    /// Remote bridge agent's id — appears in the WS URL path.
    peer_id: String,
}

impl SshTransport {
    /// Spawn `ssh -L local:localhost:remote -N <host>`, wait for the
    /// local port to accept, then connect [`WsTransport`] over it.
    ///
    /// `local_port = 0` asks the OS for an ephemeral free loopback
    /// port (via a momentary bind on `127.0.0.1:0`). Any non-zero
    /// value is used verbatim — callers who care about port stability
    /// (record-driven config) pass a fixed value.
    pub async fn open(
        host: &str,
        peer_id: &str,
        remote_port: u16,
        local_port: u16,
    ) -> Result<Arc<Self>, TransportError> {
        // ssh binary on PATH? Fail fast with a precise error otherwise.
        if which::which("ssh").is_err() {
            return Err(TransportError::Other("ssh binary not found on PATH".into()));
        }

        let port = if local_port == 0 {
            pick_free_loopback_port().map_err(|e| {
                TransportError::Other(format!("ssh+ws: could not pick free port: {e}"))
            })?
        } else {
            local_port
        };

        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args([
            "-N",
            "-L",
            &format!("{port}:localhost:{remote_port}"),
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
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false);

        // Run in its own process group so SIGTERM-on-tunnel kills the
        // ssh subtree without touching the kernel's process group.
        #[cfg(unix)]
        {
            // SAFETY: `setsid` is async-signal-safe and called pre-exec
            // before any I/O has occurred in the forked child.
            unsafe {
                cmd.pre_exec(|| {
                    // setsid creates a new session + process group.
                    // Ignore EPERM (already a session leader — shouldn't
                    // happen post-fork but harmless if it does).
                    let _ = nix::unistd::setsid();
                    Ok(())
                });
            }
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| TransportError::Other(format!("ssh+ws: spawn ssh: {e}")))?;
        let pid = child.id();

        // Poll the local port until it accepts or the deadline trips.
        let deadline = tokio::time::Instant::now() + TUNNEL_READY_TIMEOUT;
        let mut ready = false;
        while tokio::time::Instant::now() < deadline {
            // Early exit: ssh died (auth failure / unreachable host / port collision).
            if let Ok(Some(status)) = child.try_wait() {
                return Err(TransportError::Other(format!(
                    "ssh+ws: ssh tunnel exited early (status {status})"
                )));
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                tokio::net::TcpStream::connect(("127.0.0.1", port)),
            )
            .await
            {
                Ok(Ok(_stream)) => {
                    ready = true;
                    break;
                }
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        if !ready {
            // Tear down the ssh child cleanly before surfacing the error.
            kill_child(&mut child).await;
            return Err(TransportError::Other(format!(
                "ssh+ws: tunnel to {host}:{remote_port} not ready in {:?}",
                TUNNEL_READY_TIMEOUT
            )));
        }

        // Hand off to WsTransport. If the WS layer fails, kill the
        // tunnel before bubbling up so we don't leak the child.
        let ws_url = format!("ws://127.0.0.1:{port}/{peer_id}/ws");
        let ws_inner = match WsTransport::connect(&ws_url).await {
            Ok(ws) => ws,
            Err(e) => {
                kill_child(&mut child).await;
                return Err(e);
            }
        };

        Ok(Arc::new(Self {
            ws_inner,
            tunnel_child: Mutex::new(Some(child)),
            tunnel_pid: pid,
            local_port: port,
            host: host.to_string(),
            peer_id: peer_id.to_string(),
        }))
    }

    /// Pid of the running ssh tunnel child, if any. Surfaced via
    /// reflect so operators / telemetry can see the live tunnel.
    pub fn tunnel_pid(&self) -> Option<u32> {
        self.tunnel_pid
    }

    /// Local loopback port the tunnel binds.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// SSH host alias as passed to `ssh <host>` at construction.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Remote bridge agent's id — appears in the WS URL path.
    pub fn peer_id(&self) -> &str {
        &self.peer_id
    }
}

#[async_trait]
impl BridgeTransport for SshTransport {
    async fn send_frame(&self, frame: Value) -> Result<(), TransportError> {
        self.ws_inner.send_frame(frame).await
    }

    async fn send_binary(&self, header: Value, body: Vec<u8>) -> Result<(), TransportError> {
        self.ws_inner.send_binary(header, body).await
    }

    async fn recv_frame(&self) -> Result<super::Frame, TransportError> {
        self.ws_inner.recv_frame().await
    }

    async fn close(&self) {
        // Close the WS layer first so any pending recv resolves with a
        // ConnectionClosed before we yank the network out from under it.
        self.ws_inner.close().await;
        // Then tear down the ssh child. SIGTERM → 2s grace → SIGKILL.
        let mut slot = self.tunnel_child.lock().await;
        if let Some(mut child) = slot.take() {
            kill_child(&mut child).await;
        }
    }
}

/// Best-effort graceful kill of a `tokio::process::Child`. On Unix:
/// SIGTERM the whole process group (the ssh client may have spawned a
/// helper for auth), wait up to 2s, escalate to SIGKILL on timeout.
/// On non-Unix: fall back to `Child::kill().await`.
async fn kill_child(child: &mut Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        use nix::sys::signal::{killpg, Signal};
        use nix::unistd::Pid;
        if let Some(p) = pid {
            // Send SIGTERM to the whole process group (we started a
            // new session via setsid, so pgid == pid).
            let _ = killpg(Pid::from_raw(p as i32), Signal::SIGTERM);
        }
        // Race the child against a 2s timer; escalate to SIGKILL if it
        // didn't oblige.
        if tokio::time::timeout(std::time::Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
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

/// Bind `127.0.0.1:0` briefly to learn a free loopback port, then drop
/// the listener so the ssh child can claim the port immediately. There
/// is a small TOCTOU window between drop and ssh-bind; in practice ssh
/// retries with `ExitOnForwardFailure=yes` are caught by the early-exit
/// detection in the readiness poll.
fn pick_free_loopback_port() -> std::io::Result<u16> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    Ok(listener.local_addr()?.port())
}
