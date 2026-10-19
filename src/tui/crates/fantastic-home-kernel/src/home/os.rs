//! THE OS PORT: everything the home asks of the machine, as one trait. `SysOs`
//! answers it by calling `super::sys` and nothing else; the tests answer it with
//! `FakeOs`.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use fantastic_home::{Answer, Ask};
use serde_json::Value;

use super::sys;

/// What the home asks of the machine.
#[async_trait::async_trait]
pub trait HomeOs: Send + Sync + 'static {
    fn now_ms(&self) -> u64;
    async fn read_registry(&self) -> Result<Vec<(String, String)>, String>;
    async fn answer(&self, ask: &Ask) -> Answer;
    async fn spawn(&self, path: &str) -> Result<(), String>;
    /// Ok(true): asked to stop; Ok(false): nothing holds the folder; Err: could not ask.
    async fn terminate(&self, path: &str) -> Result<bool, String>;
    async fn forward(
        &self,
        port: u16,
        target: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, String>;
    /// `path` as a kernel running there names its folder; Err names why it is not one.
    fn folder(&self, path: &str) -> Result<String, String>;
    fn log_file(&self, path: &str) -> Option<String>;
    fn log_tail(&self, path: &str) -> Option<String>;
}

/// The real machine. `home` is the home dir (the registry every fantastic it
/// starts writes into); `fantastic_bin` the one binary it runs.
pub struct SysOs {
    home: PathBuf,
    fantastic_bin: PathBuf,
    /// The login environment, resolved once it has been read successfully.
    env: tokio::sync::OnceCell<Vec<(OsString, OsString)>>,
}

impl SysOs {
    pub fn new(home: PathBuf, fantastic_bin: PathBuf) -> SysOs {
        SysOs {
            home,
            fantastic_bin,
            env: tokio::sync::OnceCell::new(),
        }
    }
}

#[async_trait::async_trait]
impl HomeOs for SysOs {
    fn now_ms(&self) -> u64 {
        sys::now_ms()
    }

    async fn read_registry(&self) -> Result<Vec<(String, String)>, String> {
        sys::read_registry(&self.home)
    }

    async fn answer(&self, ask: &Ask) -> Answer {
        match ask {
            Ask::IsFantastic { path } => Answer::IsFantastic(sys::is_fantastic(path)),
            Ask::LockHolder { path } => Answer::LockHolder(sys::lock_holder(path)),
            Ask::RunFile { path } => Answer::RunFile(sys::run_file(path)),
            Ask::CanvasUp { canvas } => Answer::CanvasUp(sys::canvas_up(canvas).await),
            Ask::Guid { port } => Answer::Guid(sys::guid(*port).await),
        }
    }

    /// The login environment, the logs dir and the log file are resolved first;
    /// any failure is the spawn's.
    async fn spawn(&self, path: &str) -> Result<(), String> {
        let env = self.env.get_or_try_init(sys::login_env).await?;
        let log = sys::log_path(path)?;
        sys::spawn(&self.fantastic_bin, path, env, &self.home, &log).await
    }

    async fn terminate(&self, path: &str) -> Result<bool, String> {
        match sys::lock_holder(path) {
            None => Ok(false),
            Some(holder) => sys::terminate_pid(&self.fantastic_bin, holder).await,
        }
    }

    async fn forward(
        &self,
        port: u16,
        target: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        sys::forward(port, target, payload, timeout).await
    }

    fn folder(&self, path: &str) -> Result<String, String> {
        sys::folder(path)
    }

    fn log_file(&self, path: &str) -> Option<String> {
        sys::log_file(path)
    }

    fn log_tail(&self, path: &str) -> Option<String> {
        sys::log_tail(path)
    }
}
