//! `fantastic-tui` — the ratatui terminal UI for the product.
//!
//! A chat **per agent**, and you tab between them ([`chat::Tabs`]). Each
//! addressee owns its own chat: `@ai`/`@brain` streams an AI turn; `@<agent>`
//! reflects it; `@<agent> <verb> [k=v…]` sends a sugar command and opens that
//! agent's chat; `@sh`/`@ws` are the shell + workspace agents. The input is a smart
//! `@sender` field ([`chat::Composer`]): Tab-complete, Shift-Tab to turn between
//! agents, Backspace into the sender, **nogo** on an unknown agent. Per-source
//! colored rails keep agents distinct; AI turns stream live, queue, and Ctrl+C
//! interrupts.
//!
//! Two facilities live INSIDE the chat (no modes):
//! - **Terminal**: `@sh <cmd>` runs a real PTY (`$SHELL`) as a breathing
//!   viewport below the transcript. **Ctrl+F** focuses the PTY for full
//!   interactivity (vim/htop/…); Esc or Ctrl+F releases focus back to chat.
//! - **Intro**: `/intro` plays a scripted retro "movie" (see `movie.rs`); any
//!   key stops it and returns to chat.

use std::collections::VecDeque;
use std::io::{self};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use fantastic_kernel::{AgentId, Kernel};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::{Paragraph, Wrap};
use serde_json::{json, Value};
use tokio::sync::mpsc;

mod bg;
mod browser;
mod chat;
mod hub;
mod input;
mod kernels;
mod movie;
use chat::{Body, Composer, Route, State, Tabs, WsCmd};
use fantastic_brain as ai;
use fantastic_host::gateway::{self, KernelHandle};
use fantastic_host::registry;
use fantastic_term::{used_rows, TerminalSession};

/// The client_id the brain emits its streaming events to (our inbox key).
const CLIENT_ID: &str = "fantastic";
/// The brain agent id (kept in sync with `fantastic-brain`).
const BRAIN_ID: &str = "brain";
/// The base "characters" present at boot, in tab order: the brain, the shell, the
/// workspace kernel. Any agent you address opens its own agent after these.
const BASE_TABS: [&str; 3] = ["ai", "sh", "ws"];

/// Collect agent ids from a reflect tree (handles `id` + `children`/`agents`).
fn collect_ids(v: &Value, out: &mut Vec<String>) {
    if let Some(id) = v.get("id").and_then(Value::as_str) {
        let id = id.to_string();
        if !out.contains(&id) {
            out.push(id);
        }
    }
    for key in ["children", "agents"] {
        if let Some(arr) = v.get(key).and_then(Value::as_array) {
            for c in arr {
                collect_ids(c, out);
            }
        }
    }
}

/// Async results from the workspace-gateway tasks, delivered back into the
/// `select!` loop. Spawned tasks own a cloned `KernelHandle` and report here.
enum WsEvent {
    /// A workspace kernel is live (`spawned` = we started it, else attached).
    Attached(KernelHandle, bool),
    /// A reply from a workspace verb / reflect.
    Reply(Value),
    /// A gateway error (attach/spawn/send failed).
    Error(String),
    /// The workspace kernel was asked to shut down.
    Down,
}

/// The runtimes the `+ new` picker offers, in order (cursor indexes this).
const RUNTIMES: [gateway::Runtime; 3] = [
    gateway::Runtime::Rust,
    gateway::Runtime::Python,
    gateway::Runtime::Swift,
];

/// Result of attaching/spawning a folder kernel (from `+ new`, or selecting an
/// existing folder square). Delivered into the `select!` loop.
enum KernelTaskResult {
    /// The folder kernel is live — switch the active kernel to it.
    Attached {
        dir: PathBuf,
        runtime: gateway::Runtime,
        handle: KernelHandle,
        /// True for `+ new` (record it in the registry); false when re-selecting one.
        remember: bool,
    },
    /// Attach/spawn failed — surfaced on the Kernels screen, never a silent failure.
    Failed(String),
}

/// The top-level launch phase (a little game state machine).
///
/// Attract (big title + "press any key") → Minimizing (the title shrinks and docks
/// to the top over ~1s) → Hub (the active kernel's agent-tree lobby) → Chat (a
/// maximized agent chat). Enter descends the ladder; Esc climbs it:
/// Chat →Esc→ Hub →Esc→ Kernels (the list of all kernels, the top rung).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Attract,
    Minimizing,
    Kernels,
    Hub,
    Chat,
}

/// How long the attract→hub minimize animation runs.
const MINIMIZE_SECS: f32 = 1.0;

struct App {
    /// The in-proc `★ RAM` manager kernel — always present (also the `app_home`
    /// source). Chat/hub drive [`App::active`], which defaults to this one.
    kernel: Arc<Kernel>,
    /// Which kernel the chat + hub currently drive (the RAM kernel, or a folder
    /// kernel selected from the Kernels screen). Defaults to `InProc(kernel)`.
    active: kernels::ActiveKernel,
    /// One chat **per agent**: `@ai`/`@sh`/`@ws` + any agent you address, each
    /// its own chat; Shift-Tab turns between them.
    tabs: Tabs,
    /// The smart input line — an editable `@sender` field + the message.
    composer: Composer,
    /// The active input control — `Chat` normally, or a setup-flow Select/Field
    /// while `/setup` · `/model` is running.
    control: input::Control,
    /// The `@`-palette selection index (meaningful while the composer is editing
    /// the sender; reset to 0 as the fragment changes).
    palette_idx: usize,
    /// True while the `/help` overlay is up (any key closes it).
    help_open: bool,
    /// Transcript scrollback: VISUAL rows scrolled up from the bottom (0 = follow
    /// the newest). PageUp/PageDown page it, the mouse wheel nudges it; any
    /// submit / agent switch snaps back to the bottom.
    scroll: usize,
    /// The max meaningful `scroll` for the active agent, computed at render (the
    /// handler clamps against it — a `Cell` because render sees `&App`).
    scroll_max: std::cell::Cell<usize>,
    /// The running connector-setup wizard, if any.
    flow: Option<input::SetupFlow>,
    /// Kernel agent ids (cached at boot) — the extra "characters" you can address
    /// + Tab-complete beyond the base agents.
    known_agents: Vec<String>,
    /// AI turns typed while a turn is in flight queue here and fire in order.
    /// Each entry carries the transcript seq of its rendered line (shown dim
    /// + `⏳` while queued) so the drain can flip it to `Done`.
    pending: VecDeque<(u64, String)>,
    /// The final AI-turn result is sent here from the spawned send task.
    ai_tx: mpsc::UnboundedSender<String>,
    /// The live phase of the in-flight AI turn (sending → thinking → generating),
    /// shown as a status indicator until tokens stream in. `None` when idle.
    ai_phase: Option<String>,
    chat_busy: bool,
    /// The live out-of-process workspace kernel (over the loopback gateway), if
    /// one has been brought `up`. `None` until an explicit `@ws up`.
    workspace: Option<KernelHandle>,
    /// True while a workspace gateway task (attach/spawn) is in flight.
    ws_busy: bool,
    /// Async results from the workspace gateway tasks flow back here.
    ws_tx: mpsc::UnboundedSender<WsEvent>,
    /// Reply channel for one-shot kernel commands (reflect / sugar verbs).
    cmd_tx: mpsc::UnboundedSender<(String, String)>,
    /// PTY-output repaint ping sender (used to lazily spawn the PTY for `@sh`).
    redraw_tx: mpsc::UnboundedSender<()>,
    /// The PTY grid the chat viewport should request (cols, max rows).
    chat_term_grid: (u16, u16),
    /// True once the brain has been provisioned (so we only ensure it once).
    brain_ready: bool,
    /// The shared live PTY (`$SHELL`), spawned at startup.
    term: Option<TerminalSession>,
    /// Render the live PTY as a breathing viewport below the transcript (set
    /// once `@sh` runs a command in this session).
    term_active: bool,
    /// While true, keystrokes are encoded straight to the PTY (full
    /// interactivity); Esc / Ctrl+F release focus back to the chat input. Only
    /// meaningful when a terminal viewport is active.
    term_focused: bool,
    /// The scripted intro movie + when it (re)started (for its frame clock).
    movie: movie::Movie,
    /// True while the intro movie is playing (the 10s-idle auto-demo, or a
    /// manual `/intro`); any key stops it. `intro_since` is its frame clock.
    intro_playing: bool,
    intro_since: Option<Instant>,
    /// The launch phase (see [`Phase`]). `Attract` at boot; the first key starts
    /// the minimize animation → the `Hub` lobby. `last_activity` is reset on any
    /// key (the idle clock that drives the 10s-idle → auto-demo); `boot` is the
    /// global animation clock for the grid + title + blink phases.
    phase: Phase,
    /// When the attract→hub minimize animation started (its tween clock), or
    /// `None` when not minimizing.
    minimize_since: Option<Instant>,
    /// The lobby's agent tree (agents = the kernel reflect tree, 1:1). Rebuilt
    /// when `hub_dirty` is set (on entering the hub).
    hub: hub::Hub,
    /// Set when the hub should re-reflect the kernel (agents may have changed);
    /// the tick loop does the async refresh and clears it.
    hub_dirty: bool,
    /// The Kernels screen (the top nav rung): the row of kernel squares + selection.
    kernels_view: kernels::KernelsView,
    /// The persisted list of folder kernels (`<app_home>/kernels.json`).
    registry: registry::Registry,
    /// Set when the Kernels screen should re-reflect-ping its folder kernels (on
    /// entering it, or after creating one); the tick loop does the async refresh.
    kernels_dirty: bool,
    /// The folder browser overlay while creating a new kernel (`+ new`).
    browser: Option<browser::Browser>,
    /// After a folder is chosen, the runtime pick: `(dir, cursor into RUNTIMES)`.
    new_runtime: Option<(PathBuf, usize)>,
    /// True while a kernel attach/spawn task is in flight (blocks re-entry).
    kernels_busy: bool,
    /// A transient status/error line for the Kernels screen (attach failures, etc).
    kernels_msg: Option<String>,
    /// Attach/spawn task results (create-new or selecting a folder kernel).
    kernel_task_tx: mpsc::UnboundedSender<KernelTaskResult>,
    last_activity: Instant,
    boot: Instant,
    /// When the attract screen last (re)appeared — set at boot and reset every
    /// time the cabinet drops back to attract (after a demo pass). Drives the
    /// top→bottom "power-on" reveal of the big FANTASTIC on the attract screen.
    attract_since: Instant,
    quit: bool,
    /// Exit affordances. `last_ctrl_c`: a second Ctrl+C within the window quits
    /// (a single one still reaches the shell in terminal mode). `q_streak`:
    /// count of rapid `q` auto-repeats (holding q) — normal typing resets it.
    last_ctrl_c: Option<Instant>,
    q_streak: u8,
    last_q: Option<Instant>,
    /// True once a `q`-hold has been recognized as an exit gesture (started from
    /// an empty line) — held `q`s are then consumed, not typed. Typing normal
    /// text with q's never arms it.
    q_armed: bool,
}

impl App {
    /// Every addressable character: the open agents (base + spawned tabs) plus the
    /// known kernel agents. Drives Tab-complete, Shift-Tab roll, and the nogo
    /// check on send.
    fn known_senders(&self) -> Vec<String> {
        let mut v: Vec<String> = self.tabs.ids().to_vec();
        for a in &self.known_agents {
            if !v.contains(a) {
                v.push(a.clone());
            }
        }
        v
    }
}

pub async fn run(kernel: Arc<Kernel>, _agent_count: usize) -> Result<()> {
    // One-shot kernel command replies flow back here as (source_id, text).
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<(String, String)>();
    // The final result of an AI turn (the blocking `response`, or `✗ error`) — the
    // authoritative turn boundary. Renders into the `@ai` chat + drives the queue.
    let (ai_tx, mut ai_rx) = mpsc::unbounded_channel::<String>();
    // Workspace gateway task results (attach/spawn/send/down) flow back here.
    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<WsEvent>();
    // Kernel attach/spawn task results (the Kernels screen: `+ new` / select folder).
    let (kernel_task_tx, mut kernel_task_rx) = mpsc::unbounded_channel::<KernelTaskResult>();
    // PTY output → repaint pings.
    let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<()>();

    // The brain streams its per-turn events (token/say/status/done) to the
    // `CLIENT_ID` inbox. Register a bounded channel matching `kernel.inboxes`'
    // Sender type and drain it into the transcript. A generous bound so a fast
    // token BURST isn't dropped on `try_send` before the ~16fps loop drains it
    // (a dropped `done` would hang the turn).
    let (brain_tx, mut brain_rx) = tokio::sync::mpsc::channel::<Value>(8192);
    kernel.inboxes.insert(AgentId::from(CLIENT_ID), brain_tx);

    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Event>();
    std::thread::spawn(move || {
        while let Ok(ev) = event::read() {
            if in_tx.send(ev).is_err() {
                break;
            }
        }
    });

    enable_raw_mode()?;
    let mut out = io::stdout();
    // NOTE: mouse capture is deliberately LEFT OFF so the terminal keeps its native
    // selection — you can drag-select the chat and copy (Cmd/Ctrl+C in the terminal),
    // and paste (Cmd/Ctrl+V) arrives as a bracketed-paste event. Scroll history with
    // PageUp/PageDown; fly the hub with the arrows.
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    // Spawn the shared PTY sized to the chat breathing-viewport grid.
    let (trows, tcols) = chat_term_grid(&term);
    let session = TerminalSession::spawn(trows, tcols, redraw_tx.clone()).ok();

    // The active kernel — starts as the in-proc RAM kernel, so everything below is
    // byte-identical to before until you switch to a folder kernel.
    let active = kernels::ActiveKernel::InProc(Arc::clone(&kernel));
    // Cache the active kernel's agent ids as addressable "characters".
    let known_agents = active.agent_ids().await;
    // The lobby tree — the active kernel's live agent hierarchy.
    let hub = hub::Hub::from_reflect(&active.reflect_tree().await);
    // The persisted folder-kernel list → seed the Kernels screen (offline until the
    // first reflect-ping when you enter it).
    let registry = registry::Registry::load();
    let mut kernels_view = kernels::KernelsView::default();
    kernels_view.rebuild(
        registry
            .entries
            .iter()
            .map(|e| (e.dir.clone(), e.runtime, false))
            .collect(),
    );

    let mut app = App {
        kernel,
        active,
        tabs: Tabs::new(&BASE_TABS),
        composer: Composer::new("ai"),
        control: input::Control::Chat,
        palette_idx: 0,
        help_open: false,
        scroll: 0,
        scroll_max: std::cell::Cell::new(0),
        flow: None,
        known_agents,
        pending: VecDeque::new(),
        ai_tx,
        ai_phase: None,
        chat_busy: false,
        workspace: None,
        ws_busy: false,
        ws_tx,
        cmd_tx,
        redraw_tx: redraw_tx.clone(),
        chat_term_grid: (tcols, trows),
        brain_ready: false,
        term: session,
        term_active: false,
        term_focused: false,
        movie: movie::Movie::storyboard(),
        intro_playing: false,
        intro_since: None,
        phase: Phase::Attract,
        minimize_since: None,
        hub,
        hub_dirty: false,
        kernels_view,
        registry,
        kernels_dirty: false,
        browser: None,
        new_runtime: None,
        kernels_busy: false,
        kernels_msg: None,
        kernel_task_tx,
        last_activity: Instant::now(),
        boot: Instant::now(),
        attract_since: Instant::now(),
        quit: false,
        last_ctrl_c: None,
        q_streak: 0,
        last_q: None,
        q_armed: false,
    };
    term.draw(|f| ui(f, &app))?;

    // ~16fps heartbeat — the title reveal/minimize tween, live token streaming,
    // and the blinking attract prompt all animate, so this fires CONTINUOUSLY and
    // every tick falls through to a redraw. It's a cabinet: accept the steady
    // repaint. The tick also drives the attract state machine (10s-idle →
    // auto-demo, demo end → attract).
    let mut ticker = tokio::time::interval(Duration::from_millis(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(ev) = in_rx.recv() => handle_input(&mut app, ev),
            Some((from, text)) = cmd_rx.recv() => {
                // A kernel reply lands in that agent's chat (unread if you're away).
                app.tabs.deliver(&from).push(&from, "you", Body::Text(text), State::Done);
            }
            Some(ev) = brain_rx.recv() => {
                // LIVE EVENT STREAM, shared by `@ai` AND `@<id>` commands. Any agent
                // we `watch` mirrors its self-emitted events here; each carries a
                // `source` naming the emitter. We render it live into THAT source's
                // agent via the same `on_event` (token append · say/activity note ·
                // done seal). The brain (`source == BRAIN_ID`) additionally drives the
                // AI turn lifecycle (phase indicator + queue drain on the ordered
                // `done`); command targets just render — their final reply arrives via
                // `cmd_rx` and there is no turn queue to drain.
                let ty = ev.get("type").and_then(Value::as_str).unwrap_or("");
                let source = ev
                    .get("source")
                    .and_then(Value::as_str)
                    .unwrap_or(BRAIN_ID)
                    .to_string();
                let is_ai = source == BRAIN_ID;
                // Routing: the brain renders in the `ai` chat; every other agent
                // renders in the chat named by its id (the `@<id>` chat).
                let agent = if is_ai { "ai" } else { source.as_str() };
                if is_ai && ty == "status" {
                    if let Some(p) = ev.get("phase").and_then(Value::as_str) {
                        app.ai_phase = Some(p.to_string());
                    }
                }
                app.tabs.deliver(agent).on_event(&ev);
                if is_ai && ty == "done" {
                    finish_ai_turn(&mut app);
                }
            }
            Some(out) = ai_rx.recv() => {
                // ERROR path only: a turn that failed produced no `done` event (the
                // send returned an error), so the spawned task sends the error/dry
                // text here. Seal the live line with it and end the turn — a raw
                // `✗ …` seals red (Error); dry guidance seals as a normal line.
                let state = if out.starts_with('✗') { State::Error } else { State::Done };
                app.tabs.deliver("ai").close_stream_with(BRAIN_ID, &out, state);
                finish_ai_turn(&mut app);
            }
            Some(ev) = ws_rx.recv() => handle_ws_event(&mut app, ev),
            Some(res) = kernel_task_rx.recv() => {
                app.kernels_busy = false;
                match res {
                    KernelTaskResult::Attached { dir, runtime, handle, remember } => {
                        if remember {
                            app.registry.add(dir.clone(), runtime);
                            let _ = app.registry.save();
                        }
                        let label = dir
                            .file_name()
                            .map(|s| s.to_string_lossy().into_owned())
                            .unwrap_or_else(|| dir.to_string_lossy().into_owned());
                        app.active = kernels::ActiveKernel::Remote(kernels::RemoteKernel {
                            handle,
                            label,
                        });
                        app.known_agents = app.active.agent_ids().await;
                        app.kernels_msg = None;
                        app.brain_ready = false; // a fresh kernel — re-provision on first @ai
                        app.hub_dirty = true;
                        enter_hub(&mut app);
                    }
                    KernelTaskResult::Failed(e) => {
                        app.kernels_msg = Some(format!("✗ {e}"));
                    }
                }
            }
            Some(()) = redraw_rx.recv() => {}
            _ = ticker.tick() => {
                // The animated bg needs a continuous repaint; we always fall
                // through to `term.draw` below. Drive the attract machine here.
                let now = Instant::now();
                let idle = app.last_activity.elapsed().as_secs_f32();
                let intro_elapsed = app
                    .intro_since
                    .map(|t| t.elapsed().as_secs_f32())
                    .unwrap_or(0.0);
                match attract_tick(
                    app.phase != Phase::Attract,
                    app.intro_playing,
                    idle,
                    intro_elapsed,
                    app.movie.total_secs(),
                ) {
                    AttractTick::StartIntro => {
                        app.intro_playing = true;
                        app.intro_since = Some(now);
                    }
                    AttractTick::EndIntroToAttract => {
                        app.intro_playing = false;
                        app.intro_since = None;
                        // Reset the idle clock so attract waits another 10s.
                        app.last_activity = now;
                        // Re-arm the title reveal so it re-appears top→bottom.
                        app.attract_since = now;
                    }
                    AttractTick::Nothing => {}
                }
                // The minimize animation completes → the hub lobby opens.
                if app.phase == Phase::Minimizing {
                    let t = app
                        .minimize_since
                        .map(|s| s.elapsed().as_secs_f32())
                        .unwrap_or(1.0);
                    if t >= MINIMIZE_SECS {
                        app.phase = Phase::Hub;
                        app.minimize_since = None;
                        app.hub_dirty = true;
                    }
                }
                // Refresh the lobby's agent tree (async reflect) when it's dirty —
                // entering the hub, or after opening an agent that may have spawned
                // more. Keep the selection index in range across the rebuild.
                if app.hub_dirty && app.phase == Phase::Hub {
                    app.hub_dirty = false;
                    let sel = app.hub.sel;
                    app.hub = hub::Hub::from_reflect(&app.active.reflect_tree().await);
                    app.hub.sel = sel.min(app.hub.nodes().len().saturating_sub(1));
                }
                // Reflect-ping each registry folder kernel (live/offline) when the
                // Kernels screen is up and dirty — entering it, or after creating one.
                if app.kernels_dirty && app.phase == Phase::Kernels {
                    app.kernels_dirty = false;
                    let mut folders = Vec::new();
                    for e in &app.registry.entries {
                        let live = gateway::Workspace::new(e.dir.clone())
                            .attach()
                            .await
                            .ok()
                            .flatten()
                            .is_some();
                        folders.push((e.dir.clone(), e.runtime, live));
                    }
                    app.kernels_view.rebuild(folders);
                }
            }
            else => break,
        }
        if app.quit {
            break;
        }
        // Keep the PTY grid matched to the breathing chat viewport: it gets the
        // full chat-body height (so htop/vim get real agent) while only
        // `used_rows` of it are displayed below the transcript.
        app.chat_term_grid = chat_term_grid(&term);
        if app.term_active {
            let (r, c) = app.chat_term_grid;
            if let Some(ts) = app.term.as_mut() {
                ts.resize(r, c);
            }
        }
        term.draw(|f| ui(f, &app))?;
    }

    disable_raw_mode()?;
    execute!(
        term.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    )?;
    term.show_cursor()?;
    Ok(())
}

/// Window in which a SECOND Ctrl+C counts as "exit the app".
const DOUBLE_PRESS_MS: u64 = 1000;
/// Inter-key gap below which consecutive `q` presses count as a hold (the
/// cadence of terminal key auto-repeat).
const Q_REPEAT_MS: u64 = 220;
/// Number of rapid `q` repeats that trigger exit (≈ holding the key down).
const Q_HOLD_STREAK: u8 = 6;

/// True when a Ctrl+C at `now` should exit — i.e. a prior Ctrl+C (`last`) lands
/// inside the double-press window. Pure: caller owns the state + the `now` clock.
fn ctrl_c_exits(last: Option<Instant>, now: Instant) -> bool {
    last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(DOUBLE_PRESS_MS))
}

/// The next `q`-hold streak given the previous streak + last-`q` time. A press
/// within `Q_REPEAT_MS` of the previous extends the run (auto-repeat = holding);
/// a slower press restarts at 1. Caller exits once the result hits
/// `Q_HOLD_STREAK`. Pure: caller owns the state + the `now` clock.
fn q_hold_streak(prev: u8, last: Option<Instant>, now: Instant) -> u8 {
    let fast = last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(Q_REPEAT_MS));
    if fast {
        prev.saturating_add(1)
    } else {
        1
    }
}

/// Seconds the cabinet sits idle on the attract screen before it auto-plays the
/// intro demo (arcade "attract mode").
const ATTRACT_IDLE_SECS: f32 = 10.0;

/// The attract state-machine decision for one tick. Pure + unit-tested.
#[derive(Debug, PartialEq, Eq)]
enum AttractTick {
    /// On the attract screen and idle ≥ 10s → kick off the auto-demo.
    StartIntro,
    /// The auto-demo finished one full pass → drop back to attract (it loops).
    EndIntroToAttract,
    /// Nothing to do this tick.
    Nothing,
}

/// Decide what the attract loop should do this tick. Only acts while the game
/// hasn't `started` (the user is on the attract screen, not in chat):
/// - not started, not playing, idle ≥ 10s → `StartIntro`.
/// - not started, playing, the demo ran its full length → `EndIntroToAttract`.
/// - otherwise `Nothing`.
///
/// Once `started` (in chat), this is always `Nothing` — a manual `/intro` inside
/// chat is governed by the movie's own loop, not the attract machine.
fn attract_tick(
    started: bool,
    intro_playing: bool,
    idle_secs: f32,
    intro_elapsed: f32,
    movie_total: f32,
) -> AttractTick {
    if started {
        return AttractTick::Nothing;
    }
    if intro_playing {
        if intro_elapsed >= movie_total {
            AttractTick::EndIntroToAttract
        } else {
            AttractTick::Nothing
        }
    } else if idle_secs >= ATTRACT_IDLE_SECS {
        AttractTick::StartIntro
    } else {
        AttractTick::Nothing
    }
}

/// True when a breathing terminal viewport is live (so Ctrl+F focus + PTY
/// SIGINT routing are meaningful).
fn term_live(app: &App) -> bool {
    app.term_active && app.term.is_some()
}

/// True when the terminal viewport is BOTH live and on-screen — i.e. `@sh` is the
/// active agent. The PTY is shared, but it must only render + capture keys in its
/// own chat, never bleed into the `@ai`/`@ws`/other agent chats.
fn term_visible(app: &App) -> bool {
    term_live(app) && app.tabs.active_id() == "sh"
}

fn handle_input(app: &mut App, ev: Event) {
    // Bracketed paste arrives as ONE event — insert it whole (embedded newlines
    // preserved as literal breaks in the message) instead of replaying it as
    // keystrokes, where each newline would fire Enter and submit fragments.
    if let Event::Paste(text) = &ev {
        app.last_activity = Instant::now();
        if app.phase != Phase::Chat {
            return; // only the composer/agent takes paste
        }
        if app.term_focused {
            if let Some(ts) = app.term.as_mut() {
                ts.write(text.as_bytes());
            }
            return;
        }
        if !matches!(app.control, input::Control::Chat) {
            // A setup-flow field: paste the text flat (no newlines in a model
            // name / API key).
            for c in text.chars().filter(|c| !c.is_control()) {
                app.control.type_char(c);
            }
            return;
        }
        for c in text.chars() {
            if c == '\n' {
                if app.composer.editing_sender {
                    app.composer.type_char(' '); // commit the sender, like a space
                } else {
                    app.composer.insert_char('\n');
                }
            } else if c != '\r' {
                app.composer.type_char(c);
            }
        }
        return;
    }
    // Mouse events are NOT captured (native terminal selection/copy is preserved),
    // so none arrive here — history scrolls with PageUp/PageDown, the hub with arrows.
    let Event::Key(KeyEvent {
        code,
        kind,
        modifiers,
        ..
    }) = ev
    else {
        return;
    };
    if kind != KeyEventKind::Press {
        return;
    }
    // Any key press is activity — reset the idle clock that drives attract mode.
    app.last_activity = Instant::now();
    // Arcade "press any key": on the attract screen (or during the auto-demo)
    // the FIRST key starts the minimize animation → the hub lobby. During the
    // minimize, a key skips straight to the hub. Fully consumed — never leaks.
    if app.intro_playing {
        app.intro_playing = false;
        app.intro_since = None;
        if app.phase == Phase::Attract {
            start_minimize(app);
        }
        return;
    }
    match app.phase {
        Phase::Attract => {
            start_minimize(app);
            return;
        }
        Phase::Minimizing => {
            enter_hub(app); // skip the animation
            return;
        }
        Phase::Kernels | Phase::Hub | Phase::Chat => {}
    }
    // The /help overlay: any key dismisses it (fully consumed).
    if app.help_open {
        app.help_open = false;
        return;
    }
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl-Q is the always-reliable quit (works even while the PTY is focused).
    if ctrl && code == KeyCode::Char('q') {
        app.quit = true;
        return;
    }
    // Ctrl+F toggles PTY focus (only when the terminal viewport is on-screen — in
    // the `@sh` agent). Elsewhere it says why nothing happened.
    if ctrl && code == KeyCode::Char('f') {
        if term_visible(app) {
            app.term_focused = !app.term_focused;
        } else {
            app.tabs.active_mut().push(
                "system",
                "you",
                Body::Note(
                    "no shell here — `@sh <cmd>` opens one (Ctrl+F focuses it in @sh)".into(),
                ),
                State::Done,
            );
        }
        return;
    }
    // Ctrl+C: a SECOND press within the window exits the app. A single press
    // still does its normal job: with a live terminal it's forwarded to the
    // shell as SIGINT (0x03); otherwise it interrupts an in-flight AI stream.
    if ctrl && code == KeyCode::Char('c') {
        let now = Instant::now();
        if ctrl_c_exits(app.last_ctrl_c, now) {
            app.quit = true;
            return;
        }
        app.last_ctrl_c = Some(now);
        let ai_live = app.tabs.chat("ai").is_some_and(|c| c.has_live());
        // SIGINT goes to the shell only when its viewport is the one in front
        // (the `@sh` agent / focused); elsewhere Ctrl+C interrupts the AI stream.
        // The note says what THIS press did, so the action is never a mystery.
        let note = if app.term_focused || term_visible(app) {
            if let Some(ts) = app.term.as_mut() {
                ts.write(&[0x03]);
            }
            "^C sent to shell · Ctrl+C again to exit".to_string()
        } else if ai_live {
            app.tabs.deliver("ai").interrupt_live();
            // Queued lines will never fire now — mark them interrupted too.
            for (seq, _) in app.pending.drain(..) {
                app.tabs.deliver("ai").set_state(seq, State::Interrupted);
            }
            app.chat_busy = false;
            let kernel = Arc::clone(&app.kernel);
            tokio::spawn(async move {
                kernel
                    .send(&AgentId::from(BRAIN_ID), json!({"type":"interrupt"}))
                    .await;
            });
            "⊘ interrupted @ai · Ctrl+C again to exit".to_string()
        } else {
            "press Ctrl+C again to exit".to_string()
        };
        app.tabs
            .active_mut()
            .push("system", "you", Body::Note(note), State::Done);
        return;
    }
    // The HUB lobby owns the keys while it's up: arrows fly the agent tree
    // (↑ parent · ↓ child · ←/→ siblings), Enter opens the selected agent's chat,
    // Esc/`/` no-op. (Ctrl+Q / double-Ctrl+C above still exit from here.)
    if app.phase == Phase::Hub {
        match code {
            KeyCode::Up => app.hub.move_up(),
            KeyCode::Down => app.hub.move_down(),
            KeyCode::Left => app.hub.move_left(),
            KeyCode::Right => app.hub.move_right(),
            KeyCode::Enter => {
                let agent = app.hub.selected_agent();
                open_agent(app, &agent);
            }
            // Esc climbs one more rung: Hub → Kernels (the list of all kernels).
            KeyCode::Esc => {
                app.phase = Phase::Kernels;
                app.kernels_dirty = true; // re-reflect-ping the folder kernels
            }
            _ => {}
        }
        return;
    }
    // The "+ new kernel" overlay owns keys while it's up: first the folder browser,
    // then the runtime pick. (Global exits Ctrl+Q / double-Ctrl+C above still work.)
    if app.browser.is_some() {
        match code {
            KeyCode::Up => app.browser.as_mut().unwrap().up(),
            KeyCode::Down => app.browser.as_mut().unwrap().down(),
            KeyCode::Enter => app.browser.as_mut().unwrap().enter(),
            // Space = "use THIS folder" → advance to the runtime pick.
            KeyCode::Char(' ') => {
                let dir = app.browser.as_ref().unwrap().choose();
                app.browser = None;
                app.new_runtime = Some((dir, 0));
            }
            KeyCode::Esc => app.browser = None,
            _ => {}
        }
        return;
    }
    if app.new_runtime.is_some() {
        match code {
            KeyCode::Up => {
                if let Some((_, c)) = app.new_runtime.as_mut() {
                    *c = c.saturating_sub(1);
                }
            }
            KeyCode::Down => {
                if let Some((_, c)) = app.new_runtime.as_mut() {
                    if *c + 1 < RUNTIMES.len() {
                        *c += 1;
                    }
                }
            }
            KeyCode::Enter => {
                if let Some((dir, c)) = app.new_runtime.take() {
                    spawn_attach(app, dir, RUNTIMES[c], true);
                }
            }
            KeyCode::Esc => app.new_runtime = None,
            _ => {}
        }
        return;
    }
    // The KERNELS lobby (top rung): ←/→ pick a kernel square, Enter opens it (its
    // Hub) or the `+ new` browser, Esc stays (already home; Ctrl+Q quits).
    if app.phase == Phase::Kernels {
        match code {
            KeyCode::Left => app.kernels_view.move_left(),
            KeyCode::Right => app.kernels_view.move_right(),
            KeyCode::Enter => select_square(app),
            _ => {}
        }
        return;
    }
    // Hold `q` to exit: physically holding the key fires rapid auto-repeats; a
    // run of them STARTED FROM AN EMPTY LINE quits. The gesture ARMS on the 2nd
    // fast repeat (the first typed `q` is reclaimed, a hint shows); once armed,
    // held q's are consumed, not typed. Typing normal text with q's never arms —
    // no more quitting mid-word. SUPPRESSED while the PTY is focused, where a
    // typed `q` must reach the shell (only Ctrl+Q / double-Ctrl+C exit).
    if let KeyCode::Char('q') = code {
        let in_chat = matches!(app.control, input::Control::Chat);
        if !ctrl && !app.term_focused && in_chat && !app.composer.editing_sender {
            let now = Instant::now();
            app.q_streak = q_hold_streak(app.q_streak, app.last_q, now);
            app.last_q = Some(now);
            if app.q_streak == 1 {
                app.q_armed = false; // fresh press — arms only if a hold follows
            }
            if app.q_streak >= 2 && !app.q_armed && app.composer.message == "q" {
                // A hold beginning on an empty line: reclaim the first typed q,
                // consume the run, and hint at what's happening.
                app.q_armed = true;
                app.composer.backspace();
                app.tabs.active_mut().push(
                    "system",
                    "you",
                    Body::Note("hold q to exit…".into()),
                    State::Done,
                );
            }
            if app.q_armed {
                if app.q_streak >= Q_HOLD_STREAK {
                    app.quit = true;
                }
                return; // consume the held q — it is a gesture, not text
            }
        }
    } else {
        app.q_streak = 0;
        app.q_armed = false;
    }
    if app.term_focused {
        // Full interactivity: encode the key straight to the PTY. Esc or Ctrl+F
        // (handled above) release focus back to the chat input.
        if code == KeyCode::Esc {
            app.term_focused = false;
            return;
        }
        if let Some(ts) = app.term.as_mut() {
            if let Some(bytes) = encode_key(code, modifiers) {
                ts.write(&bytes);
            }
        }
        return;
    }
    // A setup flow (`/setup` · `/model`) owns the input while active — its dynamic
    // control (Select / Field) handles the keys instead of the composer.
    if !matches!(app.control, input::Control::Chat) {
        handle_flow_key(app, code);
        return;
    }
    // While the `@sender` is being edited, the palette owns the navigation keys:
    // ↑/↓ move the selection, Enter picks + submits the bare line (enter the
    // agent; a fresh agent opens on its reflect — the introduction), Tab
    // completes to the selection, Esc cancels back to the previous sender.
    if app.composer.editing_sender {
        match code {
            KeyCode::Up => {
                app.palette_idx = app.palette_idx.saturating_sub(1);
                return;
            }
            KeyCode::Down => {
                app.palette_idx = app.palette_idx.saturating_add(1); // clamped at pick/render
                return;
            }
            KeyCode::Enter => {
                if let Some(pick) = palette_pick(app) {
                    app.composer.set_sender(&pick);
                } else {
                    // No candidate — keep the typed fragment; submit nogo-flashes.
                    app.composer.editing_sender = false;
                }
                app.palette_idx = 0;
                submit_chat(app);
                return;
            }
            KeyCode::Tab => {
                if let Some(pick) = palette_pick(app) {
                    app.composer.set_sender(&pick);
                    if app.tabs.ids().contains(&pick) {
                        app.tabs.focus(&pick);
                        app.scroll = 0;
                    }
                } else {
                    app.composer.complete(&app.known_senders());
                }
                app.palette_idx = 0;
                return;
            }
            KeyCode::Esc => {
                app.composer.cancel_sender_edit();
                app.palette_idx = 0;
                return;
            }
            KeyCode::Char(_) | KeyCode::Backspace => {
                // Fragment is changing — reset the selection, then fall through
                // to the composer edit below.
                app.palette_idx = 0;
            }
            _ => {}
        }
    } else if code == KeyCode::Esc {
        // Esc = minimize back to the lobby. (The other Esc meanings — cancel a
        // flow, release the PTY, cancel a sender edit — were all handled above,
        // so the ladder stays consistent: Esc always steps "back", ending at the
        // hub.)
        enter_hub(app);
        return;
    }
    // Otherwise the keys edit the smart composer (`@sender` field + message):
    // chars insert at the caret, Left/Right/Home/End move it, Up/Down recall
    // sent lines, Delete removes at the caret.
    match code {
        KeyCode::Char(c) => app.composer.type_char(c),
        KeyCode::Backspace => app.composer.backspace(),
        KeyCode::Delete => app.composer.delete(),
        KeyCode::Left => app.composer.move_left(),
        KeyCode::Right => app.composer.move_right(),
        KeyCode::Home => app.composer.move_home(),
        KeyCode::End => {
            // End: caret to the end of the line AND snap the view to the newest.
            app.composer.move_end();
            app.scroll = 0;
        }
        KeyCode::Up => {
            app.composer.recall_up();
        }
        KeyCode::Down => {
            app.composer.recall_down();
        }
        KeyCode::PageUp => {
            app.scroll = (app.scroll + 8).min(app.scroll_max.get());
        }
        KeyCode::PageDown => {
            app.scroll = app.scroll.saturating_sub(8);
        }
        KeyCode::Tab => {
            // Complete the `@sender` against every known character; if it lands on
            // an open agent, turn to face it.
            app.composer.complete(&app.known_senders());
            if app.tabs.ids().contains(&app.composer.sender) {
                app.tabs.focus(&app.composer.sender);
            }
        }
        KeyCode::BackTab => {
            // Shift-Tab: turn to the next OPEN agent (rolls over the characters you
            // know); the composer follows. New agents are reached by name + Tab.
            app.tabs.cycle(true);
            let id = app.tabs.active_id().to_string();
            app.composer.set_sender(&id);
            app.scroll = 0;
        }
        KeyCode::Enter => submit_chat(app),
        _ => {}
    }
}

/// Kick off the attract→hub minimize animation (title shrinks + docks over ~1s).
fn start_minimize(app: &mut App) {
    app.phase = Phase::Minimizing;
    app.minimize_since = Some(Instant::now());
}

/// Open the hub lobby (from a chat via Esc, or skipping the minimize). Marks the
/// agent tree dirty so the tick loop refreshes it.
fn enter_hub(app: &mut App) {
    app.phase = Phase::Hub;
    app.minimize_since = None;
    app.hub_dirty = true;
}

/// Act on the selected Kernels square: `★ RAM` → make the in-proc kernel active and
/// drop into its Hub; a folder kernel → attach it and switch; `+ new` → open the
/// folder browser.
fn select_square(app: &mut App) {
    if app.kernels_busy {
        return; // an attach is already in flight
    }
    // Read what we need first (releases the `kernels_view` borrow before mutating).
    enum Pick {
        Ram,
        Folder(PathBuf, gateway::Runtime),
        New,
    }
    let pick = match app.kernels_view.selected() {
        kernels::Square::Ram => Pick::Ram,
        kernels::Square::Folder { dir, runtime, .. } => Pick::Folder(dir.clone(), *runtime),
        kernels::Square::New => Pick::New,
    };
    match pick {
        Pick::Ram => {
            app.active = kernels::ActiveKernel::InProc(Arc::clone(&app.kernel));
            app.hub_dirty = true;
            enter_hub(app);
        }
        Pick::Folder(dir, rt) => spawn_attach(app, dir, rt, false),
        Pick::New => {
            let start = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
            app.browser = Some(browser::Browser::open(start));
            app.kernels_msg = None;
        }
    }
}

/// Attach (or spawn) the folder kernel at `dir` with `runtime`, in a background
/// task; the result comes back via `kernel_task_tx` → the `select!` loop, which
/// switches the active kernel to it. `remember` records it in the registry.
fn spawn_attach(app: &mut App, dir: PathBuf, runtime: gateway::Runtime, remember: bool) {
    app.kernels_busy = true;
    app.kernels_msg = Some(format!(
        "attaching {} ({})…",
        dir.display(),
        runtime.as_str()
    ));
    let tx = app.kernel_task_tx.clone();
    tokio::spawn(async move {
        let res = match gateway::Workspace::new(dir.clone())
            .attach_or_spawn(runtime)
            .await
        {
            Ok(handle) => KernelTaskResult::Attached {
                dir,
                runtime,
                handle,
                remember,
            },
            Err(e) => KernelTaskResult::Failed(e.to_string()),
        };
        let _ = tx.send(res);
    });
}

/// Open an agent's chat from a hub selection: focus it (creating its tab), point
/// the composer there, and for a not-yet-open agent fire its reflect (the bare-`@`
/// intro) so it isn't empty. Then switch to the Chat phase.
fn open_agent(app: &mut App, agent: &str) {
    app.phase = Phase::Chat;
    app.scroll = 0;
    let base = matches!(agent, "ai" | "sh" | "ws");
    let already_open = app.tabs.ids().iter().any(|t| t == agent);
    app.tabs.focus(agent);
    app.composer.set_sender(agent);
    if !base && !already_open {
        dispatch_kernel(app, AgentId::from(agent), json!({"type":"reflect"}));
    }
}

/// The currently-selected `@`-palette candidate for the composer's fragment
/// (selection clamped into the filtered list). `None` when nothing matches.
fn palette_pick(app: &App) -> Option<String> {
    let items = chat::palette_items(&app.tabs, &app.known_agents, &app.composer.sender);
    if items.is_empty() {
        return None;
    }
    let idx = app.palette_idx.min(items.len() - 1);
    Some(items[idx].id.clone())
}

/// Submit the composed line: validate the `@sender` (a **nogo** if it names no
/// known character), enter that agent, then dispatch — an AI turn streams into the
/// `@ai` agent; a kernel command sends and routes its reply back via `cmd_tx`.
fn submit_chat(app: &mut App) {
    // Submitting re-joins the conversation: snap the view back to the newest.
    app.scroll = 0;
    // Every submitted line is recallable with Up/Down (consecutive dups collapse;
    // a nogo'd line re-recorded on the fixed resubmit collapses too).
    let typed = app.composer.message.trim().to_string();
    if !typed.is_empty() {
        app.composer.record_history(&typed);
    }
    // Local slash commands on the message body (never reach the `@`-router).
    match app.composer.message.trim() {
        "/intro" => {
            app.composer.take_message();
            app.intro_playing = true;
            app.intro_since = Some(Instant::now());
            return;
        }
        "/setup" => {
            app.composer.take_message();
            let (flow, ctrl) = input::SetupFlow::start_setup();
            app.flow = Some(flow);
            app.control = ctrl;
            return;
        }
        "/model" => {
            app.composer.take_message();
            // Pre-fill from the existing connector (or fall back to a full setup).
            let cfg = fantastic_host::ai_config();
            let (flow, ctrl) = match cfg.backend {
                Some(b) => input::SetupFlow::start_model(
                    &b,
                    cfg.model.as_deref().unwrap_or(""),
                    cfg.key_present,
                ),
                None => input::SetupFlow::start_setup(),
            };
            app.flow = Some(flow);
            app.control = ctrl;
            return;
        }
        "/help" => {
            app.composer.take_message();
            app.help_open = true;
            return;
        }
        // Any other `/command` is a command TYPO, not a message — never route it
        // to an agent as text; print the command list instead.
        other if other.starts_with('/') => {
            let cmd = other.to_string();
            app.composer.take_message();
            app.tabs.active_mut().push(
                "system",
                "you",
                Body::Note(format!(
                    "unknown command {cmd} — /help · /intro · /setup · /model"
                )),
                State::Done,
            );
            return;
        }
        _ => {}
    }
    // Nogo: you can't talk to a character who isn't there.
    let known = app.known_senders();
    if !app.composer.is_valid(&known) {
        app.composer.nogo = true;
        return;
    }
    let sender = app.composer.sender.clone();
    let message = app.composer.take_message();
    // Reconstruct the routable line `@sender body` and resolve it.
    let line = format!("@{sender} {message}");
    let (target, route) = chat::route(&line, &sender);
    // Entering the addressed agent (creates its tab if new) + keep the composer
    // pointed there.
    app.tabs.focus(&target);
    app.composer.set_sender(&target);
    match route {
        Route::Empty => {}
        Route::Ai(text) => {
            // The DRY stand-in brain: if no connector is configured, answer with
            // setup guidance instead of provisioning a real brain (which would
            // error). The real brain runs only once a connector is Ready.
            if let Some(guide) = ai::dry_reply(&ai::config_status(), None) {
                app.tabs
                    .deliver("ai")
                    .push("you", "ai", Body::Text(text), State::Done);
                app.tabs
                    .deliver("ai")
                    .push("brain", "you", Body::Text(guide), State::Done);
            } else if app.chat_busy {
                // A turn is in flight — queue this one (rendered dim + ⏳ until
                // the drain fires it on `done`).
                let seq = app.tabs.deliver("ai").push(
                    "you",
                    "ai",
                    Body::Text(text.clone()),
                    State::Queued,
                );
                app.pending.push_back((seq, text));
            } else {
                app.tabs
                    .deliver("ai")
                    .push("you", "ai", Body::Text(text.clone()), State::Done);
                fire_ai_turn(app, text);
            }
        }
        Route::Reflect(target) => dispatch_kernel(app, target, json!({"type":"reflect"})),
        Route::Kernel(target, payload) => dispatch_kernel(app, target, payload),
        Route::Shell(cmd) => run_shell(app, cmd),
        Route::Workspace(cmd) => dispatch_workspace(app, cmd),
    }
}

/// Drive the active setup flow with a key. Esc cancels back to chat; Select uses
/// ↑/↓ + Enter; Field uses chars/Backspace + Enter. Enter submits the control,
/// advancing the wizard or finishing it.
fn handle_flow_key(app: &mut App, code: KeyCode) {
    match code {
        KeyCode::Esc => {
            app.control = input::Control::Chat;
            app.flow = None;
        }
        KeyCode::Up => app.control.select_up(),
        KeyCode::Down => app.control.select_down(),
        KeyCode::Char(c) => app.control.type_char(c),
        KeyCode::Backspace => app.control.backspace(),
        KeyCode::Enter => {
            let value = app.control.submitted_value();
            let Some(flow) = app.flow.as_mut() else {
                app.control = input::Control::Chat;
                return;
            };
            match flow.submit(value) {
                input::Advance::Next(ctrl) => app.control = ctrl,
                input::Advance::Done(draft) => finish_setup(app, draft),
            }
        }
        _ => {}
    }
}

/// Persist a completed connector draft (settings + OS keychain), update the live
/// env, re-provision the brain (drop the old one), and confirm in the `@ai` agent.
fn finish_setup(app: &mut App, draft: input::Draft) {
    app.control = input::Control::Chat;
    app.flow = None;
    let key = (!draft.key.trim().is_empty()).then_some(draft.key.as_str());
    let mut seal = State::Done;
    let note = match fantastic_host::set_ai_connector(&draft.backend, &draft.model, key) {
        Ok(()) => {
            // Update the live env so the re-provisioned brain reads the new config.
            std::env::set_var("FANTASTIC_AI_BACKEND", &draft.backend);
            std::env::set_var("FANTASTIC_AI_MODEL", &draft.model);
            if let Some(k) = key {
                std::env::set_var("FANTASTIC_AI_KEY", k);
            }
            // Re-provision: drop the old brain + history so the next `@ai`
            // re-ensures with the new model/key (a stale brain keeps the old one).
            app.brain_ready = false;
            let kernel = Arc::clone(&app.kernel);
            tokio::spawn(async move {
                for id in [BRAIN_ID, "ai_fs"] {
                    let _ = kernel
                        .send(
                            &AgentId::from("kernel"),
                            json!({"type":"delete_agent","id":id}),
                        )
                        .await;
                }
            });
            format!(
                "connector set: {} · {} — @ai is ready.",
                draft.backend, draft.model
            )
        }
        Err(e) => {
            seal = State::Error;
            format!("✗ {e}")
        }
    };
    app.tabs
        .deliver("ai")
        .push("brain", "you", Body::Text(note), seal);
    app.tabs.focus("ai");
    app.composer.set_sender("ai");
}

/// End the current AI turn: clear the phase, then — Claude-Code style, AI-only —
/// fire the messages queued mid-turn as ONE concatenated next turn, or go idle.
/// Called from the watched `done` (success) or `ai_rx` (error) — exactly once per
/// turn, since a turn yields a `done` XOR an error.
fn finish_ai_turn(app: &mut App) {
    app.ai_phase = None;
    if app.pending.is_empty() {
        app.chat_busy = false;
    } else {
        let entries: Vec<(u64, String)> = app.pending.drain(..).collect();
        // The queued lines fire now — flip their ⏳ markers to sent.
        for (seq, _) in &entries {
            app.tabs.deliver("ai").set_state(*seq, State::Done);
        }
        let joined = entries
            .into_iter()
            .map(|(_, t)| t)
            .collect::<Vec<_>>()
            .join("\n");
        fire_ai_turn(app, joined);
    }
}

/// Start one AI turn: open a live streaming message in the `@ai` agent, ensure the
/// brain once (mirroring its inbox to ours so we receive the live token stream),
/// then send. SUCCESS is rendered LIVE off the watched stream and sealed by the
/// ordered `done` event (handled in the `brain_rx` arm); only an ERROR (no `done`)
/// is reported back via `ai_tx`.
fn fire_ai_turn(app: &mut App, text: String) {
    // Phase 1: the brain is provisioned in-proc. A folder kernel doesn't get a
    // local brain yet (Phase 2), so `@ai` on a remote kernel degrades to a clear
    // note rather than a hang. (`active` is always InProc until the Kernels screen
    // lands, so this branch is dormant for now.)
    let kernel = match app.active.in_proc() {
        Some(k) => k,
        None => {
            app.tabs.deliver("ai").push(
                "system",
                "ai",
                Body::Note("this kernel has no brain yet — reflect / verbs / @sh only".into()),
                State::Done,
            );
            return;
        }
    };
    app.chat_busy = true;
    app.ai_phase = Some("sending".to_string());
    app.tabs.deliver("ai").start_stream(BRAIN_ID, "you");
    let ai_tx = app.ai_tx.clone();
    let ensure = !app.brain_ready;
    app.brain_ready = true;
    tokio::spawn(async move {
        if ensure {
            if let Err(e) = ai::ensure_brain(&kernel).await {
                let _ = ai_tx.send(format!("✗ {e}"));
                return;
            }
            // The ollama backend emits its stream to the brain's OWN inbox
            // (cli-round-trip route), not ours — so mirror the brain's inbox into
            // our `CLIENT_ID` inbox to receive the live token stream + status. Once.
            kernel
                .watch(&AgentId::from(BRAIN_ID), AgentId::from(CLIENT_ID))
                .await;
        }
        let reply = kernel
            .send(
                &AgentId::from(BRAIN_ID),
                json!({"type":"send","text":text,"client_id":CLIENT_ID}),
            )
            .await;
        // SUCCESS streamed live + sealed by the watched `done`; nothing to do here.
        // Only an error needs reporting (it produces no `done`): surface the dry
        // "unreachable" guidance for a reachability failure, else the raw error.
        if let Some(e) = reply.get("error").and_then(Value::as_str) {
            let out = if ai::is_unreachable(e) {
                ai::dry_reply(&ai::config_status(), Some(e)).unwrap_or_else(|| format!("✗ {e}"))
            } else {
                format!("✗ {e}")
            };
            let _ = ai_tx.send(out);
        }
    });
}

/// Drive the out-of-process workspace kernel over the gateway. Lifecycle
/// (`Up`/`Down`) and verbs all run in spawned tasks that own a cloned handle and
/// report their result back through `ws_tx` → `handle_ws_event`.
fn dispatch_workspace(app: &mut App, cmd: WsCmd) {
    let tx = app.ws_tx.clone();
    match cmd {
        WsCmd::Up(rt) => {
            if app.ws_busy {
                app.tabs.deliver("ws").push(
                    "ws",
                    "you",
                    Body::Note("ws: still working…".into()),
                    State::Done,
                );
                return;
            }
            app.tabs.deliver("ws").push(
                "you",
                "ws",
                Body::Tool {
                    verb: "up".into(),
                    target: "ws".into(),
                    summary: String::new(),
                },
                State::Done,
            );
            app.ws_busy = true;
            tokio::spawn(async move {
                let ev = match std::env::current_dir() {
                    Ok(dir) => {
                        let ws = gateway::Workspace { dir };
                        // Probe for an existing daemon so we can report
                        // attached-vs-spawned; either way attach_or_spawn yields
                        // the live handle.
                        let pre = ws.attach().await.ok().flatten().is_some();
                        match ws.attach_or_spawn(rt).await {
                            Ok(handle) => WsEvent::Attached(handle, !pre),
                            Err(e) => WsEvent::Error(e.to_string()),
                        }
                    }
                    Err(e) => WsEvent::Error(format!("cwd: {e}")),
                };
                let _ = tx.send(ev);
            });
        }
        WsCmd::Down => {
            let Some(handle) = app.workspace.clone() else {
                app.tabs.deliver("ws").push(
                    "ws",
                    "you",
                    Body::Note("no workspace".into()),
                    State::Done,
                );
                return;
            };
            app.tabs.deliver("ws").push(
                "you",
                "ws",
                Body::Tool {
                    verb: "down".into(),
                    target: "ws".into(),
                    summary: String::new(),
                },
                State::Done,
            );
            tokio::spawn(async move {
                let _ = handle.send("core", json!({"type":"shutdown_kernel"})).await;
                let _ = tx.send(WsEvent::Down);
            });
        }
        WsCmd::Verb(target, payload) => {
            let Some(handle) = app.workspace.clone() else {
                app.tabs.deliver("ws").push(
                    "ws",
                    "you",
                    Body::Note("no workspace — try `@ws up`".into()),
                    State::Done,
                );
                return;
            };
            let verb = payload
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("send")
                .to_string();
            app.tabs.deliver("ws").push(
                "you",
                "ws",
                Body::Tool {
                    verb,
                    target: target.as_str().to_string(),
                    summary: String::new(),
                },
                State::Done,
            );
            tokio::spawn(async move {
                let ev = match handle.send(target.as_str(), payload).await {
                    Ok(v) => WsEvent::Reply(v),
                    Err(e) => WsEvent::Error(e.to_string()),
                };
                let _ = tx.send(ev);
            });
        }
    }
}

/// Fold a workspace gateway task result into the App + transcript.
fn handle_ws_event(app: &mut App, ev: WsEvent) {
    match ev {
        WsEvent::Attached(handle, spawned) => {
            let note = format!(
                "workspace {} at {}",
                if spawned { "spawned" } else { "attached" },
                handle.base_url
            );
            app.workspace = Some(handle);
            app.ws_busy = false;
            app.tabs
                .deliver("ws")
                .push("ws", "you", Body::Note(note), State::Done);
        }
        WsEvent::Reply(v) => {
            let rendered = serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string());
            app.tabs
                .deliver("ws")
                .push("ws", "you", Body::Text(rendered), State::Done);
        }
        WsEvent::Error(e) => {
            app.ws_busy = false;
            app.tabs.deliver("ws").push(
                "ws",
                "you",
                Body::Note(format!("✗ ws: {e}")),
                State::Error,
            );
        }
        WsEvent::Down => {
            app.workspace = None;
            app.tabs.deliver("ws").push(
                "ws",
                "you",
                Body::Note("workspace stopped".into()),
                State::Done,
            );
        }
    }
}

/// Run `cmd` in the shared live PTY and start breathing its screen into the
/// chat. Lazily spawns the `TerminalSession` (sized to the chat-body grid) on
/// the first `@sh`, logs the command on the `sh` rail, then writes `cmd\r`.
fn run_shell(app: &mut App, cmd: String) {
    if app.term.is_none() {
        let (rows, cols) = app.chat_term_grid;
        app.term = TerminalSession::spawn(rows, cols, app.redraw_tx.clone()).ok();
    }
    app.tabs.deliver("sh").push(
        "you",
        "sh",
        Body::Tool {
            verb: "sh".to_string(),
            target: "sh".to_string(),
            summary: cmd.clone(),
        },
        State::Done,
    );
    if let Some(ts) = app.term.as_mut() {
        let mut line = cmd;
        line.push('\r');
        ts.write(line.as_bytes());
        app.term_active = true;
    }
}

/// Push a `you→target` prompt and fire a one-shot `kernel.send`; the rendered
/// reply returns via `cmd_tx` as a `target→you` line.
fn dispatch_kernel(app: &mut App, target: AgentId, payload: Value) {
    let verb = payload
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("send")
        .to_string();
    app.tabs.deliver(target.as_str()).push(
        "you",
        target.as_str(),
        Body::Tool {
            verb,
            target: target.as_str().to_string(),
            summary: String::new(),
        },
        State::Done,
    );
    let active = app.active.clone();
    let tx = app.cmd_tx.clone();
    let from = target.as_str().to_string();
    tokio::spawn(async move {
        // `dispatch` mirrors the target's own inbox into our client inbox FIRST (for
        // the in-proc kernel), so any events it emits while handling the verb
        // (token/say/status/done, or a generic activity event) render LIVE in its
        // agent via `brain_rx` — exactly the path `@ai` uses. A remote kernel is
        // request→response here; its live emits arrive over the Phase-2 WS mirror.
        let reply = active.dispatch(&target, payload).await;
        let rendered = serde_json::to_string_pretty(&reply).unwrap_or_else(|_| reply.to_string());
        let _ = tx.send((from, rendered));
    });
}

/// Encode a key press into the bytes a PTY expects.
fn encode_key(code: KeyCode, mods: KeyModifiers) -> Option<Vec<u8>> {
    let ctrl = mods.contains(KeyModifiers::CONTROL);
    let bytes = match code {
        KeyCode::Char(c) if ctrl => {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphabetic() {
                vec![lc as u8 - b'a' + 1] // Ctrl-A..Z → 0x01..0x1a
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => {
            let mut b = [0u8; 4];
            c.encode_utf8(&mut b).as_bytes().to_vec()
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        _ => return None,
    };
    Some(bytes)
}

/// The chat breathing-viewport grid `(rows, cols)`: the PTY is sized to the
/// FULL chat-body interior so full-screen TUIs get real agent, even though only
/// `used_rows` of it are displayed below the transcript. Subtracts the header
/// (3) + body border (2) chrome, the viewport's `│ sh` header row, and the
/// input line (3) below the transcript.
fn chat_term_grid<B: ratatui::backend::Backend>(term: &Terminal<B>) -> (u16, u16) {
    let size = term.size().unwrap_or(Size::new(80, 24));
    let rows = size.height.saturating_sub(3 + 2 + 1 + 3).max(1);
    let cols = size.width.saturating_sub(2).max(1);
    (rows, cols)
}

/// Black out every cell of `area` (so a floated, opaque widget hides the stars
/// beneath it). The widget's own glyphs then render on top. The fill fg is
/// White (not Black) so any UNSTYLED span (e.g. the raw input text) renders
/// visibly over the panel instead of inheriting an invisible black fg.
fn fill_black(buf: &mut Buffer, area: Rect) {
    let st = Style::default().bg(Color::Black).fg(Color::White);
    for y in area.y..area.y.saturating_add(area.height) {
        for x in area.x..area.x.saturating_add(area.width) {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(st);
            }
        }
    }
}

/// On/off square wave from a clock (for the blinking attract prompt).
fn blink(clock: f32, hz: f32) -> bool {
    ((clock * hz) as i64) % 2 == 0
}

/// Write `s` centered on row `y` of `area`, straight into the buffer (over bg).
fn buf_text_center(buf: &mut Buffer, area: Rect, y: i32, s: &str, style: Style) {
    if y < 0 || y >= area.height as i32 {
        return;
    }
    let len = s.chars().count() as i32;
    let x0 = area.x as i32 + (area.width as i32 - len) / 2;
    for (i, ch) in s.chars().enumerate() {
        let x = x0 + i as i32;
        if x < area.x as i32 || x >= (area.x + area.width) as i32 {
            continue;
        }
        if let Some(cell) = buf.cell_mut((x as u16, area.y + y as u16)) {
            cell.set_char(ch);
            cell.set_style(style);
        }
    }
}

fn ui(f: &mut Frame, app: &App) {
    let clock = app.boot.elapsed().as_secs_f32();
    let full = f.area();

    // Dark theme, always: force a black canvas so the app looks the same on a
    // light or dark terminal — dark sky everywhere, content opaque on top.
    fill_black(f.buffer_mut(), full);

    // The intro movie plays full-screen over everything (its own scenes).
    if app.intro_playing {
        let elapsed = app
            .intro_since
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        app.movie.render(f, full, elapsed);
        return;
    }

    // The moving sky (drifting starfield) sits behind every phase.
    bg::render_sky(f.buffer_mut(), full, clock);

    match app.phase {
        // ATTRACT — the big title revealing top→bottom + a blinking prompt over the
        // moving sky.
        Phase::Attract => {
            let buf = f.buffer_mut();
            let reveal = (app.attract_since.elapsed().as_secs_f32() / 1.5).clamp(0.0, 1.0);
            let title_bottom = bg::render_title(buf, full, reveal);
            if blink(clock, 1.2) {
                buf_text_center(
                    buf,
                    full,
                    title_bottom + 2,
                    "PRESS ANY KEY TO CONTINUE",
                    Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                );
            }
        }
        // MINIMIZING — the title tweens from big-centered to small-docked over
        // ~1s (eased), sliding + shrinking to the top-left header slot.
        Phase::Minimizing => {
            let buf = f.buffer_mut();
            let raw = app
                .minimize_since
                .map(|s| s.elapsed().as_secs_f32() / MINIMIZE_SECS)
                .unwrap_or(1.0)
                .clamp(0.0, 1.0);
            let t = 1.0 - (1.0 - raw) * (1.0 - raw); // ease-out
            let geo = bg::tween_geo(bg::title_geometry(full), bg::docked_geometry(full), t);
            bg::draw_title(buf, full, geo, 1.0);
        }
        // KERNELS — the top rung: the row of kernel squares, plus the `+ new`
        // overlay (folder browser, then runtime pick) when creating one.
        Phase::Kernels => {
            render_kernels(f, full, app);
            if app.browser.is_some() {
                render_browser(f, full, app);
            } else if app.new_runtime.is_some() {
                render_runtime(f, full, app);
            }
        }
        // HUB — the lobby: the small docked title + the navigable agent tree.
        Phase::Hub => render_hub(f, full, app),
        // CHAT — the maximized agent chat.
        Phase::Chat => render_chat(f, full, app),
    }
}

/// Write a cell at absolute `(x, y)`, clipped to `clip`; out-of-bounds is a no-op.
fn cell_at(buf: &mut Buffer, clip: Rect, x: i32, y: i32, ch: char, style: Style) {
    if x < clip.x as i32
        || y < clip.y as i32
        || x >= (clip.x + clip.width) as i32
        || y >= (clip.y + clip.height) as i32
    {
        return;
    }
    if let Some(c) = buf.cell_mut((x as u16, y as u16)) {
        c.set_char(ch);
        c.set_style(style);
    }
}

/// Write a string starting at absolute `(x, y)` (clipped). Returns nothing.
fn str_at(buf: &mut Buffer, clip: Rect, x: i32, y: i32, s: &str, style: Style) {
    for (i, ch) in s.chars().enumerate() {
        cell_at(buf, clip, x + i as i32, y, ch, style);
    }
}

/// The rendered label for a tree node (`★@ai`, `@fs`, …).
fn hub_label(n: &hub::HubNode) -> String {
    format!("{}@{}", if n.is_home { "★" } else { "" }, n.label)
}

/// Truncate `s` to `w` display cells, appending `…` when it overflows.
fn clip_str(s: &str, w: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= w {
        return s.to_string();
    }
    let mut out: String = chars[..w.saturating_sub(1)].iter().collect();
    out.push('…');
    out
}

/// Draw a 1-cell box border `[bx, by] .. [bx+w-1, by+h-1]` (clipped to `clip`).
fn draw_box(buf: &mut Buffer, clip: Rect, bx: i32, by: i32, w: i32, h: i32, st: Style) {
    let (r, b) = (bx + w - 1, by + h - 1);
    cell_at(buf, clip, bx, by, '┌', st);
    cell_at(buf, clip, r, by, '┐', st);
    cell_at(buf, clip, bx, b, '└', st);
    cell_at(buf, clip, r, b, '┘', st);
    for x in (bx + 1)..r {
        cell_at(buf, clip, x, by, '─', st);
        cell_at(buf, clip, x, b, '─', st);
    }
    for y in (by + 1)..b {
        cell_at(buf, clip, bx, y, '│', st);
        cell_at(buf, clip, r, y, '│', st);
    }
}

/// The KERNELS screen (top nav rung): the docked title, then a horizontal row of
/// fixed-size boxes — `★ RAM` (the in-proc kernel, always first), any folder kernels
/// (Slice 3), and a trailing `+ new`. Each box carries 3 status lines
/// (name · kind · status); the selected one is drawn white. `←/→` pick, `⏎` opens.
fn render_kernels(f: &mut Frame, full: Rect, app: &App) {
    bg::draw_title(f.buffer_mut(), full, bg::docked_geometry(full), 1.0);

    let dim = Style::default().fg(Color::DarkGray);
    let mag = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let squares = &app.kernels_view.squares;
    if squares.is_empty() {
        return;
    }
    let sel = app.kernels_view.sel.min(squares.len() - 1);
    let box_w = 16_i32;
    let box_h = 5_i32;
    let col_step = box_w + 2;
    let n = squares.len() as i32;
    let total_w = n * col_step - 2;
    let base_x = full.x as i32 + 4 + ((full.width as i32 - 8 - total_w).max(0)) / 2;
    let top = full.y as i32 + 6;
    let active_inproc = matches!(app.active, kernels::ActiveKernel::InProc(_));

    let buf = f.buffer_mut();
    str_at(
        buf,
        full,
        full.x as i32 + 4,
        full.y as i32 + 4,
        "kernels — pick one",
        mag,
    );
    for (i, sq) in squares.iter().enumerate() {
        let selected = i == sel;
        let label = sq.label();
        let (l2, l3): (String, String) = match sq {
            kernels::Square::Ram => (
                "in-proc".into(),
                if active_inproc { "active" } else { "live" }.into(),
            ),
            kernels::Square::Folder { runtime, live, .. } => (
                runtime.as_str().into(),
                if *live { "live" } else { "offline" }.into(),
            ),
            kernels::Square::New => ("kernel".into(), String::new()),
        };
        let white = Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD);
        let border = if selected { white } else { dim };
        let name_st = if selected { white } else { mag };
        let body_st = if selected { white } else { dim };
        let x0 = base_x + i as i32 * col_step;
        draw_box(buf, full, x0, top, box_w, box_h, border);
        let cxi = x0 + 2;
        str_at(buf, full, cxi, top + 1, &clip_str(&label, 12), name_st);
        str_at(buf, full, cxi, top + 2, &clip_str(&l2, 12), body_st);
        str_at(buf, full, cxi, top + 3, &clip_str(&l3, 12), body_st);
    }
    // A transient status/error line (attaching…, ✗ failed), above the hint.
    if let Some(msg) = &app.kernels_msg {
        str_at(
            buf,
            full,
            full.x as i32 + 4,
            full.y as i32 + full.height as i32 - 3,
            &clip_str(msg, full.width.saturating_sub(8) as usize),
            if msg.starts_with('✗') {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Yellow)
            },
        );
    }
    str_at(
        buf,
        full,
        full.x as i32 + 4,
        full.y as i32 + full.height as i32 - 2,
        "←→ pick · ⏎ open · Ctrl+Q quit",
        dim,
    );
}

/// A centered opaque panel (fills black, draws a magenta border) at `(w×h)`; returns
/// the interior top-left `(x, y)`. Used by the `+ new` overlays.
fn draw_panel(buf: &mut Buffer, full: Rect, w: i32, h: i32) -> (i32, i32) {
    let x0 = full.x as i32 + (full.width as i32 - w).max(0) / 2;
    let y0 = full.y as i32 + (full.height as i32 - h).max(0) / 2;
    let panel = Rect {
        x: x0.max(0) as u16,
        y: y0.max(0) as u16,
        width: (w.max(0) as u16).min(full.width),
        height: (h.max(0) as u16).min(full.height),
    };
    fill_black(buf, panel);
    draw_box(
        buf,
        full,
        x0,
        y0,
        w,
        h,
        Style::default()
            .fg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD),
    );
    (x0 + 2, y0 + 1)
}

/// The `+ new` folder browser overlay: a centered list of the current dir's subdirs
/// (`..` first), selection highlighted; ⏎ descends, Space uses the current folder.
fn render_browser(f: &mut Frame, full: Rect, app: &App) {
    let Some(b) = &app.browser else {
        return;
    };
    let dim = Style::default().fg(Color::DarkGray);
    let mag = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let white = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let w = (full.width as i32 - 8).clamp(24, 60);
    let rows = (full.height as i32 - 10).clamp(3, 14);
    let h = rows + 4;
    let inner_w = (w - 4) as usize;
    let buf = f.buffer_mut();
    let (ix, iy) = draw_panel(buf, full, w, h);
    str_at(
        buf,
        full,
        ix,
        iy,
        &clip_str(
            &format!("pick a folder — {}", b.cwd.to_string_lossy()),
            inner_w,
        ),
        mag,
    );
    // Scroll the entry window so the selection stays visible.
    let n = b.entries.len() as i32;
    let start = (b.sel as i32 - rows / 2).clamp(0, (n - rows).max(0)) as usize;
    for (row, i) in (start..b.entries.len()).take(rows as usize).enumerate() {
        let selected = i == b.sel;
        let name = &b.entries[i];
        let slash = if name == ".." { "" } else { "/" };
        let cursor = if selected { "▸ " } else { "  " };
        str_at(
            buf,
            full,
            ix,
            iy + 2 + row as i32,
            &clip_str(&format!("{cursor}{name}{slash}"), inner_w),
            if selected { white } else { dim },
        );
    }
    str_at(
        buf,
        full,
        ix,
        iy + h - 2,
        &clip_str("↑↓ move · ⏎ open · space: use this folder · esc", inner_w),
        dim,
    );
}

/// The `+ new` runtime picker (after a folder is chosen): rust / python / swift.
fn render_runtime(f: &mut Frame, full: Rect, app: &App) {
    let Some((dir, cursor)) = &app.new_runtime else {
        return;
    };
    let dim = Style::default().fg(Color::DarkGray);
    let mag = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let white = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let w = 44_i32;
    let h = RUNTIMES.len() as i32 + 4;
    let inner_w = (w - 4) as usize;
    let buf = f.buffer_mut();
    let (ix, iy) = draw_panel(buf, full, w, h);
    str_at(
        buf,
        full,
        ix,
        iy,
        &clip_str(
            &format!(
                "runtime for {}",
                dir.file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| dir.to_string_lossy().into_owned())
            ),
            inner_w,
        ),
        mag,
    );
    for (i, rt) in RUNTIMES.iter().enumerate() {
        let selected = i == *cursor;
        let cur = if selected { "▸ " } else { "  " };
        str_at(
            buf,
            full,
            ix,
            iy + 2 + i as i32,
            &format!("{cur}{}", rt.as_str()),
            if selected { white } else { dim },
        );
    }
    str_at(buf, full, ix, iy + h - 2, "↑↓ · ⏎ create · esc", dim);
}

/// The HUB lobby: the small docked FANTASTIC title, then the kernel agents as a
/// **hierarchical top-down tree** of fixed-size boxes (1-cell border, up to 3
/// status lines each — the selected one drawn white), fanning out from the root
/// and joined by connector lines, then a nav hint. Arrows fly the tree; Enter opens
/// an agent. (Overflow past the viewport is left for a future pan/scroll.)
fn render_hub(f: &mut Frame, full: Rect, app: &App) {
    bg::draw_title(f.buffer_mut(), full, bg::docked_geometry(full), 1.0);

    let dim = Style::default().fg(Color::DarkGray);
    let nodes = app.hub.nodes();
    if nodes.is_empty() {
        return;
    }
    let sel = app.hub.sel.min(nodes.len() - 1);

    // Fixed-size boxes: a 1-cell border wrapping `CONTENT_W`-wide content, 3 status
    // lines tall. `col` (leaf units) → box columns; `depth` → box rows.
    const CONTENT_W: usize = 12;
    let box_w = CONTENT_W as i32 + 4; // 1 border + 1 pad on each side
    let box_h = 5_i32; // border + 3 content lines + border
    let col_step = box_w + 2;
    let row_step = box_h + 1; // a box + one connector row per depth
    let min_col = nodes.iter().map(|n| n.col).fold(f32::MAX, f32::min);
    let max_col = nodes.iter().map(|n| n.col).fold(f32::MIN, f32::max);
    let total_w = ((max_col - min_col) * col_step as f32) as i32 + box_w;
    let pad = ((full.width as i32 - 8 - total_w).max(0)) / 2;
    let base_x = full.x as i32 + 4 + pad;
    let top = full.y as i32 + 6; // below the docked title (3 rows) + a heading row
    let bx = |n: &hub::HubNode| base_x + ((n.col - min_col) * col_step as f32) as i32;
    let by = |n: &hub::HubNode| top + (n.depth as i32) * row_step;
    let ccol = |n: &hub::HubNode| bx(n) + box_w / 2;

    let buf = f.buffer_mut();
    str_at(
        buf,
        full,
        full.x as i32 + 4,
        full.y as i32 + 4,
        "the agents — pick one",
        Style::default()
            .fg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD),
    );

    // Boxes with their status lines.
    for (i, n) in nodes.iter().enumerate() {
        let selected = i == sel;
        // White on select; otherwise a dim border + the agent's own tint on line 1.
        let (border, name_st) = if selected {
            let white = Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD);
            (white, white)
        } else if n.is_home {
            (
                dim,
                Style::default()
                    .fg(chat::color_for("ai"))
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            (dim, Style::default().fg(chat::color_for(&n.agent)))
        };
        let body_st = if selected { border } else { dim };
        let (x0, y0) = (bx(n), by(n));
        draw_box(buf, full, x0, y0, box_w, box_h, border);
        // 3 status lines: name · type · children/leaf.
        let name = clip_str(&hub_label(n), CONTENT_W);
        let ty = clip_str(n.ty.as_deref().unwrap_or("agent"), CONTENT_W);
        let sub = if n.children.is_empty() {
            "leaf".to_string()
        } else {
            format!("{} agents", n.children.len())
        };
        let cxi = x0 + 2;
        str_at(buf, full, cxi, y0 + 1, &name, name_st);
        str_at(buf, full, cxi, y0 + 2, &ty, body_st);
        str_at(buf, full, cxi, y0 + 3, &clip_str(&sub, CONTENT_W), body_st);
    }

    // Connectors (drawn over the box borders): a `┬` stem out of each parent's
    // bottom, a merged horizontal bus, and a `┴` into each child's top.
    for p in nodes.iter().filter(|n| !n.children.is_empty()) {
        let pcx = ccol(p);
        let bus_y = by(p) + box_h; // the row just below the parent box
        let child_cx: Vec<i32> = p.children.iter().map(|&i| ccol(&nodes[i])).collect();
        let lo = *child_cx.iter().min().unwrap().min(&pcx);
        let hi = *child_cx.iter().max().unwrap().max(&pcx);
        cell_at(buf, full, pcx, bus_y - 1, '┬', dim); // parent bottom border
        for x in lo..=hi {
            let up = x == pcx;
            let down = child_cx.contains(&x);
            let ch = box_char(up, down, x > lo, x < hi);
            if ch != ' ' {
                cell_at(buf, full, x, bus_y, ch, dim);
            }
        }
        for &cxx in &child_cx {
            cell_at(buf, full, cxx, bus_y + 1, '┴', dim); // child top border
        }
    }

    str_at(
        buf,
        full,
        full.x as i32 + 4,
        full.y as i32 + full.height as i32 - 2,
        "↑ parent · ↓ child · ←→ siblings · ⏎ open · Ctrl+Q quit",
        dim,
    );
}

/// Pick a box-drawing glyph from the four connection directions.
fn box_char(up: bool, down: bool, left: bool, right: bool) -> char {
    match (up, down, left, right) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (true, true, false, false) => '│',
        (true, false, true, true) => '┴',
        (true, false, true, false) => '┘',
        (true, false, false, true) => '└',
        (true, false, false, false) => '│',
        (false, true, true, true) => '┬',
        (false, true, true, false) => '┐',
        (false, true, false, true) => '┌',
        (false, true, false, false) => '│',
        (false, false, true, _) | (false, false, false, true) => '─',
        _ => ' ',
    }
}

/// The unified chat over the moving sky. The whole chat block is inset 2 cells from
/// every screen edge (so the sky shows in that border margin); inside, a slim
/// header, the transcript, a 1-row gap, then the input box at the bottom. Transcript
/// and input are opaque. The header is deliberately minimal: a neon-magenta bar,
/// `FANTASTIC` on the left, and a right-aligned `esc to switch` hint. Esc drops back
/// to the hub, which is the agent switcher, so no tab bar or ws or agent-count
/// clutter is drawn here.
fn render_chat_header(f: &mut Frame, area: Rect) {
    fill_black(f.buffer_mut(), area);
    let bar = Color::Indexed(165); // neon magenta — matches ansi_banner's 38;5;165
    let buf = f.buffer_mut();
    // The vertical bar, full height of the header.
    for dy in 0..area.height {
        if let Some(c) = buf.cell_mut((area.x, area.y + dy)) {
            c.set_char('█');
            c.set_style(Style::default().fg(bar));
        }
    }
    let ty = area.y + area.height / 2; // the middle row
                                       // `FANTASTIC` on the left, bright magenta + bold, two cells past the bar.
    let title_st = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    for (i, ch) in "FANTASTIC".chars().enumerate() {
        let x = area.x + 3 + i as u16;
        if x < area.x + area.width {
            if let Some(c) = buf.cell_mut((x, ty)) {
                c.set_char(ch);
                c.set_style(title_st);
            }
        }
    }
    // `/help · esc → switch` right-aligned on the same row.
    let hint = "/help · esc → switch";
    let hint_st = Style::default().fg(Color::DarkGray);
    let hx = area.x + area.width.saturating_sub(hint.chars().count() as u16 + 1);
    for (i, ch) in hint.chars().enumerate() {
        let x = hx + i as u16;
        if x < area.x + area.width {
            if let Some(c) = buf.cell_mut((x, ty)) {
                c.set_char(ch);
                c.set_style(hint_st);
            }
        }
    }
}

fn render_chat(f: &mut Frame, area: Rect, app: &App) {
    // Inset 2 cells from every edge — the stars show through this border margin.
    let inset = Rect {
        x: area.x + 2,
        y: area.y + 2,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(4),
    };
    // header (3) · transcript (min) · bottom region (dynamic). The bottom is the
    // chat input normally; it grows into a setup-flow panel when a `/setup` or
    // `/model` flow is active (a select list needs agent for its options).
    let bottom_h: u16 = match &app.control {
        input::Control::Select { options, .. } => {
            (options.len() as u16 + 4).clamp(5, inset.height.saturating_sub(6).max(5))
        }
        _ => 4, // Chat: 1 star-gap + 3-row input. Field: title + field + hint.
    };
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(bottom_h),
        ])
        .split(inset);
    let header_area = parts[0];
    let chat_area = parts[1];
    let bottom = parts[2];

    render_chat_header(f, header_area);

    // The transcript is OPAQUE: black it out first.
    fill_black(f.buffer_mut(), chat_area);

    // When a `@sh` command is live AND `@sh` is the active agent, the bottom of the
    // chat body breathes the PTY screen. The viewport height = `used_rows` of the
    // PTY (clamped to the body) + 1 for its `│ sh` header; the transcript scrolls
    // in what remains. For every OTHER agent the PTY is hidden (it never bleeds).
    let transcript_area = if term_visible(app) {
        if let Some(ts) = &app.term {
            if let Ok(p) = ts.parser.lock() {
                // Leave the transcript at least one row; the viewport claims
                // the rest, up to `used_rows` (+ its header).
                let max_body = chat_area.height.saturating_sub(2).max(1);
                let used = used_rows(&p, max_body).clamp(1, max_body);
                let vp_h = (used + 1).min(chat_area.height.saturating_sub(1));
                let split = Layout::default()
                    .direction(Direction::Vertical)
                    .constraints([Constraint::Min(1), Constraint::Length(vp_h)])
                    .split(chat_area);
                render_chat_terminal(f, split[1], p.screen(), app.term_focused);
                split[0]
            } else {
                chat_area
            }
        } else {
            chat_area
        }
    } else {
        chat_area
    };

    let lines = transcript_lines(app);
    // Show the tail that fits the pane height, BOTTOM-anchored like a chat app:
    // the newest message sits just above the input, and a short conversation
    // leaves the empty space at the TOP rather than a gap in the
    // middle. Count VISUAL rows (a long line wraps to several), accumulating from
    // the bottom up, so the newest entries are never clipped off the bottom edge.
    // `app.scroll` pages BACK through history (PageUp / mouse wheel): it skips
    // that many visual rows off the bottom before anchoring, line-granular.
    let h = transcript_area.height as usize;
    let aw = transcript_area.width.max(1) as usize;
    let rows: Vec<usize> = lines
        .iter()
        .map(|l| l.width().div_ceil(aw).max(1))
        .collect();
    let total: usize = rows.iter().sum();
    // Publish the max meaningful scroll so the key/wheel handlers clamp to it.
    let max_scroll = total.saturating_sub(h);
    app.scroll_max.set(max_scroll);
    let scroll = app.scroll.min(max_scroll);
    let mut skipped = 0usize;
    let mut end = lines.len();
    while end > 0 && skipped < scroll {
        skipped += rows[end - 1];
        end -= 1;
    }
    let mut used = 0usize;
    let mut start = end;
    for i in (0..end).rev() {
        let r = rows[i];
        if used + r > h && i + 1 < start {
            break; // keep at least the newest visible line even if it overflows
        }
        used += r;
        start = i;
    }
    let pad = h.saturating_sub(used);
    let mut view: Vec<Line> = vec![Line::from(""); pad];
    view.extend(lines.into_iter().take(end).skip(start));
    f.render_widget(
        Paragraph::new(view).wrap(Wrap { trim: false }),
        transcript_area,
    );
    // Scrolled away from the live edge: a dim marker points back down.
    if scroll > 0 {
        let marker = " ↓ newer (End) ";
        let w = marker.chars().count() as u16;
        let mx = transcript_area
            .x
            .saturating_add(transcript_area.width.saturating_sub(w));
        let my = transcript_area
            .y
            .saturating_add(transcript_area.height.saturating_sub(1));
        let area = Rect {
            x: mx,
            y: my,
            width: w.min(transcript_area.width),
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                marker,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::DIM),
            )),
            area,
        );
    }

    // Bottom region: the chat composer, or the active setup-flow control.
    match &app.control {
        input::Control::Chat => {
            // 1 star-gap row (stars peek through) + the 3-row opaque input box.
            let b = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(1), Constraint::Min(1)])
                .split(bottom);
            let input_area = b[1];
            fill_black(f.buffer_mut(), input_area);
            render_composer(f, input_area, app);
            // The `@`-palette floats over the transcript bottom while the
            // sender is being edited — navigate + discover in one gesture.
            if app.composer.editing_sender {
                render_palette(f, transcript_area, app);
            }
        }
        ctrl => {
            fill_black(f.buffer_mut(), bottom);
            render_flow(f, bottom, ctrl);
        }
    }

    // The /help overlay floats over everything; any key closes it.
    if app.help_open {
        render_help(f, inset);
    }
}

/// The `/help` overlay: the whole interactive surface on one card — routing,
/// the palette, agent turns, editing, scrollback, commands, exits.
fn render_help(f: &mut Frame, within: Rect) {
    let dim = Style::default().fg(Color::DarkGray);
    let key = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let body = Style::default().fg(Color::White);
    let rows: [(&str, &str); 12] = [
        ("@", "palette: ↑↓ pick · ⏎ open · ⇥ complete · esc"),
        (
            "@ai …",
            "talk to the brain (streams live · Ctrl+C interrupts)",
        ),
        (
            "@sh <cmd>",
            "run in the live shell (Ctrl+F focus · Esc release)",
        ),
        ("@ws …", "workspace kernel: up · down · <verb> [k=v…]"),
        ("@<id> [verb]", "drive any agent; bare @<id> reflects it"),
        ("⇧⇥ / Esc", "next agent · Esc minimizes to the lobby"),
        ("↑ ↓", "recall sent lines"),
        ("PgUp PgDn", "scroll history (End snaps down)"),
        ("←→ Home End", "move the caret in the input"),
        (
            "copy / paste",
            "select + copy in your terminal · Cmd/Ctrl+V pastes",
        ),
        ("/commands", "/help · /intro · /setup · /model"),
        ("exit", "Ctrl+Q · Ctrl+C twice · hold q"),
    ];
    let w = (within.width.saturating_sub(4)).clamp(20, 60);
    let h = (rows.len() as u16 + 4).min(within.height);
    let area = Rect {
        x: within.x + (within.width.saturating_sub(w)) / 2,
        y: within.y + (within.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    };
    fill_black(f.buffer_mut(), area);
    let mut lines = vec![
        Line::from(Span::styled(
            "FANTASTIC — how to play",
            Style::default()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
    ];
    for (k, v) in rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {k:<13}"), key),
            Span::styled(v.to_string(), body),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled("  any key closes", dim)));
    f.render_widget(Paragraph::new(lines), area);
}

/// The `@`-palette: a small panel anchored just above the input while the
/// `@sender` is being edited. **`ai` pinned first**, then open agents (unread
/// `•`), then known-but-unopened agents (dim — picking one opens its chat and
/// a bare submit reflects it). ↑↓ move · ⏎ open · ⇥ complete · esc.
fn render_palette(f: &mut Frame, transcript_area: Rect, app: &App) {
    let items = chat::palette_items(&app.tabs, &app.known_agents, &app.composer.sender);
    let mag = Color::Indexed(165);
    let dim = Style::default().fg(Color::DarkGray);
    // Panel geometry: bottom-anchored inside the transcript, modest width.
    let rows = items.len().clamp(1, 8) as u16;
    let h = (rows + 2).min(transcript_area.height); // title + rows + hint
    if h < 3 {
        return; // no space — the inline sender edit still works
    }
    let area = Rect {
        x: transcript_area.x,
        y: transcript_area.y + transcript_area.height - h,
        width: transcript_area.width.min(44),
        height: h,
    };
    fill_black(f.buffer_mut(), area);
    let mut lines = vec![Line::from(Span::styled(
        "@ agents",
        Style::default()
            .fg(Color::LightMagenta)
            .add_modifier(Modifier::BOLD),
    ))];
    if items.is_empty() {
        lines.push(Line::from(Span::styled("no match", dim)));
    } else {
        // Keep the selection visible: slide the window once it scrolls past.
        let sel = app.palette_idx.min(items.len() - 1);
        let win = rows as usize;
        let start = (sel + 1).saturating_sub(win);
        for (i, it) in items.iter().enumerate().skip(start).take(win) {
            let selected = i == sel;
            let mark = if selected { "▸ " } else { "  " };
            let id_style = if selected {
                Style::default().fg(mag).add_modifier(Modifier::BOLD)
            } else if it.open {
                Style::default().fg(chat::color_for(&it.id))
            } else {
                dim
            };
            let mut spans = vec![
                Span::styled(mark.to_string(), id_style),
                Span::styled(format!("@{}", it.id), id_style),
            ];
            if it.unread {
                spans.push(Span::styled(" •", Style::default().fg(Color::Yellow)));
            }
            if !it.open {
                spans.push(Span::styled("  opens it", dim));
            }
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(Span::styled(
        "↑↓ · ⏎ open · ⇥ complete · esc",
        dim,
    )));
    f.render_widget(Paragraph::new(lines), area);
}

/// The smart composer: an editable `@<sender>` field + ` ▸ ` + the message. The
/// sender lights its character color; while you're editing it (or it names no
/// known character → a nogo) it's underlined / flashes red. Borderless; the
/// message text is explicit White over the black panel.
fn render_composer(f: &mut Frame, input_area: Rect, app: &App) {
    let c = &app.composer;
    let valid = c.is_valid(&app.known_senders());
    let sender_col = if c.nogo || !valid {
        Color::Red
    } else {
        chat::color_for(&c.sender)
    };
    let sender_st = if c.editing_sender {
        Style::default()
            .fg(sender_col)
            .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
    } else {
        Style::default().fg(sender_col).add_modifier(Modifier::BOLD)
    };
    // Pasted newlines render as a visible `⏎` on the one-line prompt (the sent
    // message keeps the real breaks).
    let shown: String = c
        .message
        .chars()
        .map(|ch| if ch == '\n' { '⏎' } else { ch })
        .collect();
    let prompt = Line::from(vec![
        Span::styled(format!("@{}", c.sender), sender_st),
        Span::styled(" ▸ ", Style::default().fg(Color::DarkGray)),
        Span::styled(shown, Style::default().fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(prompt), input_area);
    // The terminal caret tracks the composer's char cursor (not just the tail).
    let cx = if c.editing_sender {
        input_area.x + 1 + c.sender.chars().count() as u16
    } else {
        input_area.x + (1 + c.sender.chars().count() + 3 + c.cursor) as u16
    };
    f.set_cursor_position((
        cx.min(input_area.x + input_area.width.saturating_sub(1)),
        input_area.y,
    ));
}

/// Render the active setup-flow control: an arrow-key option list, or a (maskable)
/// text field — a Claude-Code-style dynamic input.
fn render_flow(f: &mut Frame, area: Rect, ctrl: &input::Control) {
    let mag = Color::Indexed(165);
    let dim = Style::default().fg(Color::DarkGray);
    let title_st = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    match ctrl {
        input::Control::Select {
            title,
            options,
            cursor,
        } => {
            let mut lines = vec![Line::from(Span::styled(format!("⚙ {title}"), title_st))];
            for (i, opt) in options.iter().enumerate() {
                let st = if i == *cursor {
                    Style::default().fg(mag).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let mark = if i == *cursor { "▸ " } else { "  " };
                lines.push(Line::from(vec![
                    Span::styled(mark, st),
                    Span::styled(opt.clone(), st),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "↑↓ select · enter · esc cancel",
                dim,
            )));
            f.render_widget(Paragraph::new(lines), area);
        }
        input::Control::Field {
            title,
            value,
            masked,
        } => {
            let shown = if *masked {
                "•".repeat(value.chars().count())
            } else {
                value.clone()
            };
            let lines = vec![
                Line::from(Span::styled(format!("⚙ {title}"), title_st)),
                Line::from(vec![
                    Span::styled(" ▸ ", dim),
                    Span::styled(shown, Style::default().fg(Color::White)),
                ]),
                Line::from(Span::styled("enter · esc cancel", dim)),
            ];
            f.render_widget(Paragraph::new(lines), area);
            let cx = area.x + 3 + value.chars().count() as u16;
            f.set_cursor_position((cx.min(area.x + area.width.saturating_sub(1)), area.y + 1));
        }
        input::Control::Chat => {}
    }
}

/// Render the breathing PTY viewport: a colored `│ sh` rail header (showing
/// focus state), then the `tui-term` widget of the vt100 screen below it. A
/// short Rect shows the TOP of the screen — which is where shell output lives —
/// so a few lines of output render compactly while a full-screen TUI fills the
/// available height. When `focused`, keystrokes pipe straight to the PTY.
fn render_chat_terminal(f: &mut Frame, area: Rect, screen: &vt100::Screen, focused: bool) {
    let rail = chat::color_for("sh");
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let mut spans = vec![Span::styled(
        "│ sh",
        Style::default().fg(rail).add_modifier(Modifier::BOLD),
    )];
    if focused {
        spans.push(Span::styled(
            " ● focused (Esc to release)",
            Style::default().fg(rail),
        ));
    } else {
        spans.push(Span::styled(
            "  (Ctrl+F to focus)",
            Style::default().fg(Color::DarkGray),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), parts[0]);
    let pt = tui_term::widget::PseudoTerminal::new(screen);
    f.render_widget(pt, parts[1]);
}

/// Flatten the transcript into styled lines: each message renders a colored `│`
/// gutter in its source color, the source label, then the wrapped body. `you`
/// is bold/white; tool/note lines are dim; a live stream shows a trailing `▌`.
fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut out: Vec<Line> = Vec::new();
    for m in app.tabs.active().msgs() {
        let rail = chat::color_for(&m.from);
        let gutter = Span::styled("│ ", Style::default().fg(rail));
        let label_style = if m.from == "you" {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(rail).add_modifier(Modifier::BOLD)
        };
        let label = Span::styled(format!("{}: ", m.from), label_style);
        match &m.body {
            Body::Text(t) => {
                let live = matches!(m.state, State::Streaming);
                if live && t.is_empty() {
                    // No tokens yet — show the turn phase (sending → thinking →
                    // generating) with a spinner; tokens replace it as they stream.
                    let phase = app.ai_phase.as_deref().unwrap_or("thinking");
                    out.push(Line::from(vec![
                        gutter,
                        label,
                        Span::styled(format!("⟳ {phase}…"), dim.add_modifier(Modifier::ITALIC)),
                    ]));
                } else {
                    let body_style = match m.state {
                        // Failed lines read red; queued lines read dim until fired.
                        State::Error => Style::default().fg(Color::Red),
                        State::Queued => dim,
                        _ if m.from == "you" => Style::default().fg(Color::White),
                        _ => Style::default(),
                    };
                    // Preserve embedded newlines: one rendered line per text line
                    // (so poems / lists / code keep their breaks). The first line
                    // carries the `from:` label; continuation lines indent under it.
                    let segs: Vec<&str> = t.split('\n').collect();
                    let pad = " ".repeat(m.from.chars().count() + 2); // under "from: "
                    let last = segs.len() - 1;
                    for (i, seg) in segs.iter().enumerate() {
                        let mut seg = (*seg).to_string();
                        if i == last {
                            if live {
                                seg.push('▌');
                            }
                            if matches!(m.state, State::Interrupted) {
                                seg.push_str(" ⊘");
                            }
                            if matches!(m.state, State::Queued) {
                                seg.push_str(" ⏳");
                            }
                        }
                        if i == 0 {
                            out.push(Line::from(vec![
                                gutter.clone(),
                                label.clone(),
                                Span::styled(seg, body_style),
                            ]));
                        } else {
                            out.push(Line::from(vec![
                                gutter.clone(),
                                Span::styled(format!("{pad}{seg}"), body_style),
                            ]));
                        }
                    }
                }
            }
            Body::Tool {
                verb,
                target,
                summary,
            } => {
                // Activity line: `{from} → {target}: {verb} {summary}` — actor,
                // addressee, action. A shell command (verb == target) collapses
                // to `you → sh: ls -la` (no doubled name).
                let head = Span::styled(format!("{} → {}: ", m.from, target), label_style);
                let mut text = if verb == target {
                    String::new()
                } else {
                    format!("{verb} ")
                };
                text.push_str(summary);
                let text = text.trim_end().to_string();
                out.push(Line::from(vec![gutter, head, Span::styled(text, dim)]));
            }
            Body::Note(n) => {
                let st = if matches!(m.state, State::Error) {
                    Style::default().fg(Color::Red)
                } else {
                    dim
                };
                out.push(Line::from(vec![gutter, Span::styled(n.clone(), st)]));
            }
        }
    }
    out
}

#[cfg(test)]
mod e2e {
    //! Same-crate TUI e2e against ratatui's `TestBackend` — no tty, no network,
    //! no model. Builds a real `App` (private fields) over a REAL in-proc host
    //! kernel, renders frames into an in-memory buffer, and drives the actual
    //! `handle_input` path. Fully deterministic.
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// Build an `App` wired to a real host kernel, with dummy channels and no
    /// PTY. The kernel is composed via a blocking runtime so the helper itself
    /// is synchronous (the e2e tests need no async surface).
    fn test_app() -> App {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        let (kernel, _loaded) = rt
            .block_on(fantastic_host::compose_manager_in_memory())
            .expect("compose host kernel");
        let active = kernels::ActiveKernel::InProc(Arc::clone(&kernel));

        // Dummy channels: hold the receivers so the senders stay live (a dropped
        // receiver would make `send` error, but the e2e paths never rely on it).
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<(String, String)>();
        let (ai_tx, _ai_rx) = mpsc::unbounded_channel::<String>();
        let (ws_tx, _ws_rx) = mpsc::unbounded_channel::<WsEvent>();
        let (redraw_tx, _redraw_rx) = mpsc::unbounded_channel::<()>();

        App {
            kernel,
            active,
            tabs: Tabs::new(&BASE_TABS),
            composer: Composer::new("ai"),
            control: input::Control::Chat,
            palette_idx: 0,
            help_open: false,
            scroll: 0,
            scroll_max: std::cell::Cell::new(0),
            flow: None,
            known_agents: vec!["kernel".into(), "core".into()],
            pending: VecDeque::new(),
            ai_tx,
            ai_phase: None,
            chat_busy: false,
            workspace: None,
            ws_busy: false,
            ws_tx,
            cmd_tx,
            redraw_tx,
            chat_term_grid: (80, 24),
            brain_ready: false,
            term: None,
            term_active: false,
            term_focused: false,
            movie: movie::Movie::storyboard(),
            intro_playing: false,
            intro_since: None,
            // The chat e2e tests drive the chat path → start in a maximized Chat.
            // The attract/minimize/hub behavior is tested separately.
            phase: Phase::Chat,
            minimize_since: None,
            hub: hub::Hub::from_reflect(&json!({})),
            hub_dirty: false,
            kernels_view: kernels::KernelsView::default(),
            registry: registry::Registry::default(),
            kernels_dirty: false,
            browser: None,
            new_runtime: None,
            kernels_busy: false,
            kernels_msg: None,
            kernel_task_tx: mpsc::unbounded_channel::<KernelTaskResult>().0,
            last_activity: Instant::now(),
            boot: Instant::now(),
            attract_since: Instant::now(),
            quit: false,
            last_ctrl_c: None,
            q_streak: 0,
            last_q: None,
            q_armed: false,
        }
    }

    fn ctrl(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::CONTROL,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        })
    }

    /// Flatten a rendered buffer to a single string (cell symbols, row order).
    fn buffer_text(buf: &Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        })
    }

    #[test]
    fn renders_transcript_text_and_rail_glyph() {
        let mut app = test_app();
        app.tabs
            .deliver("ai")
            .push("you", "ai", Body::Text("ping-from-you".into()), State::Done);
        app.tabs.deliver("ai").push(
            "brain",
            "you",
            Body::Text("pong-from-ai".into()),
            State::Done,
        );

        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("test terminal");
        term.draw(|f| ui(f, &app)).expect("draw frame");

        let text = buffer_text(term.backend().buffer());
        assert!(
            text.contains("ping-from-you"),
            "rendered frame should contain the user message"
        );
        assert!(
            text.contains("pong-from-ai"),
            "rendered frame should contain the ai message"
        );
        assert!(
            text.contains('│'),
            "rendered frame should contain the per-source `│` rail glyph"
        );
    }

    #[test]
    fn multiline_answer_renders_one_line_per_break() {
        // A message body with embedded `\n` must render as separate transcript
        // lines (poems / lists / code keep their breaks), not one collapsed line.
        let mut app = test_app();
        app.tabs.deliver("ai").push(
            "brain",
            "you",
            Body::Text("alpha\nbeta\ngamma".into()),
            State::Done,
        );
        let texts: Vec<String> = transcript_lines(&app)
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        // Each segment lands on its OWN line (beta's line has no alpha, etc.).
        assert!(texts.iter().any(|t| t.contains("alpha")), "{texts:?}");
        assert!(
            texts
                .iter()
                .any(|t| t.contains("beta") && !t.contains("alpha")),
            "beta should be on its own line: {texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("gamma") && !t.contains("beta")),
            "gamma should be on its own line: {texts:?}"
        );
    }

    #[test]
    fn typed_char_appends_to_composer_message() {
        let mut app = test_app();
        assert!(app.composer.message.is_empty());
        handle_input(&mut app, key(KeyCode::Char('x')));
        assert_eq!(app.composer.message, "x");
        handle_input(&mut app, key(KeyCode::Char('y')));
        assert_eq!(app.composer.message, "xy");
    }

    #[test]
    fn shift_tab_turns_between_agents() {
        let mut app = test_app();
        assert_eq!(app.tabs.active_id(), "ai");
        handle_input(&mut app, key(KeyCode::BackTab));
        assert_eq!(
            app.tabs.active_id(),
            "sh",
            "Shift-Tab turns to the next agent"
        );
        assert_eq!(app.composer.sender, "sh", "and the composer follows");
    }

    #[test]
    fn nogo_blocks_send_to_unknown_character() {
        let mut app = test_app();
        // Retype the sender to a character that doesn't exist.
        app.composer.set_sender("ghost");
        for ch in "hello".chars() {
            handle_input(&mut app, key(KeyCode::Char(ch)));
        }
        handle_input(&mut app, key(KeyCode::Enter));
        assert!(app.composer.nogo, "send to an unknown character is a nogo");
        assert_eq!(
            app.composer.message, "hello",
            "the message is kept, not sent"
        );
    }

    #[test]
    fn slash_intro_plays_and_any_key_stops_it() {
        let mut app = test_app();
        assert!(!app.intro_playing);
        // Type `/intro` and submit it.
        for c in "/intro".chars() {
            handle_input(&mut app, key(KeyCode::Char(c)));
        }
        handle_input(&mut app, key(KeyCode::Enter));
        assert!(app.intro_playing, "`/intro` submit starts the movie");
        assert!(app.intro_since.is_some(), "the movie clock is armed");
        assert!(
            app.composer.message.is_empty(),
            "the input line is consumed"
        );

        // Any key stops it and returns to chat — and does NOT leak into input.
        handle_input(&mut app, key(KeyCode::Char('z')));
        assert!(!app.intro_playing, "any key stops the movie");
        assert!(
            app.composer.message.is_empty(),
            "the stop key does not edit input"
        );
    }

    #[test]
    fn first_key_on_attract_starts_minimize_then_hub_without_leaking() {
        // A fresh cabinet boots on the attract screen. The very first key starts
        // the minimize animation (consumed — must NOT leak into the input line);
        // a key during the minimize skips straight to the hub lobby.
        let mut app = test_app();
        app.phase = Phase::Attract;
        assert!(app.composer.message.is_empty());
        handle_input(&mut app, key(KeyCode::Char('x')));
        assert_eq!(
            app.phase,
            Phase::Minimizing,
            "the first key starts the minimize animation"
        );
        assert!(!app.intro_playing);
        assert!(
            app.composer.message.is_empty(),
            "the first key is consumed, not typed"
        );
        handle_input(&mut app, key(KeyCode::Char('y')));
        assert_eq!(app.phase, Phase::Hub, "a key during minimize opens the hub");
        assert!(
            app.composer.message.is_empty(),
            "hub keys never leak into the input"
        );
    }

    #[test]
    fn attract_render_then_chat_renders_over_bg() {
        // Attract screen: grid + big title + the blinking prompt.
        let mut app = test_app();
        app.phase = Phase::Attract;
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("test terminal");
        term.draw(|f| ui(f, &app)).expect("draw attract");
        let _ = buffer_text(term.backend().buffer());

        // A maximized Chat: the transcript renders opaquely over the same bg.
        app.phase = Phase::Chat;
        app.tabs.deliver("ai").push(
            "you",
            "ai",
            Body::Text("over-the-stars".into()),
            State::Done,
        );
        term.draw(|f| ui(f, &app)).expect("draw chat");
        let text = buffer_text(term.backend().buffer());
        assert!(
            text.contains("over-the-stars"),
            "the transcript renders over the animated background"
        );
    }

    #[test]
    fn ctrl_f_toggles_focus_only_with_live_terminal() {
        let mut app = test_app();
        // No terminal viewport → Ctrl+F is a no-op.
        assert!(!app.term_focused);
        handle_input(&mut app, ctrl(KeyCode::Char('f')));
        assert!(
            !app.term_focused,
            "Ctrl+F does nothing without a live terminal viewport"
        );

        // The toggle is gated on a live viewport (`term_active && term.is_some`).
        // Spawning a real PTY in a unit test is awkward, so assert the gate
        // directly: with no session, `term_live` is false regardless of the flag.
        app.term_active = true;
        assert!(
            !term_live(&app),
            "term_active alone is not a live viewport without a session"
        );
        handle_input(&mut app, ctrl(KeyCode::Char('f')));
        assert!(
            !app.term_focused,
            "Ctrl+F still no-ops while there is no PTY session"
        );
    }

    #[test]
    fn focused_terminal_suppresses_q_hold_exit() {
        // While focused, a held `q` must reach the PTY, NOT trip the exit streak.
        let mut app = test_app();
        app.term_focused = true;
        for _ in 0..(Q_HOLD_STREAK + 2) {
            handle_input(&mut app, key(KeyCode::Char('q')));
        }
        assert!(!app.quit, "held `q` while focused must not exit the app");
        assert_eq!(app.q_streak, 0, "the exit streak never accumulates");
    }

    #[test]
    fn esc_releases_terminal_focus() {
        let mut app = test_app();
        app.term_focused = true;
        handle_input(&mut app, key(KeyCode::Esc));
        assert!(!app.term_focused, "Esc releases focus back to chat");
    }
}

#[cfg(test)]
mod exit_tests {
    use super::*;

    #[test]
    fn ctrl_c_first_press_does_not_exit() {
        // No prior Ctrl+C → never exits on the first press.
        assert!(!ctrl_c_exits(None, Instant::now()));
    }

    #[test]
    fn ctrl_c_double_press_within_window_exits() {
        let t0 = Instant::now();
        let within = t0 + Duration::from_millis(DOUBLE_PRESS_MS - 1);
        assert!(ctrl_c_exits(Some(t0), within));
    }

    #[test]
    fn ctrl_c_second_press_after_window_does_not_exit() {
        // Too slow → the first press has lapsed; this is a fresh single press.
        let t0 = Instant::now();
        let after = t0 + Duration::from_millis(DOUBLE_PRESS_MS + 1);
        assert!(!ctrl_c_exits(Some(t0), after));
    }

    #[test]
    fn q_first_press_starts_streak_at_one() {
        assert_eq!(q_hold_streak(0, None, Instant::now()), 1);
    }

    #[test]
    fn q_fast_repeat_extends_streak() {
        let t0 = Instant::now();
        let fast = t0 + Duration::from_millis(Q_REPEAT_MS - 1);
        assert_eq!(q_hold_streak(3, Some(t0), fast), 4);
    }

    #[test]
    fn q_slow_press_resets_streak() {
        // A deliberate, slow `q` (e.g. typing) restarts the run, so it never
        // accumulates toward exit.
        let t0 = Instant::now();
        let slow = t0 + Duration::from_millis(Q_REPEAT_MS + 50);
        assert_eq!(q_hold_streak(5, Some(t0), slow), 1);
    }

    #[test]
    fn attract_idle_10s_starts_intro() {
        // Not started, not playing: 9.9s idle waits; 10.0s kicks the demo.
        assert_eq!(
            attract_tick(false, false, 9.9, 0.0, 27.4),
            AttractTick::Nothing
        );
        assert_eq!(
            attract_tick(false, false, 10.0, 0.0, 27.4),
            AttractTick::StartIntro
        );
    }

    #[test]
    fn attract_demo_end_returns_to_attract() {
        // Not started, playing: under the movie total keeps playing; at/over it
        // loops back to attract.
        assert_eq!(
            attract_tick(false, true, 0.0, 27.3, 27.4),
            AttractTick::Nothing
        );
        assert_eq!(
            attract_tick(false, true, 0.0, 27.4, 27.4),
            AttractTick::EndIntroToAttract
        );
        assert_eq!(
            attract_tick(false, true, 0.0, 30.0, 27.4),
            AttractTick::EndIntroToAttract
        );
    }

    #[test]
    fn attract_does_nothing_once_started() {
        // In chat (`started`): the attract machine never fires, regardless of
        // idle time or a manual `/intro` (governed by the movie's own loop).
        assert_eq!(
            attract_tick(true, false, 999.0, 0.0, 27.4),
            AttractTick::Nothing
        );
        assert_eq!(
            attract_tick(true, true, 0.0, 999.0, 27.4),
            AttractTick::Nothing
        );
    }

    #[test]
    fn q_held_reaches_exit_threshold() {
        // Simulate holding `q`: rapid repeats climb to the exit threshold.
        let mut streak = 0u8;
        let mut t = Instant::now();
        let mut last = None;
        for _ in 0..Q_HOLD_STREAK {
            streak = q_hold_streak(streak, last, t);
            last = Some(t);
            t += Duration::from_millis(Q_REPEAT_MS - 100); // auto-repeat cadence
        }
        assert!(
            streak >= Q_HOLD_STREAK,
            "held q should reach exit, got {streak}"
        );
    }
}
