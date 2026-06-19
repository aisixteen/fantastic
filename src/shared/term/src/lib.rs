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
