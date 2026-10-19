//! The Windows side of the OS layer (#48 §11.3): the logs dir, the folder as a
//! kernel's `current_dir()` names it, the environment, spawning a canvas in a
//! hidden console of its own, the `__ctrl_break` stop helper, and the host's stop
//! signals.
//!
//! WHY A HELPER. The app is a GUI process with no console. A console program it
//! spawns with only `CREATE_NEW_PROCESS_GROUP` gets a new VISIBLE console
//! window, and `GenerateConsoleCtrlEvent` reaches a process group only through a
//! console the caller shares, so a host with no console could not stop a canvas.
//! Each canvas starts in a hidden console (`CREATE_NO_WINDOW`) and leads its own
//! process group; a stop runs `fantastic __ctrl_break <pid>` with no console
//! (`DETACHED_PROCESS`), which attaches to the canvas's console and sends it
//! Ctrl-Break — every fantastic's SIGTERM.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, DETACHED_PROCESS,
};

/// How long the stop helper has to finish.
const HELPER_TIMEOUT: Duration = Duration::from_secs(5);

/// Where launch logs go: `%LOCALAPPDATA%\fantastic\logs`, created.
pub fn logs_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("LOCALAPPDATA")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .ok_or("no folder for launch logs: LOCALAPPDATA is not set")?;
    let dir = base.join("fantastic").join("logs");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

/// `path` as a kernel running there names its folder (`current_dir()`):
/// absolute, no `\\?\` prefix, no trailing separator, a directory, UTF-8.
pub fn folder(path: &str) -> Result<String, String> {
    let abs = std::path::absolute(path).map_err(|e| format!("{path}: {e}"))?;
    if !abs.is_dir() {
        return Err(format!("{} is not a directory", abs.display()));
    }
    let text = abs
        .into_os_string()
        .into_string()
        .map_err(|p| format!("{} is not UTF-8", PathBuf::from(p).display()))?;
    let trimmed = text.trim_end_matches(['\\', '/']);
    // A drive root keeps its separator: `C:` alone names the drive's current dir.
    if trimmed.len() == 2 && trimmed.ends_with(':') {
        return Ok(format!("{trimmed}\\"));
    }
    Ok(trimmed.to_string())
}

/// This process's own environment: Windows has no login shell (a session is
/// already the user's, as the exec bridge's empty `LOGIN_ARGS` says).
pub async fn login_env() -> Result<Vec<(OsString, OsString)>, String> {
    Ok(std::env::vars_os().collect())
}

/// Start `<bin> canvas` in `path` with exactly `env` plus `FANTASTIC_HOME=<home>`,
/// stdin and stdout null, stderr to `log`, leading its own process group in a
/// hidden console of its own: no window shows, and the host's own Ctrl-C and
/// Ctrl-Break never reach it.
pub async fn spawn(
    bin: &Path,
    path: &str,
    env: &[(OsString, OsString)],
    home: &Path,
    log: &Path,
) -> Result<(), String> {
    let err = std::fs::File::create(log).map_err(|e| format!("{}: {e}", log.display()))?;
    let mut child = tokio::process::Command::new(bin)
        .arg("canvas")
        .current_dir(path)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .env("FANTASTIC_HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(err)
        .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("{}: {e}", bin.display()))?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

/// Ask `pid` to stop: `<bin> __ctrl_break <pid>`, run with no console, 5 s.
/// Exit 0 is `Ok(true)`; exit 1 is its stderr; anything else names the exit.
pub async fn terminate_pid(bin: &Path, pid: u32) -> Result<bool, String> {
    let child = tokio::process::Command::new(bin)
        .args(["__ctrl_break", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .creation_flags(DETACHED_PROCESS)
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("the stop helper {}: {e}", bin.display()))?;
    let out = match tokio::time::timeout(HELPER_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("the stop helper: {e}")),
        // dropping the child kills it (`kill_on_drop`)
        Err(_) => return Err("the stop helper did not finish in 5 s".to_string()),
    };
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Err(stderr),
        Some(code) => Err(format!("the stop helper exited {code}: {stderr}")),
        None => Err(format!("the stop helper exited with no code: {stderr}")),
    }
}

/// The host is asked to stop: the first Ctrl-Break or Ctrl-C.
pub async fn stopped() -> Result<(), String> {
    let mut brk = tokio::signal::windows::ctrl_break().map_err(|e| format!("Ctrl-Break: {e}"))?;
    let mut c = tokio::signal::windows::ctrl_c().map_err(|e| format!("Ctrl-C: {e}"))?;
    tokio::select! {
        _ = brk.recv() => {}
        _ = c.recv() => {}
    }
    Ok(())
}
