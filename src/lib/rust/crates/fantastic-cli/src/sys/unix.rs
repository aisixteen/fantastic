//! Unix (macOS, Linux): tokio's unix signal streams.

use tokio::signal::unix::{signal, Signal, SignalKind};

/// The daemon's stop signals — SIGINT, SIGTERM and SIGHUP, the three python's
/// `_request_stop` handles. INSTALLED by [`StopSignals::install`]: from the moment
/// it returns, a delivered signal is HELD until [`StopSignals::recv`] takes it,
/// however late that await starts. A registration that fails is an error, never a
/// signal quietly left at its default action.
pub(crate) struct StopSignals {
    int: Signal,
    term: Signal,
    hup: Signal,
}

impl StopSignals {
    pub(crate) fn install() -> std::io::Result<Self> {
        Ok(Self {
            int: signal(SignalKind::interrupt())?,
            term: signal(SignalKind::terminate())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    /// The name of the first stop signal to arrive.
    pub(crate) async fn recv(&mut self) -> &'static str {
        tokio::select! {
            _ = self.int.recv() => "SIGINT",
            _ = self.term.recv() => "SIGTERM",
            _ = self.hup.recv() => "SIGHUP",
        }
    }
}

/// The daemon installs its stop handlers BEFORE it prints `[kernel] up`; this
/// pins what that buys: a SIGTERM that lands after `install` and before anyone
/// awaits is HELD for `recv`, not lost to the default action (which killed the
/// process with no teardown when the handler was only registered on the first
/// poll, after the up line). Unix only: a process cannot send a console control
/// event to itself alone on Windows.
#[cfg(test)]
mod stop_signal_tests {
    use super::StopSignals;
    use std::time::Duration;

    #[tokio::test]
    async fn a_sigterm_before_the_wait_is_held_not_lost() {
        let mut stop = StopSignals::install().expect("install the stop handlers");
        // Safety: `raise` only delivers SIGTERM to this process, whose handler is
        // installed above (tokio keeps it for the life of the process).
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = tokio::time::timeout(Duration::from_secs(5), stop.recv())
            .await
            .expect("the SIGTERM raised before the wait must be held");
        assert_eq!(got, "SIGTERM");
    }
}
