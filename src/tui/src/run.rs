//! THE TERMINAL VIEW. It owns the tty and nothing else.
//!
//! Every key becomes an intent to the ViewModel (`vm`), every frame the
//! ViewModel publishes is painted cell for cell, and the terminal's size is an
//! intent too. It never touches the `App`.

use anyhow::Result;
use futures_util::StreamExt;

use crate::app::App;
use crate::env::Paths;
use crate::recipe::Opts;
use crate::vm::{self, VmIn};

pub async fn main_loop(opts: Opts) -> Result<()> {
    let paths = Paths::here()?;
    // A KERNEL ALREADY RUNS HERE: this is a THIN CLIENT of its shell (`share`)
    // — the owner drives recipes, so a thin client takes none.
    if let Ok(Some(pid)) = fantastic_kernel::lock::held(&paths.cwd) {
        if pid != std::process::id() {
            if opts.headless || !opts.recipes.is_empty() || opts.port.is_some() {
                anyhow::bail!(
                    "a kernel runs in this directory (pid {pid}); recipes and flags belong to the process that owns it — run bare `fantastic` to attach as a thin client"
                );
            }
            return crate::share::attach(&paths, pid).await;
        }
    }
    // Running here IS the command to init, like `fantastic mcp`: a fresh
    // directory is seeded before anything else.
    if paths.is_fresh() {
        crate::boot::seed::write_all(&paths).map_err(|e| anyhow::anyhow!("init failed: {e}"))?;
    }
    let mut app = App::new(paths.clone());
    app.want = opts.shows();

    if opts.headless {
        // Keep the real stdout for the announce line and the message stream,
        // before it becomes the kernel log.
        // SAFETY: dup of fd 1 returns a fresh descriptor this File owns.
        let out = unsafe { libc::dup(1) };
        anyhow::ensure!(out >= 0, "could not keep stdout");
        let announce = Some(std::sync::Arc::new(unsafe {
            <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(out)
        }));
        crate::tty::redirect_std_to(&paths.cli.join("kernel.log"))?;
        let boot = vm::Boot { canvas: opts.canvas(), port: opts.port, vllm: opts.vllm.clone(), tui: false, announce };
        let (_h, model) = vm::new();
        let _ = model.run(app, paths, boot).await;
        if vm::FAILED.load(std::sync::atomic::Ordering::SeqCst) {
            std::process::exit(2);
        }
        return Ok(());
    }
    // The TUI draws on /dev/tty and the kernel's own prints go to a log file.
    let log = paths.cli.join("kernel.log");
    let screen = crate::tty::open(&log)?;
    let boot = vm::Boot { canvas: opts.canvas(), port: opts.port, vllm: opts.vllm.clone(), tui: true, announce: None };
    let (h, model) = vm::new();
    let view = tokio::spawn(paint(screen.term, h));
    let app = model.run(app, paths, boot).await;
    let res = view.await?;
    crate::tty::restore();
    for n in &app.notes {
        crate::tty::say(&format!("fantastic: {n}"));
    }
    if vm::FAILED.load(std::sync::atomic::Ordering::SeqCst) {
        std::process::exit(2);
    }
    res
}

/// Keys and size in, frames out, until the ViewModel ends.
async fn paint(
    mut term: ratatui::Terminal<ratatui::backend::CrosstermBackend<std::fs::File>>,
    mut h: vm::Handle,
) -> Result<()> {
    let size = term.size()?;
    let _ = h.input.send(VmIn::Resize(vm::LOCAL, size.width, size.height)).await;
    let mut keys = crossterm::event::EventStream::new();
    let mut captured = false;
    loop {
        tokio::select! {
            changed = h.frames.changed() => {
                if changed.is_err() {
                    break; // the ViewModel is gone: the human quit
                }
                // THIS View's frame (`vm::Frames`): laid out for it alone
                let Some(frame) = h.frames.borrow_and_update().as_ref().and_then(|fs| fs.for_view(vm::LOCAL)) else { continue };
                if frame.mouse != captured {
                    captured = frame.mouse;
                    if captured {
                        crossterm::execute!(term.backend_mut(), crossterm::event::EnableMouseCapture)?;
                    } else {
                        crossterm::execute!(term.backend_mut(), crossterm::event::DisableMouseCapture)?;
                    }
                }
                term.draw(|f| {
                    let area = f.area();
                    let buf = f.buffer_mut();
                    for y in 0..area.height.min(frame.buf.area.height) {
                        for x in 0..area.width.min(frame.buf.area.width) {
                            if let (Some(src), Some(dst)) = (frame.buf.cell((x, y)), buf.cell_mut((x, y))) {
                                *dst = src.clone();
                            }
                        }
                    }
                    if let Some((x, y)) = frame.cursor {
                        f.set_cursor_position((x, y));
                    }
                })?;
            }
            maybe = keys.next() => match maybe {
                Some(Ok(crossterm::event::Event::Resize(w, hgt))) => {
                    let _ = h.input.send(VmIn::Resize(vm::LOCAL, w, hgt)).await;
                }
                Some(Ok(e)) => {
                    let _ = h.input.send(VmIn::Event(vm::LOCAL, e)).await;
                }
                Some(Err(_)) | None => break,
            },
        }
    }
    if captured {
        let _ = crossterm::execute!(term.backend_mut(), crossterm::event::DisableMouseCapture);
    }
    Ok(())
}
