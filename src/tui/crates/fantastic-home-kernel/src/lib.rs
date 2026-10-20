//! THE HOME KERNEL: the manager of your fantastics, as a library the desktop
//! app hosts in-process.
//!
//! A minimal kernel in the fantastic home dir, `<home>/app` (its workdir and
//! its state dir; launch logs in `<home>/logs`), whose
//! only job is managing fantastics: its own sqlite store, a loopback web host
//! that serves the home page, and the `home` agent, which binds the pure
//! ViewModel (`fantastic_home::Home`) to the kernel protocol. No exec, no file
//! leg, no AI. Its only host is the desktop app (and the kiosk that runs that
//! app); the e2e's `fantastic-home-host` is a test binary.
//!
//! ```text
//!   lib        the API: home_dir_from_env, Config, Running, running, hand_over, HomeKernel
//!   kernel     HomeKernel: boot, url, port, call, events, shutdown; the registry; the seed
//!   tree       the ids, the tree and its grants (written whole on every boot)
//!   home       the `home` agent, its runner, its state, its OS port and OS layer
//! ```
//!
//! Every OS call is in `home/sys` (`tests/os_boundary.rs` holds the rest to it).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::json;

pub mod home;
mod kernel;
mod tree;

pub use fantastic_user_home_store::home_dir_from_env;
pub use kernel::HomeKernel;

use home::sys;

/// The README's example, compiled as a doctest (`no_run`: it needs a
/// `fantastic` binary and a port).
#[doc = include_str!("../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;

/// How long a running home has to answer a hand-over.
const HAND_OVER_TIMEOUT: Duration = Duration::from_secs(5);

/// What the host gives the home kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The fantastic home dir (absolute): `home_dir_from_env()` in the app.
    pub home: PathBuf,
    /// The web host's port; `None` keeps the recorded one (a free one the first time).
    pub port: Option<u16>,
    /// The `fantastic` binary (absolute) run as `fantastic canvas`, and on
    /// Windows as the stop helper. Never `current_exe()`: that is the app.
    pub fantastic_bin: PathBuf,
}

/// A home host that serves now, from its run file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Running {
    pub pid: u32,
    pub port: u16,
    pub url: String,
}

/// The host that serves `home` now, if any: the home kernel's lock, and its run
/// file `<home>/app/home.yaml`, which must name the lock's holder.
pub async fn running(home: &Path) -> Result<Option<Running>, String> {
    let w = kernel::workdir(home);
    match fantastic_kernel::lock::held_at(&w) {
        Err(e) => Err(e.to_string()),
        Ok(None) => Ok(None),
        Ok(Some(pid)) => match sys::read_host_run_file(home) {
            Some(run) if run.pid == pid => Ok(Some(Running {
                pid,
                port: run.port,
                url: run.url,
            })),
            _ => Err(format!(
                "{} is held by pid {pid}, but no run file names it a home host",
                home.display()
            )),
        },
    }
}

/// Hand the folders `paths` to the running home `to` (`open`, when there are
/// any), then ask it to show itself (`focus`). The first error is the `Err`.
pub async fn hand_over(to: &Running, paths: Vec<String>) -> Result<(), String> {
    if !paths.is_empty() {
        sys::forward(
            to.port,
            tree::HOME,
            json!({"type": "open", "paths": paths}),
            HAND_OVER_TIMEOUT,
        )
        .await?;
    }
    sys::forward(
        to.port,
        tree::HOME,
        json!({"type": "focus"}),
        HAND_OVER_TIMEOUT,
    )
    .await?;
    Ok(())
}
