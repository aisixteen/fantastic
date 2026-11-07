//! LINUX AUDIO: PipeWire through its own tools. `pw-dump` reads every object as
//! JSON, `pw-dump --monitor` streams the ones that change, `wpctl` sets volume,
//! mute and the default. The JSON goes to the ViewModel as it is; nothing here
//! reads it beyond splitting the stream into whole values.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use fantastic_system::audio::{Call, Event};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::sys::{AudioOs, OsError};

/// How long one `pw-dump` or `wpctl` may take.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How often the monitor looks up from its read to see if anyone still listens.
const POLL: Duration = Duration::from_secs(1);
/// How long the monitor waits before starting `pw-dump --monitor` again.
const RESTART: Duration = Duration::from_secs(1);

pub struct LinuxAudio {
    pw_dump: PathBuf,
    wpctl: PathBuf,
}

impl LinuxAudio {
    /// `pw-dump` and `wpctl` from `$PATH`.
    pub fn new() -> Self {
        Self::with_programs("pw-dump".into(), "wpctl".into())
    }

    /// Given programs (tests use fakes).
    pub fn with_programs(pw_dump: PathBuf, wpctl: PathBuf) -> Self {
        LinuxAudio { pw_dump, wpctl }
    }

    fn wpctl(&self) -> Result<PathBuf, OsError> {
        find_program(&self.wpctl)
            .ok_or_else(|| OsError::Unavailable("wpctl is not installed".into()))
    }

    fn pw_dump(&self) -> Result<PathBuf, OsError> {
        find_program(&self.pw_dump)
            .ok_or_else(|| OsError::Unavailable("pw-dump is not installed".into()))
    }
}

impl Default for LinuxAudio {
    fn default() -> Self {
        Self::new()
    }
}

/// A program as a path: one with a `/` must be a file; a bare name is looked up in `$PATH`.
pub(crate) fn find_program(p: &Path) -> Option<PathBuf> {
    if p.as_os_str().to_string_lossy().contains('/') {
        return p.is_file().then(|| p.to_path_buf());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(p))
        .find(|c| c.is_file())
}

/// Run a program to its end within [`TIMEOUT`]: its stdout, or its stderr as the error.
async fn run(program: &Path, args: &[String]) -> Result<Vec<u8>, OsError> {
    let name = program
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| OsError::Failed(format!("{name}: {e}")))?;
    let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| OsError::Failed(format!("{name} took longer than 5 s")))?
        .map_err(|e| OsError::Failed(format!("{name}: {e}")))?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(OsError::Failed(if err.is_empty() {
        format!("{name} failed ({})", out.status)
    } else {
        err
    }))
}

/// `wpctl`'s argv for a call.
fn wpctl_args(c: Call) -> Vec<String> {
    match c {
        Call::SetVolume { id, volume } => vec![
            "set-volume".into(),
            id.to_string(),
            format!("{volume:.2}"),
            "-l".into(),
            "1.5".into(),
        ],
        Call::SetMute { id, muted } => vec![
            "set-mute".into(),
            id.to_string(),
            if muted { "1" } else { "0" }.into(),
        ],
        Call::SetDefault { id } => vec!["set-default".into(), id.to_string()],
    }
}

/// Take every whole JSON value off the front of `buf`; a value cut short stays.
/// Bytes that are not JSON at all are dropped with the rest of the buffer.
pub(crate) fn drain_json(buf: &mut Vec<u8>) -> Vec<Value> {
    let mut out = Vec::new();
    let used = {
        let mut it = serde_json::Deserializer::from_slice(buf).into_iter::<Value>();
        let mut used = 0;
        loop {
            match it.next() {
                Some(Ok(v)) => {
                    out.push(v);
                    used = it.byte_offset();
                }
                Some(Err(e)) if e.is_eof() => break,
                Some(Err(_)) => {
                    used = buf.len();
                    break;
                }
                None => {
                    used = it.byte_offset();
                    break;
                }
            }
        }
        used
    };
    buf.drain(..used);
    let blank = buf.iter().take_while(|b| b.is_ascii_whitespace()).count();
    buf.drain(..blank);
    out
}

/// Starts `program --monitor`. A program another process still holds open for
/// writing (ETXTBSY, which a fork racing a fresh install can cause) is tried
/// again for a moment; any other error gives up.
async fn spawn_monitor(path: &Path) -> Option<tokio::process::Child> {
    for _ in 0..50 {
        match Command::new(path)
            .arg("--monitor")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => return Some(child),
            Err(e) if e.raw_os_error() == Some(26) => {
                tokio::time::sleep(Duration::from_millis(100)).await
            }
            Err(_) => return None,
        }
    }
    None
}

/// `pw-dump --monitor`, each array a [`Event::Delta`], until the receiver is
/// dropped; a child that ends is started again after [`RESTART`]. A missing
/// program ends it.
async fn monitor(program: PathBuf, tx: mpsc::Sender<Event>) {
    while !tx.is_closed() {
        let Some(path) = find_program(&program) else {
            return;
        };
        let Some(mut child) = spawn_monitor(&path).await else {
            return;
        };
        let Some(mut out) = child.stdout.take() else {
            return;
        };
        let (mut buf, mut chunk) = (Vec::new(), vec![0u8; 64 * 1024]);
        loop {
            if tx.is_closed() {
                return;
            }
            match tokio::time::timeout(POLL, out.read(&mut chunk)).await {
                Err(_) => continue,
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    buf.extend_from_slice(&chunk[..n]);
                    for v in drain_json(&mut buf) {
                        if tx.send(Event::Delta(v)).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
        let _ = child.kill().await;
        tokio::time::sleep(RESTART).await;
    }
}

#[async_trait::async_trait]
impl AudioOs for LinuxAudio {
    async fn read(&self) -> Result<Value, OsError> {
        self.wpctl()?;
        let out = run(&self.pw_dump()?, &[]).await?;
        serde_json::from_slice(&out)
            .map_err(|e| OsError::Failed(format!("pw-dump printed no JSON: {e}")))
    }

    async fn call(&self, c: Call) -> Result<(), OsError> {
        run(&self.wpctl()?, &wpctl_args(c)).await.map(|_| ())
    }

    fn changes(&self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(64);
        tokio::spawn(monitor(self.pw_dump.clone(), tx));
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drain_json_returns_complete_arrays_and_keeps_a_partial_tail() {
        let mut buf = b"[1]\n[2".to_vec();
        assert_eq!(drain_json(&mut buf), vec![json!([1])]);
        assert_eq!(buf, b"[2");
        buf.extend_from_slice(b"]");
        assert_eq!(drain_json(&mut buf), vec![json!([2])]);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_json_handles_two_arrays_in_one_chunk() {
        let mut buf = b"[{\"id\": 1}]\n[{\"id\": 2}]\n".to_vec();
        assert_eq!(
            drain_json(&mut buf),
            vec![json!([{"id": 1}]), json!([{"id": 2}])]
        );
        assert!(buf.is_empty());
    }

    #[test]
    fn find_program_finds_a_file_path_and_rejects_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("fc-audio-find-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("wpctl");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        assert_eq!(find_program(&file), Some(file.clone()));
        assert_eq!(find_program(&dir.join("absent")), None);
        assert_eq!(find_program(&dir), None, "a directory is no program");
        // a bare name is looked up in $PATH: `sh` is everywhere, this name nowhere
        assert!(find_program(Path::new("sh")).is_some());
        assert_eq!(find_program(Path::new("fc-no-such-program-x55")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_program_busy_for_writing_is_spawned_once_it_is_free() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fc-audio-busy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("pw-dump");
        std::fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        // open for writing: exec of it fails with ETXTBSY until this is dropped
        let held = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        let freed = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            drop(held);
        });
        assert!(spawn_monitor(&file).await.is_some(), "retried past ETXTBSY");
        freed.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_wpctl_argv_of_each_call() {
        assert_eq!(
            wpctl_args(Call::SetVolume {
                id: 52,
                volume: 0.3
            }),
            ["set-volume", "52", "0.30", "-l", "1.5"]
        );
        assert_eq!(
            wpctl_args(Call::SetMute {
                id: 52,
                muted: false
            }),
            ["set-mute", "52", "0"]
        );
        assert_eq!(
            wpctl_args(Call::SetDefault { id: 53 }),
            ["set-default", "53"]
        );
    }
}
