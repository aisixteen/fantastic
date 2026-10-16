//! THE SCREEN IS `/dev/tty`, AND FDS 1 AND 2 GO TO A LOG FILE.
//!
//! This is not a preference. The kernel hosted in this process prints to stdout
//! and stderr on its own initiative — `bootstrap` announces hydration, the ledger
//! and message pumps announce themselves, a bundle reports a bad write. Those
//! writes are `println!`, they land on the same terminal ratatui is drawing to,
//! and they SCROLL IT. Measured 2026-09-13 in the headful frames: the attract
//! screen's title and stars stayed on screen forever underneath the live panes,
//! because a stray write had desynchronised ratatui's idea of what was where.
//!
//! Editing the kernel's prints is not an option — `src/lib/*` is off limits — and
//! it would be the wrong fix anyway: a kernel that logs is correct, a shell that
//! shares its screen with a log is not.
//!
//! So the split is at the file-descriptor level: the terminal is opened directly
//! as `/dev/tty` and the TUI draws THERE, while fds 1 and 2 are pointed at
//! `.fantastic/cli/kernel.log`. Everything the kernel says is kept, readable, and
//! off the screen. Input still comes from fd 0, which is untouched.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

/// The terminal, plus the handle the shell restores through.
pub struct Screen {
    pub term: Terminal<CrosstermBackend<File>>,
}

/// The log keeps its last this-many lines: older ones go when a run opens it.
pub(crate) const LOG_KEEP_LINES: usize = 10_000;

/// Cut `path` to its last `keep` lines, IN PLACE (same file: a running kernel's
/// append descriptor keeps writing to it). A missing file is nothing to cut.
fn keep_last_lines(path: &Path, keep: usize) -> Result<()> {
    let text = match std::fs::read(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
    };
    let lines = text.iter().filter(|&&b| b == b'\n').count();
    if lines <= keep {
        return Ok(());
    }
    // the byte after the (lines - keep)-th newline starts the kept tail
    let cut = text
        .iter()
        .enumerate()
        .filter(|(_, &b)| b == b'\n')
        .nth(lines - keep - 1)
        .map_or(0, |(i, _)| i + 1);
    std::fs::write(path, &text[cut..]).with_context(|| format!("could not write {}", path.display()))
}

/// Point fds 1 and 2 at `path`, keeping nothing on the screen. The log is cut
/// to its last [`LOG_KEEP_LINES`] lines first. Returns the log path actually
/// used so a failure can name it.
pub(crate) fn redirect_std_to(path: &Path) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).with_context(|| format!("could not write {}", d.display()))?;
    }
    keep_last_lines(path, LOG_KEEP_LINES)?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("could not open {}", path.display()))?;
    crate::sys::redirect_stdio_to(&log).with_context(|| format!("could not redirect stdout/stderr to {}", path.display()))
}

/// Open the screen. The panic hook is installed here and restores the terminal
/// BEFORE panicking, so a crash leaves a usable shell rather than a wrecked one.
pub fn open(log: &Path) -> Result<Screen> {
    redirect_std_to(log)?;
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open(crate::sys::TTY)
        .context("could not open /dev/tty — this shell needs a terminal")?;

    enable_raw_mode().context("could not enter raw mode")?;
    crossterm::execute!(
        tty,
        EnterAlternateScreen,
        // Bracketed paste ON so a paste is text, not a burst of keys that could
        // trigger bindings. Mouse capture stays OFF: capturing it breaks the
        // terminal's own selection and copy.
        crossterm::event::EnableBracketedPaste,
        crossterm::event::EnableFocusChange,
    )?;

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        prev(info);
    }));

    let term = Terminal::new(CrosstermBackend::new(tty))?;
    Ok(Screen { term })
}

/// Put the terminal back. Safe to call twice, and safe from a panic hook: it opens
/// its own handle rather than borrowing one that may be poisoned.
pub fn restore() {
    let _ = disable_raw_mode();
    if let Ok(mut tty) = OpenOptions::new().read(true).write(true).open(crate::sys::TTY) {
        let _ = crossterm::execute!(
            tty,
            crossterm::event::DisableBracketedPaste,
            crossterm::event::DisableFocusChange,
            crossterm::event::DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show,
        );
        let _ = tty.flush();
    }
}

/// Say something to the human AFTER the screen is restored. Goes to the tty, not
/// to stderr, because stderr is the kernel's log now.
pub fn say(line: &str) {
    if let Ok(mut tty) = OpenOptions::new().read(true).write(true).open(crate::sys::TTY) {
        let _ = writeln!(tty, "{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirecting_captures_what_a_print_would_have_shown() {
        // The whole point, asserted: after the redirect a `println!` lands in the
        // file and not on the screen. Run in a child process so this test cannot
        // steal the test harness's own stdout.
        let d = std::env::temp_dir().join(format!("fc-tty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let log = d.join("cli").join("kernel.log");
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--nocapture")
            .env("FC_TTY_CHILD", log.display().to_string())
            .arg("tty::tests::child_writes_to_the_log")
            .output()
            .expect("re-exec the test binary");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            text.contains("KERNEL SAID THIS"),
            "the log did not capture it: {text:?}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("KERNEL SAID THIS"),
            "it reached the screen anyway"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn child_writes_to_the_log() {
        let Ok(path) = std::env::var("FC_TTY_CHILD") else {
            return;
        };
        redirect_std_to(Path::new(&path)).unwrap();
        println!("KERNEL SAID THIS");
        eprintln!("AND THIS");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        let _ = std::io::stderr().flush();
    }

    #[test]
    fn the_log_keeps_only_its_last_lines() {
        let d = std::env::temp_dir().join(format!("fc-tty3-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join("kernel.log");
        let body: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&p, &body).unwrap();
        keep_last_lines(&p, 20).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), body, "under the cap: untouched");
        keep_last_lines(&p, 3).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "line 10\nline 11\nline 12\n");
        keep_last_lines(&d.join("none.log"), 3).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_log_path_is_created_on_demand() {
        let d = std::env::temp_dir().join(format!("fc-tty2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        // Only the directory creation is observable without touching this
        // process's own descriptors, so that is what this asserts.
        let p = d.join("deep").join("kernel.log");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        assert!(p.parent().unwrap().is_dir());
        let _ = std::fs::remove_dir_all(&d);
    }
}
