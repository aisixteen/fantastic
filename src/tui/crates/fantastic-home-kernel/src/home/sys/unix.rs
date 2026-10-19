//! The Unix side of the OS layer: the logs dir, the canonical folder, the login
//! shell's environment, spawning a canvas in its own session, SIGTERM, and the
//! host's stop signals.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// What a login shell prints before its environment, so the profile's own
/// output is skipped.
const MARKER: &str = "__FANTASTIC_LOGIN_ENV__";
/// How long the login shell has to print its environment.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5);

fn home_var() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| "no folder for launch logs: HOME is not set".to_string())
}

/// Where launch logs go, created `0700`: `~/Library/Logs/Fantastic` on macOS;
/// elsewhere `$XDG_STATE_HOME/fantastic/logs` when that is set and absolute,
/// else `~/.local/state/fantastic/logs`.
pub fn logs_dir() -> Result<PathBuf, String> {
    let dir = logs_dir_path()?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

#[cfg(target_os = "macos")]
fn logs_dir_path() -> Result<PathBuf, String> {
    Ok(home_var()?.join("Library").join("Logs").join("Fantastic"))
}

#[cfg(not(target_os = "macos"))]
fn logs_dir_path() -> Result<PathBuf, String> {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(state.join("fantastic").join("logs"));
    }
    Ok(home_var()?
        .join(".local")
        .join("state")
        .join("fantastic")
        .join("logs"))
}

/// `path` as a kernel running there names its folder: canonical (links
/// resolved), a directory, UTF-8.
pub fn folder(path: &str) -> Result<String, String> {
    let real = std::fs::canonicalize(path).map_err(|e| format!("{path}: {e}"))?;
    if !real.is_dir() {
        return Err(format!("{} is not a directory", real.display()));
    }
    real.into_os_string()
        .into_string()
        .map_err(|p| format!("{} is not UTF-8", PathBuf::from(p).display()))
}

/// The login shell's environment over this process's own, the shell's values
/// winning: `$SHELL -ilc "printf '%s' MARKER; /usr/bin/env -0"`, its stdout a
/// private file (a daemon the profile starts would hold a pipe open), 5 s.
/// `SHELL` unset, a timeout or no `PATH` is an `Err` naming which.
pub async fn login_env() -> Result<Vec<(OsString, OsString)>, String> {
    let shell = std::env::var_os("SHELL")
        .filter(|s| !s.is_empty())
        .ok_or("no login environment: SHELL is not set")?;
    let file = logs_dir()?.join(format!("login-env-{}.txt", super::pid()));
    let out = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&file)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    let printed = run_login_shell(&shell, out).await;
    let bytes = std::fs::read(&file).map_err(|e| format!("{}: {e}", file.display()));
    let _ = std::fs::remove_file(&file);
    printed?;
    let login = parse_login_env(&bytes?)?;
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| !login.iter().any(|(l, _)| l == k))
        .collect();
    env.extend(login);
    Ok(env)
}

async fn run_login_shell(shell: &std::ffi::OsStr, out: std::fs::File) -> Result<(), String> {
    let script = format!("printf '%s' {MARKER}; /usr/bin/env -0");
    let mut child = tokio::process::Command::new(shell)
        .args(["-ilc", &script])
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            format!(
                "no login environment: {}: {e}",
                PathBuf::from(shell).display()
            )
        })?;
    match tokio::time::timeout(LOGIN_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(e)) => Err(format!("no login environment: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            Err("no login environment: the login shell did not finish in 5 s".to_string())
        }
    }
}

/// What the login shell printed, after the marker: NUL-separated `NAME=value`
/// pairs with a non-empty name. `PATH` is required.
pub fn parse_login_env(printed: &[u8]) -> Result<Vec<(OsString, OsString)>, String> {
    let marker = MARKER.as_bytes();
    let start = printed
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or("no login environment: the login shell printed none")?;
    let mut out = Vec::new();
    for pair in printed[start + marker.len()..].split(|b| *b == 0) {
        let Some(eq) = pair.iter().position(|b| *b == b'=') else {
            continue;
        };
        if eq == 0 {
            continue;
        }
        out.push((
            OsString::from_vec(pair[..eq].to_vec()),
            OsString::from_vec(pair[eq + 1..].to_vec()),
        ));
    }
    if !out.iter().any(|(k, _)| k == "PATH") {
        return Err("no login environment: the login shell's environment has no PATH".to_string());
    }
    Ok(out)
}

/// Start `<bin> canvas` in `path` with exactly `env` plus `FANTASTIC_HOME=<home>`,
/// stdin and stdout null, stderr to `log` (`0600`). The child leads its own
/// session (`setsid`), so it outlives the host and never gets its terminal's
/// signals.
pub async fn spawn(
    bin: &Path,
    path: &str,
    env: &[(OsString, OsString)],
    home: &Path,
    log: &Path,
) -> Result<(), String> {
    let err = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("canvas")
        .current_dir(path)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("FANTASTIC_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err);
    // SAFETY: `setsid` is async-signal-safe and touches no memory of the parent.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", bin.display()))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// Ask `pid` to stop (SIGTERM). `Ok(true)` asked; `Ok(false)` no such process;
/// `Err` it is not ours to stop, or the signal could not be sent.
pub async fn terminate_pid(_bin: &Path, pid: u32) -> Result<bool, String> {
    let Ok(target) = libc::pid_t::try_from(pid) else {
        return Ok(false);
    };
    // SAFETY: no pointers; it sends SIGTERM to one process.
    if unsafe { libc::kill(target, libc::SIGTERM) } == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        Some(libc::EPERM) => Err("not yours to stop".to_string()),
        _ => Err(e.to_string()),
    }
}

/// The host is asked to stop: the first SIGTERM or SIGINT.
pub async fn stopped() -> Result<(), String> {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).map_err(|e| format!("SIGTERM: {e}"))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| format!("SIGINT: {e}"))?;
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
    Ok(())
}
