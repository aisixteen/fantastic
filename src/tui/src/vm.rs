//! THE ONE VIEWMODEL, and nothing that draws on a screen.
//!
//! It owns the `App` — nav state, keys, panels, the kernel's reads — and after
//! every change renders the whole shell into an OFF-SCREEN buffer at the size it
//! was last given; the terminal View draws only from it. (The browser View that
//! drew the same frames is gone: the canvas replaced it, owner 2026-09-30.)
//!
//! Inputs are INTENTS (`VmIn`) from any number of Views — the owner's own
//! terminal and every thin client (`share`): a key or paste event, a resize, a
//! View gone. Nothing else reaches the App. Outputs are [`Frames`] on a
//! `watch` channel: the newest only; a slow View skips to the newest.
//!
//! ONE STATE, A FRAME PER VIEW (owner, 2026-10-02: "show we know MVVM"; "the
//! view is arbitrary"). The shared state is laid out and drawn once per View —
//! a terminal of any size, a thin client, anything that shows the shell — each
//! with ITS OWN derived state (`App::ViewSlots`: its screen, each panel's scroll
//! window and wrap memo), entered before its pass and left after. Every View
//! fits exactly and they all move together. The one thing that cannot have two
//! sizes — a terminal panel's PTY — follows the AUTHORITATIVE View, the latest
//! to resize or type (`effective`, tmux's `latest`); the others draw it as is.
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
/// Presence, blink (only while a View is attached).
const TICK: Duration = Duration::from_millis(250);

/// Which View an intent came from: the owner's own terminal is [`LOCAL`];
/// every thin client (`share`) gets its own id.
pub type ViewId = u64;
pub const LOCAL: ViewId = 0;

/// An intent from a View.
#[derive(Debug)]
pub enum VmIn {
    Event(ViewId, crossterm::event::Event),
    Resize(ViewId, u16, u16),
    /// A thin client went away.
    Gone(ViewId),
}

/// THE SIZE POLICY, tmux's `latest`: the screen is the size of the View that
/// most recently resized or typed; with that one gone, any remaining View
/// (the owner's own first). None until some View has said how big it is.
pub fn effective(sizes: &std::collections::BTreeMap<ViewId, (u16, u16)>, latest: Option<ViewId>) -> Option<(u16, u16)> {
    latest.and_then(|v| sizes.get(&v).copied()).or_else(|| sizes.values().next().copied())
}

/// Every View's frame: the same state, laid out for each View.
#[derive(Debug, Clone, Default)]
pub struct Frames {
    pub by_view: std::collections::BTreeMap<ViewId, Arc<Frame>>,
}

impl Frames {
    pub fn for_view(&self, v: ViewId) -> Option<Arc<Frame>> {
        self.by_view.get(&v).cloned()
    }
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
    pub frames: watch::Receiver<Option<Arc<Frames>>>,
}

/// What the ViewModel does once the kernel is up.
#[derive(Debug, Clone)]
pub struct Boot {
    /// Serve the canvas (the `canvas` recipe).
    pub canvas: bool,
    /// `--port`: stored on `web`'s record; a different one overwrites it at boot.
    pub port: Option<u16>,
    /// `--host`: the address `web` binds, stored on its record like `port`.
    pub bind: Option<String>,
    /// `ai BACKEND …`: the root AI's backend (`kern/root_ai`).
    pub ai: Option<crate::kern::root_ai::Backend>,
    /// `run COMMAND…`: the program the terminal (or a canvas cell) runs.
    pub run: Option<Vec<String>>,
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
    /// The same input, for the thin clients' Views (`share`).
    input_tx: mpsc::Sender<VmIn>,
    frames: watch::Sender<Option<Arc<Frames>>>,
}

/// Create the ViewModel and the handle every View uses.
pub fn new() -> (Handle, Vm) {
    let (in_tx, input) = mpsc::channel(1024);
    let (frames, f_rx) = watch::channel(None);
    let h = Handle { input: in_tx.clone(), frames: f_rx };
    (h, Vm { input, input_tx: in_tx, frames })
}

impl Vm {
    /// Run until the human quits. INLINE, on the caller's task: the `App`'s
    /// panels are not `Send`, so the ViewModel stays on one task and the
    /// Views (which are) are the ones spawned. Returns the final `App`, whose
    /// notes the caller says after restoring its screen.
    pub async fn run(self, app: App, paths: Paths, boot: Boot) -> App {
        run(app, paths, boot, self.input, self.input_tx, self.frames).await
    }
}

async fn run(
    mut app: App,
    paths: Paths,
    boot: Boot,
    mut input: mpsc::Receiver<VmIn>,
    input_tx: mpsc::Sender<VmIn>,
    frames: watch::Sender<Option<Arc<Frames>>>,
) -> App {
    let (tx, mut rx) = mpsc::channel::<Ev>(1024);
    let mut frame = tokio::time::interval(FRAME);
    let mut tick = tokio::time::interval(TICK);
    for i in [&mut frame, &mut tick] {
        i.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    }
    // One off-screen terminal per View, and each View's own derived state.
    let mut terms: std::collections::BTreeMap<ViewId, Terminal<TestBackend>> = Default::default();
    let mut slots: std::collections::BTreeMap<ViewId, crate::app::ViewSlots> = Default::default();
    let mut last_drawn = std::time::Instant::now();
    // Every View's size, and the one the screen follows (`effective`).
    let mut sizes: std::collections::BTreeMap<ViewId, (u16, u16)> = Default::default();
    let mut latest: Option<ViewId> = None;
    let mut share: Option<Arc<crate::share::Share>> = None;

    let mut host: Option<Arc<Host>> = None;
    let mut cmds: Option<mpsc::Sender<Cmd>> = None;
    let mut booting = false;
    // A SIGNAL IS A QUIT, the same quit a key makes: the kernel shuts down in
    // its own order and the lock is released. Without this, `kill` on a
    // headless `fantastic canvas` left the lock file behind (measured).
    let mut term_sig = crate::sys::terminate().expect("SIGTERM handler");
    let mut int_sig = crate::sys::interrupt().expect("SIGINT handler");

    loop {
        // BOOT, once, as soon as the directory is seeded — BEFORE the wait: with
        // nothing attached (headless) no branch fires, so it must not wait for one (see `run.rs` history:
        // the order is the kernel daemon's own).
        if host.is_none() && !booting {
            booting = true;
            if let Err(e) = crate::tty::redirect_std_to(&paths.cli.join("kernel.log")) {
                app.note(format!("{e}"));
            }
            match Host::boot(&paths, boot.canvas, boot.port, boot.bind.clone()).await {
                Ok(h) => {
                    let h = Arc::new(h);
                    app.cfg = h.cfg.clone();
                    app.link = Link::Up;
                    crate::kern::runner::spawn_row_tail(Arc::clone(&h), tx.clone(), h.kernel.rows.next_id() - 1);
                    crate::kern::runner::spawn_row_watch(Arc::clone(&h), tx.clone()).await;
                    crate::kern::runner::subscribe_state(&h, tx.clone());
                    // a host bundle's folder changed → it takes effect at once
                    crate::kern::bundle_watch::spawn(Arc::clone(&h), paths.store.clone());
                    // each terminal's last launched command (`kern/term_watch`)
                    crate::kern::term_watch::spawn(Arc::clone(&h));
                    cmds = Some(crate::kern::runner::spawn(Arc::clone(&h), tx.clone()));
                    if boot.canvas {
                        match crate::kern::web::ensure(&h, boot.port, boot.bind.as_deref()).await {
                            Ok(urls) => {
                                boot.say(&format!("fantastic: the canvas: {}", urls.canvas));
                                app.note(format!("the canvas: {}", urls.canvas));
                                if let Err(e) = crate::kern::web::write_run_file(&paths.cwd, &urls) {
                                    app.note(format!("the run file: {e}"));
                                }
                                if let Err(e) = crate::kern::user_store::set_canvas(&h, &urls.canvas).await {
                                    app.note(e);
                                }
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
                    // THE GRANT WATCH, once the boot's own grant writes (the canvas's
                    // above) are done; `ai` and `run` below wait on its grants
                    if let Err(e) = crate::kern::reach::start(&h).await {
                        app.note(e);
                    }
                    // `ai BACKEND …` and `run COMMAND…`: with or without the canvas. One
                    // that cannot be done is a loud stop, like no page.
                    if let Some(b) = &boot.ai {
                        match crate::kern::root_ai::run(&h, b).await {
                            Ok(said) => {
                                boot.say(&format!("fantastic: {said}"));
                                app.note(said);
                            }
                            Err(e) => {
                                boot.say(&format!("fantastic: ai: {e}"));
                                app.note(format!("ai: {e}"));
                                failed();
                                app.quit = true;
                            }
                        }
                    }
                    if let Some(argv) = &boot.run {
                        let r = if boot.canvas {
                            crate::kern::terminal::canvas_cell(&h, argv).await.map(|c| match c {
                                Some(cell) => format!("`{}` runs in the canvas cell `{cell}`", argv.join(" ")),
                                None => "the canvas has no view yet — open it once and run again".to_string(),
                            })
                        } else {
                            crate::kern::terminal::prepare_run(&h, argv).await.map(|()| format!("`{}` runs in the terminal", argv.join(" ")))
                        };
                        match r {
                            Ok(said) => {
                                boot.say(&format!("fantastic: {said}"));
                                app.note(said);
                            }
                            Err(e) => {
                                boot.say(&format!("fantastic: run: {e}"));
                                app.note(format!("run: {e}"));
                                failed();
                                app.quit = true;
                            }
                        }
                    }
                    // HEADLESS: the kernel's message stream is the whole output.
                    if boot.announce.is_some() && !boot.tui {
                        spawn_stream(Arc::clone(&h), boot.clone());
                    }
                    // THE TUI SOCKET: a second `fantastic` here is a thin client
                    // of THIS ViewModel.
                    match crate::share::listen(input_tx.clone(), frames.subscribe(), boot.tui) {
                        Ok(s) => share = Some(s),
                        Err(e) => {
                            boot.say(&format!("fantastic: {e}"));
                            app.note(e);
                        }
                    }
                    // THE MCP SOCKET: a coding CLI's `fantastic mcp` attaches here.
                    if let Err(e) = crate::mcp::listen(crate::mcp::Served::new(Arc::clone(&h))) {
                        boot.say(&format!("fantastic: {e}"));
                        app.note(e);
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

        tokio::select! {
            biased;

            _ = term_sig.recv() => app.quit = true,
            _ = int_sig.recv() => app.quit = true,

            // NO IDLE WAKEUPS (owner 2026-10-08): a frame only while a View is
            // attached and something is dirty or animating; the tick (the caret,
            // presence) only while a View is attached. Headless, nothing ticks.
            _ = frame.tick(), if !sizes.is_empty() && (app.dirty || app.animating()) => {
                let anim_due = app.animating() && last_drawn.elapsed() >= ANIM_FRAME;
                if !sizes.is_empty() && (app.dirty || anim_due) {
                    last_drawn = std::time::Instant::now();
                    terms.retain(|v, _| sizes.contains_key(v));
                    slots.retain(|v, _| sizes.contains_key(v));
                    // the authoritative View LAST, so what a key acts on between
                    // passes is the latest View's
                    let mut order: Vec<ViewId> = sizes.keys().copied().filter(|v| Some(*v) != latest).collect();
                    order.extend(latest.filter(|v| sizes.contains_key(v)));
                    let mut by_view = std::collections::BTreeMap::new();
                    for view in order {
                        let (w, h) = sizes[&view];
                        let t = terms
                            .entry(view)
                            .or_insert_with(|| Terminal::new(TestBackend::new(w, h)).expect("off-screen terminal"));
                        if t.size().map(|s| (s.width, s.height)).ok() != Some((w, h)) {
                            t.backend_mut().resize(w, h);
                            let _ = t.clear();
                        }
                        app.enter(slots.remove(&view).unwrap_or_default());
                        crate::ui::layout(&mut app, ratatui::layout::Rect::new(0, 0, w, h), latest == Some(view));
                        let buf = t
                            .draw(|f| crate::ui::draw(f, &app))
                            .expect("the off-screen backend cannot fail")
                            .buffer
                            .clone();
                        slots.insert(view, app.leave());
                        let b = t.backend();
                        let cursor = b.cursor_visible().then(|| {
                            let p = b.cursor_position();
                            (p.x, p.y)
                        });
                        by_view.insert(view, Arc::new(Frame { buf, cursor, mouse: app.mouse }));
                    }
                    frames.send_replace(Some(Arc::new(Frames { by_view })));
                    app.dirty = false;
                    dispatch(&mut app, &cmds);
                }
            }

            maybe = input.recv() => match maybe {
                Some(VmIn::Event(view, e)) => {
                    if !matches!(e, crossterm::event::Event::FocusLost | crossterm::event::Event::FocusGained) && latest != Some(view) {
                        latest = Some(view);
                        app.dirty = true;
                    }
                    // a key acts on what THIS View shows: its own derived state
                    // and layout first, kept again after
                    app.enter(slots.remove(&view).unwrap_or_default());
                    if let Some(&(w, h)) = sizes.get(&view) {
                        crate::ui::layout(&mut app, ratatui::layout::Rect::new(0, 0, w, h), latest == Some(view));
                    }
                    app.remote = view != LOCAL;
                    app.apply(Ev::Term(e));
                    app.pump();
                    app.remote = false;
                    slots.insert(view, app.leave());
                    if std::mem::take(&mut app.detach) {
                        if let Some(s) = &share {
                            s.detach(view, "detached; the kernel keeps running");
                        }
                    }
                    dispatch(&mut app, &cmds);
                }
                Some(VmIn::Resize(view, w, h)) => {
                    let first = sizes.is_empty();
                    sizes.insert(view, (w, h));
                    latest = Some(view);
                    app.dirty = true;
                    // the first View catches up on what went by unread
                    if first {
                        app.pump();
                        dispatch(&mut app, &cmds);
                    }
                }
                Some(VmIn::Gone(view)) => {
                    sizes.remove(&view);
                    if latest == Some(view) {
                        latest = sizes.contains_key(&LOCAL).then_some(LOCAL);
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
                // what those events made due is read now — only for a View to
                // draw: headless, nothing is shown, so nothing is read (each read
                // is a row-store scan that keystrokes would queue behind); the
                // stamps the events cleared stay cleared until a View attaches
                if !sizes.is_empty() {
                    app.pump();
                }
                dispatch(&mut app, &cmds);
            }

            _ = tick.tick(), if !sizes.is_empty() => {
                app.on_tick();
                dispatch(&mut app, &cmds);
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
        crate::mcp::close_socket();
        crate::share::close();
        crate::kern::web::remove_run_file(&paths.cwd);
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
