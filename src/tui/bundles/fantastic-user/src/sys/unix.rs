//! The Unix side of [`crate::sys`]: the login shell and file modes.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

/// How long the login shell may take to print its PATH.
const SHELL_WAIT: Duration = Duration::from_secs(5);

/// The login shell's PATH (`$SHELL -l -c`), if it answers in time. No `$SHELL`:
/// none.
pub(crate) async fn login_path() -> Option<String> {
    let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty())?;
    let child = tokio::process::Command::new(shell)
        .args(["-l", "-c", "printf %s \"$PATH\""])
        .env("TERM", "dumb")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let out = tokio::time::timeout(SHELL_WAIT, child).await.ok()?.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The name a shell runs the file named `file` by, and its rank among the files
/// of one directory that share that name (lower wins): a file is run by its own
/// name, so there is only ever one.
pub(crate) fn program_name(file: &str) -> Option<(String, usize)> {
    Some((file.to_string(), 0))
}

/// Whether the file at `p` (links followed) is a program: a regular file
/// carrying any execute bit.
pub(crate) fn is_program(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// TEST ONLY: a file `stem` in `dir` that is a program (`0o755`) or not
/// (`0o644`), listed as `stem`. Returns its path.
#[cfg(test)]
pub(crate) fn program_fixture(dir: &Path, stem: &str, runs: bool) -> std::path::PathBuf {
    let f = dir.join(stem);
    std::fs::write(&f, "x").unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(if runs { 0o755 } else { 0o644 })).unwrap();
    f
}
