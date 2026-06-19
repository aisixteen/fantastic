//! Terminal-proxy mode: a real PTY hosted by the product (the runtime owns
//! spawning + ttys, per the kernel/host split). Spawns `$SHELL` via
//! `portable-pty` (same crate the kernel's terminal_backend uses), feeds output
//! into a `vt100` screen, and renders it with `tui-term`. The product's own
//! sugar-PTY — and the basis for the kernel's terminal capability post-purify.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc::UnboundedSender;

pub struct TerminalSession {
    pub parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    _child: Box<dyn Child + Send + Sync>,
    rows: u16,
    cols: u16,
}

impl TerminalSession {
    /// Spawn `$SHELL` in a PTY sized `rows`x`cols`. A reader thread pumps PTY
    /// bytes into the vt100 parser and pings `redraw` so the UI repaints.
    pub fn spawn(rows: u16, cols: u16, redraw: UnboundedSender<()>) -> Result<Self> {
        let (rows, cols) = (rows.max(1), cols.max(1));
        let pty = native_pty_system();
        let pair = pty.openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        let mut cmd = CommandBuilder::new(shell);
        cmd.env("TERM", "xterm-256color");
        if let Ok(cwd) = std::env::current_dir() {
            cmd.cwd(cwd);
        }
        let child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);

        let writer = pair.master.take_writer()?;
        let mut reader = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 2000)));
        let p2 = Arc::clone(&parser);
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if let Ok(mut p) = p2.lock() {
                            p.process(&buf[..n]);
                        }
                        if redraw.send(()).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        Ok(Self {
            parser,
            writer,
            master: pair.master,
            _child: child,
            rows,
            cols,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) {
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        let (rows, cols) = (rows.max(1), cols.max(1));
        if rows == self.rows && cols == self.cols {
            return;
        }
        self.rows = rows;
        self.cols = cols;
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Ok(mut p) = self.parser.lock() {
            p.set_size(rows, cols);
        }
    }
}

/// Rows the screen actually uses: the last row with any non-blank cell, +1,
/// clamped to [1, max]. A full-screen TUI paints every row → returns max; a
/// few lines of shell output → returns that few. (If the parser is in the
/// alternate screen, return max directly — alt-screen programs own the whole
/// grid even when individual rows read blank.)
pub fn used_rows(parser: &vt100::Parser, max: u16) -> u16 {
    let max = max.max(1);
    let screen = parser.screen();
    if screen.alternate_screen() {
        return max;
    }
    let (rows, cols) = screen.size();
    // Scan bottom-up for the last row carrying any non-blank cell.
    for row in (0..rows).rev() {
        let used = (0..cols).any(|col| {
            screen
                .cell(row, col)
                .map(|c| !c.contents().is_empty())
                .unwrap_or(false)
        });
        if used {
            return (row + 1).clamp(1, max);
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::used_rows;

    fn parser_with(rows: u16, cols: u16, bytes: &[u8]) -> vt100::Parser {
        let mut p = vt100::Parser::new(rows, cols, 0);
        p.process(bytes);
        p
    }

    #[test]
    fn two_lines_of_output_use_two_rows() {
        let p = parser_with(24, 80, b"hi\r\nthere\r\n");
        assert_eq!(used_rows(&p, 24), 2);
    }

    #[test]
    fn empty_screen_uses_one_row() {
        let p = parser_with(24, 80, b"");
        assert_eq!(used_rows(&p, 24), 1);
    }

    #[test]
    fn content_on_last_row_reaches_near_full_height() {
        // Push the cursor down to the final row, then print there.
        let p = parser_with(24, 80, b"\x1b[24;1Hlast");
        assert_eq!(used_rows(&p, 24), 24);
    }

    #[test]
    fn max_clamps_below_used() {
        // Three lines of output, but the caller only allows two rows.
        let p = parser_with(24, 80, b"a\r\nb\r\nc\r\n");
        assert_eq!(used_rows(&p, 2), 2);
    }
}
