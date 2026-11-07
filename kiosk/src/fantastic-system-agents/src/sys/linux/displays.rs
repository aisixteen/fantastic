//! DISPLAYS on Linux: the Wayland compositor's outputs through `wlr-randr`
//! (the compositor, cage, implements wlr-output-management; there is no change
//! stream, so the agent reads on a clock and after each write), and the
//! backlight through logind's `Session.SetBrightness` on the display session,
//! its levels read from `/sys/class/backlight/<name>/{brightness,max_brightness}`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use fantastic_system::displays::{BacklightRead, Call, Event, Read};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;

use crate::sys::linux::audio::find_program;
use crate::sys::linux::logind::{display_session, LoginSessionProxy, LOGIND};
use crate::sys::linux::os_error;
use crate::sys::{DisplaysOs, OsError};

/// How long one `wlr-randr` may take.
const TIMEOUT: Duration = Duration::from_secs(5);

pub struct LinuxDisplays {
    conn: zbus::Connection,
    program: PathBuf,
    /// `$XDG_RUNTIME_DIR` and `$WAYLAND_DISPLAY`: the compositor's socket is their join.
    runtime_dir: PathBuf,
    wayland_display: String,
    backlight_dir: PathBuf,
}

/// The user id of this process, from its `/proc` entry (no C library).
fn own_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0)
}

impl LinuxDisplays {
    /// `wlr-randr` from `$PATH`, the compositor in the environment's
    /// `XDG_RUNTIME_DIR` and `WAYLAND_DISPLAY` (`wayland-0` when unset), the
    /// kernel's backlight directory, and logind on a given system-bus connection.
    pub fn new(system: zbus::Connection) -> Self {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", own_uid())));
        let wayland_display = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "wayland-0".to_string());
        LinuxDisplays {
            conn: system,
            program: "wlr-randr".into(),
            runtime_dir,
            wayland_display,
            backlight_dir: "/sys/class/backlight".into(),
        }
    }

    /// A given `wlr-randr` (tests use a fake).
    pub fn with_program(mut self, program: PathBuf) -> Self {
        self.program = program;
        self
    }

    /// A given compositor socket: `$XDG_RUNTIME_DIR` and `$WAYLAND_DISPLAY`.
    pub fn with_wayland(mut self, runtime_dir: PathBuf, display: &str) -> Self {
        self.runtime_dir = runtime_dir;
        self.wayland_display = display.into();
        self
    }

    /// A given backlight directory (tests use a temp one).
    pub fn with_backlight_dir(mut self, dir: PathBuf) -> Self {
        self.backlight_dir = dir;
        self
    }

    fn socket(&self) -> PathBuf {
        self.runtime_dir.join(&self.wayland_display)
    }

    /// Run `wlr-randr` with the compositor's environment: its stdout.
    async fn wlr_randr(&self, args: &[String]) -> Result<Vec<u8>, OsError> {
        let program = find_program(&self.program)
            .ok_or_else(|| OsError::Unavailable("wlr-randr is not installed".into()))?;
        let socket = self.socket();
        if !socket.exists() {
            return Err(OsError::Unavailable(format!(
                "no Wayland display at {}",
                socket.display()
            )));
        }
        let child = Command::new(&program)
            .args(args)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env("WAYLAND_DISPLAY", &self.wayland_display)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| OsError::Failed(format!("wlr-randr: {e}")))?;
        let out = tokio::time::timeout(TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| OsError::Failed("wlr-randr took longer than 5 s".into()))?
            .map_err(|e| OsError::Failed(format!("wlr-randr: {e}")))?;
        if out.status.success() {
            return Ok(out.stdout);
        }
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        Err(OsError::Failed(if err.is_empty() {
            format!("wlr-randr failed ({})", out.status)
        } else {
            err
        }))
    }

    /// Every backlight device, by name; none when the directory is missing.
    fn backlights(&self) -> Vec<BacklightRead> {
        let Ok(dir) = std::fs::read_dir(&self.backlight_dir) else {
            return vec![];
        };
        let number = |d: &Path, file: &str| {
            std::fs::read_to_string(d.join(file))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
        };
        let mut found: Vec<BacklightRead> = dir
            .flatten()
            .filter_map(|e| {
                let path = e.path();
                Some(BacklightRead {
                    name: e.file_name().to_string_lossy().into_owned(),
                    brightness: number(&path, "brightness")?,
                    max: number(&path, "max_brightness")?,
                    kind: std::fs::read_to_string(path.join("type"))
                        .map(|s| s.trim().to_string())
                        .unwrap_or_default(),
                })
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }
}

#[async_trait::async_trait]
impl DisplaysOs for LinuxDisplays {
    async fn read(&self) -> Result<Read, OsError> {
        let json = self.wlr_randr(&["--json".to_string()]).await?;
        let outputs: Value = serde_json::from_slice(&json)
            .map_err(|e| OsError::Failed(format!("wlr-randr printed no JSON: {e}")))?;
        Ok(Read {
            outputs,
            backlights: self.backlights(),
        })
    }

    async fn call(&self, c: Call) -> Result<(), OsError> {
        match c {
            Call::SetOutput { argv, .. } => self.wlr_randr(&argv).await.map(|_| ()),
            Call::SetBrightness { name, brightness } => {
                let e = |e| os_error(e, LOGIND);
                let (_, path) = display_session(&self.conn).await?;
                LoginSessionProxy::builder(&self.conn)
                    .path(path)
                    .map_err(e)?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await
                    .map_err(e)?
                    .set_brightness("backlight", &name, brightness)
                    .await
                    .map_err(e)
            }
        }
    }

    /// `wlr-randr` has no change stream: the agent reads on its clock.
    fn changes(&self) -> mpsc::Receiver<Event> {
        mpsc::channel(1).1
    }
}
