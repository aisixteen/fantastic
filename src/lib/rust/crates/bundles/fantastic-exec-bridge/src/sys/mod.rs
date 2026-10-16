//! The OS boundary of exec_bridge: process spawning and teardown, the inherited
//! control socket, pseudo-terminals, the PATH grammar, and the temp dir a pasted
//! image lands in. Every OS call the rest of the crate makes goes through here, and
//! what the rest of the crate sees is OS-NEUTRAL — one name per concept, its OS
//! half in `unix.rs` / `windows.rs`:
//!
//! - **the control socket** — [`ctrl_pair`] makes a connected stream pair, one
//!   [`CtrlEnd`] for the job (its number, [`ctrl_number`], is what
//!   `FANTASTIC_CTRL_FD` advertises) and one the relay serves as a [`CtrlStream`]
//!   ([`ctrl_stream`]), split into [`CtrlReadHalf`] / [`CtrlWriteHalf`];
//! - **a job** — [`spawn`] starts one (a pipe job, or a PTY job on a [`PtySlave`])
//!   and answers a [`Child`]: its pid, its pipes, `wait`/`try_wait`. A job is a
//!   CONTAINER — everything it launches stays in it — and [`kill_session`] kills
//!   the whole container by the job's pid; [`interrupt`] is the polite Ctrl-C.
//!   [`exit_code`] is python's `returncode` of what `wait` answers
//!   ([`KILLED_EXIT_CODE`] for a killed job), and [`cwd_failed`] tells a spawn
//!   that failed on its directory from one that failed on its program;
//! - **a terminal** — [`open_pty`] answers the kernel's [`PtyMaster`] (async
//!   read / write, `resize`) and the [`PtySlave`] a job is spawned on;
//! - **the PATH grammar** — [`PATH_LIST_SEP`], [`names_a_file`], [`exe_names`],
//!   [`is_executable`] and [`not_found`] are the OS's halves of python's
//!   `proc.resolve`;
//! - [`mkdtemp`] — python `tempfile.mkdtemp`, owner-only.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::*;
#[cfg(unix)]
pub use unix::{Child, CtrlReadHalf, CtrlStream, CtrlWriteHalf};
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::*;
#[cfg(windows)]
pub use windows::{Child, CtrlReadHalf, CtrlStream, CtrlWriteHalf};
