//! THE ONE VIEWMODEL, and nothing that draws on a screen.
//!
//! It owns the `App` — nav state, keys, panels, the kernel's reads — and after
//! every change renders the whole shell into an OFF-SCREEN buffer at the size it
//! was last given; the terminal View draws only from it. (The browser View that
//! drew the same frames is gone: the canvas replaced it, owner 2026-09-30.)
//!
//! Inputs are INTENTS (`VmIn`): a key or paste event, and a resize. Nothing
//! else reaches the App. Outputs are `Frame`s on a `watch` channel: the newest
//! frame only; a slow View skips to the newest instead of queueing.
//!
//! There is no default size: nothing is drawn until a View says how big it is.

use std::sync::Arc;
use std::time::Duration;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use tokio::sync::{mpsc, watch};

use crate::app::{App, Link};
use crate::env::Paths;
use crate::ev::{Cmd, Ev};
use crate::kern::host::Host;

/// 60 Hz: the most often a CHANGE is drawn (requests between ticks coalesce).
const FRAME: Duration = Duration::from_millis(16);
/// An animation alone (the `thinking` flash) redraws at ~30 fps — codex's
/// status line paces itself the same way (`schedule_frame_in(32ms)`).
const ANIM_FRAME: Duration = Duration::from_millis(32);
/// Presence, blink, poll cadence.
const TICK: Duration = Duration::from_millis(250);
/// The safety net under anything live.
const RECONCILE: Duration = Duration::from_secs(60);

/// An intent from a View.
#[derive(Debug)]
pub enum VmIn {
    Event(crossterm::event::Event),
    Resize(u16, u16),
}

/// One rendered screen: the universal view data.
#[derive(Debug, Clone)]
pub struct Frame {
    pub buf: Buffer,
    pub cursor: Option<(u16, u16)>,
    /// Whether clicks are wanted (`App::mouse`): the terminal View captures
    /// the mouse only then, so native copy works when it is off.
    pub mouse: bool,
}

/// How a View reaches the ViewModel.
#[derive(Clone)]
pub struct Handle {
    pub input: mpsc::Sender<VmIn>,
    pub frames: watch::Receiver<Option<Arc<Frame>>>,
}

/// What the ViewModel does once the kernel is up.
#[derive(Debug, Clone)]
pub struct Boot {
    /// Serve the canvas (the `canvas` recipe).
    pub canvas: bool,
    /// `--port`: creation-only; a later run reads it from `web`'s record.
    pub port: Option<u16>,
    /// A terminal View is attached (it sizes the ViewModel).
    pub tui: bool,
    /// HEADLESS ONLY: where to say the page's URL, or why there is none. The
    /// process's own stdout from before it was pointed at the kernel log.
    pub announce: Option<Arc<std::fs::File>>,
}

/// Set when the ViewModel stopped because it could not serve at all (the
/// kernel did not boot, or a headless page could not be set up): the process
/// then exits non-zero.
pub static FAILED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn failed() {
    FAILED.store(true, std::sync::atomic::Ordering::SeqCst);
}

impl Boot {
    fn say(&self, line: &str) {
        use std::io::Write;
        if let Some(mut f) = self.announce.as_deref() {
            let _ = writeln!(f, "{line}");
        }
    }
}

/// The ViewModel, created but not yet running.
pub struct Vm {
    input: mpsc::Receiver<VmIn>,
    frames: watch::Sender<Option<Arc<Frame>>>,
}

/// Create the ViewModel and the handle every View uses.
pub fn new() -> (Handle, Vm) {
    let (in_tx, input) = mpsc::channel(1024);
    let (frames, f_rx) = watch::channel(None);
    let h = Handle { input: in_tx, frames: f_rx };
    (h, Vm { input, frames })
}

impl Vm {
    /// Run until the human quits. INLINE, on the caller's task: the `App`'s
    /// panels are not `Send`, so the ViewModel stays on one task and the
    /// Views (which are) are the ones spawned. Returns the final `App`, whose
    /// notes the caller says after restoring its screen.
    pub async fn run(self, app: App, paths: Paths, boot: Boot) -> App {
        run(app, paths, boot, self.input, self.frames).await
    }
}

async fn run(
    mut app: App,
    paths: Paths,
    boot: Boot,
    mut input: mpsc::Receiver<VmIn>,
    frames: watch::Sender<Option<Arc<Frame>>>,
) -> App {
    let (tx, mut rx) = mpsc::channel::<Ev>(1024);
    let mut frame = tokio::time::interval(FRAME);
    let mut tick = tokio::time::interval(TICK);
    let mut recon = tokio::time::interval(RECONCILE);
    for i in [&mut frame, &mut tick, &mut recon] {
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    }
    let mut term: Option<Terminal<TestBackend>> = None;
    let mut last_drawn = std::time::Instant::now();

    let mut host: Option<Arc<Host>> = None;
    let mut cmds: Option<mpsc::Sender<Cmd>> = None;
    let mut booting = false;
    // A SIGNAL IS A QUIT, the same quit a key makes: the kernel shuts down in
    // its own order and the lock is released. Without this, `kill` on a
    // headless `fantastic canvas` left the lock file behind (measured).
    let mut term_sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    let mut int_sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("SIGINT handler");

    loop {
        tokio::select! {
            biased;

            _ = term_sig.recv() => app.quit = true,
            _ = int_sig.recv() => app.quit = true,

            _ = frame.tick() => {
                if let Some(t) = term.as_mut() {
                    let anim_due = app.animating() && last_drawn.elapsed() >= ANIM_FRAME;
                    if app.dirty || anim_due {
                        last_drawn = std::time::Instant::now();
                        let size = t.size().expect("the off-screen backend has a size");
                        crate::ui::layout(&mut app, ratatui::layout::Rect::new(0, 0, size.width, size.height));
                        let buf = t
                            .draw(|f| crate::ui::draw(f, &app))
                            .expect("the off-screen backend cannot fail")
                            .buffer
                            .clone();
                        let b = t.backend();
                        let cursor = b.cursor_visible().then(|| {
                            let p = b.cursor_position();
                            (p.x, p.y)
                        });
                        frames.send_replace(Some(Arc::new(Frame { buf, cursor, mouse: app.mouse })));
                        app.dirty = false;
                        dispatch(&mut app, &cmds);
                    }
                }
            }

            maybe = input.recv() => match maybe {
                Some(VmIn::Event(e)) => {
                    app.apply(Ev::Term(e));
                    dispatch(&mut app, &cmds);
                }
                Some(VmIn::Resize(w, h)) => {
                    match term.as_mut() {
                        Some(t) => t.backend_mut().resize(w, h),
                        None => term = Some(Terminal::new(TestBackend::new(w, h)).expect("off-screen terminal")),
                    }
                    // A resize redraws everything: the old frame's cells are stale.
                    if let Some(t) = term.as_mut() {
                        let _ = t.clear();
                    }
                    app.dirty = true;
                }
                None => break,
            },

            Some(ev) = rx.recv() => {
                app.apply(ev);
                while let Ok(more) = rx.try_recv() {
                    app.apply(more);
                }
                dispatch(&mut app, &cmds);
            }

            _ = tick.tick() => {
                app.on_tick();
                dispatch(&mut app, &cmds);
            }

            _ = recon.tick() => app.apply(Ev::Recon),
        }

        // BOOT, once, as soon as the directory is seeded (see `run.rs` history:
        // the order is the kernel daemon's own).
        if host.is_none() && !booting && app.phase != crate::boot::attract::Phase::Attract {
            booting = true;
            if let Err(e) = crate::tty::redirect_std_to(&paths.cli.join("kernel.log")) {
                app.note(format!("{e}"));
            }
            match Host::boot(&paths, boot.canvas).await {
                Ok(h) => {
                    let h = Arc::new(h);
                    app.cfg = h.cfg.clone();
                    app.link = Link::Up;
                    crate::kern::runner::spawn_row_tail(Arc::clone(&h), tx.clone(), h.kernel.rows.next_id() - 1);
                    crate::kern::runner::spawn_row_watch(Arc::clone(&h), tx.clone()).await;
                    crate::kern::runner::subscribe_state(&h, tx.clone());
                    cmds = Some(crate::kern::runner::spawn(Arc::clone(&h), tx.clone(), paths.clone()));
                    if boot.canvas {
                        match crate::kern::web::ensure(&h, boot.port).await {
                            Ok(urls) => {
                                boot.say(&format!("fantastic: the canvas: {}", urls.canvas));
                                app.note(format!("the canvas: {}", urls.canvas));
                            }
                            // No page to serve is fatal where the page is the
                            // only View; with a terminal it is a note there.
                            // No page is fatal: a served-but-dead page is worse
                            // than a loud stop. The reason is said on the way out.
                            Err(e) => {
                                boot.say(&format!("fantastic: {e}"));
                                app.note(e);
                                failed();
                                app.quit = true;
                            }
                        }
                    }
                    // HEADLESS: the kernel's message stream is the whole output.
                    if boot.announce.is_some() && !boot.tui {
                        spawn_stream(Arc::clone(&h), boot.clone());
                    }
                    host = Some(h);
                    for c in [Cmd::RootReflect, Cmd::Tree, Cmd::Agents, Cmd::Ps] {
                        app.push(c);
                    }
                    dispatch(&mut app, &cmds);
                }
                Err(e) => {
                    boot.say(&format!("fantastic: {e}"));
                    app.note(format!("{e}"));
                    failed();
                    app.quit = true;
                }
            }
        }

        if app.quit {
            break;
        }
    }

    if let Some(c) = &cmds {
        let _ = c.try_send(Cmd::Quit);
    }
    app.seen.flush();
    if let Some(h) = &host {
        h.shutdown(&paths).await;
    }
    // Dropping the sender tells every View the ViewModel is gone.
    drop(frames);
    app
}

/// The one place a `Cmd` leaves the app.
fn dispatch(app: &mut App, cmds: &Option<mpsc::Sender<Cmd>>) {
    let out = app.drain_cmds();
    if out.is_empty() {
        return;
    }
    let Some(tx) = cmds else {
        return;
    };
    for c in out {
        if let Err(e) = tx.try_send(c.clone()) {
            if c.is_act() {
                app.note(format!("{:?} was not sent: the command queue is full ({e})", c.what()));
            }
        }
    }
}

/// `--headless`: one line per row, as it lands. The kernel's execution rows come
/// off its row tail; an AI agent's own rows come from its row event (the shell
/// watches every AI agent at boot) — a digest, so the line names the row and
/// its size, never its content.
fn spawn_stream(h: Arc<Host>, boot: Boot) {
    let tail_boot = boot.clone();
    let hh = Arc::clone(&h);
    tokio::spawn(async move {
        let mut tail = hh.kernel.rows.subscribe(hh.kernel.rows.next_id() - 1);
        while let Some(id) = tail.recv().await {
            if let Some(row) = hh.kernel.rows.get_one(id) {
                tail_boot.say(&stream_line(&serde_json::Value::Object(row)));
            }
        }
    });
    let w = fantastic_kernel::AgentId::from(format!("{}#stream", h.cfg.user).as_str());
    let mut rx = h.kernel.ensure_inbox(&w);
    tokio::spawn(async move {
        for ai in h.ai_agents() {
            let _ = h.operator(&ai, serde_json::json!({"type": "watch", "watcher": w.0})).await;
        }
        while let Some(item) = rx.recv().await {
            let p = &item.payload;
            if p.get("type").and_then(serde_json::Value::as_str) != Some("row") {
                continue;
            }
            let mut row = p["row"].clone();
            row["agent"] = p["source"].clone();
            boot.say(&stream_line(&row));
        }
    });
}

/// `[<id>] <agent> <kind|role> <verb>? · <first line of content>`
pub fn stream_line(row: &serde_json::Value) -> String {
    let s = |k: &str| row.get(k).and_then(serde_json::Value::as_str).unwrap_or("");
    let what = if s("kind").is_empty() { s("role") } else { s("kind") };
    let head = crate::tags::head_of(s("content"));
    let mut line = format!("[{}] {} {}", row.get("id").map(|v| v.to_string()).unwrap_or_default(), s("agent"), what);
    if !s("verb").is_empty() {
        line.push(' ');
        line.push_str(s("verb"));
    }
    if !head.is_empty() {
        line.push_str(" · ");
        line.extend(head.chars().take(160));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_stream_line_says_who_what_and_the_first_line() {
        let row = json!({"id": 12, "agent": "ai", "role": "assistant", "content": "I'll scan it.\nmore"});
        assert_eq!(stream_line(&row), "[12] ai assistant · I'll scan it.");
        let call = json!({"id": 13, "agent": "files", "kind": "call", "verb": "list"});
        assert_eq!(stream_line(&call), "[13] files call list");
    }
}
