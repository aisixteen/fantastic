//! Windows: tokio's console control event streams.

use tokio::signal::windows::{
    ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown, CtrlBreak, CtrlC, CtrlClose, CtrlShutdown,
};

/// The daemon's stop signals — the console control events: Ctrl-C, Ctrl-Break,
/// the console closing and the system shutting down. Each is named as python's
/// `signal` module sees it (the C runtime raises SIGINT for Ctrl-C and SIGBREAK
/// for every other event). INSTALLED by [`StopSignals::install`]: from the
/// moment it returns, a delivered event is HELD until [`StopSignals::recv`]
/// takes it. A registration that fails is an error, never an event quietly left
/// to its default action (the process killed with no teardown).
pub(crate) struct StopSignals {
    c: CtrlC,
    brk: CtrlBreak,
    close: CtrlClose,
    shutdown: CtrlShutdown,
}

impl StopSignals {
    pub(crate) fn install() -> std::io::Result<Self> {
        Ok(Self {
            c: ctrl_c()?,
            brk: ctrl_break()?,
            close: ctrl_close()?,
            shutdown: ctrl_shutdown()?,
        })
    }

    /// The name of the first stop event to arrive.
    pub(crate) async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.c.recv() => "SIGINT",
            _ = self.brk.recv() => "SIGBREAK",
            _ = self.close.recv() => "SIGBREAK",
            _ = self.shutdown.recv() => "SIGBREAK",
        }
    }
}
