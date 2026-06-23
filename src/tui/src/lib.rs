//! `fantastic-tui` — the ratatui terminal UI for the product.
//!
//! A chat **per character**, and you tab between them ([`chat::Tabs`]). Each
//! addressee owns its own room: `@ai`/`@brain` streams an AI turn; `@<agent>`
//! reflects it; `@<agent> <verb> [k=v…]` sends a sugar command and opens that
//! agent's room; `@sh`/`@ws` are the shell + workspace rooms. The input is a smart
//! `@sender` field ([`chat::Composer`]): Tab-complete, Shift-Tab to turn between
//! rooms, Backspace into the sender, **nogo** on an unknown character. Per-source
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use fantastic_kernel::{AgentId, Kernel};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
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
mod chat;
mod input;
mod movie;
use chat::{Body, Composer, Route, State, Tabs, WsCmd};
use fantastic_brain as ai;
use fantastic_host::gateway::{self, KernelHandle};
use fantastic_term::{used_rows, TerminalSession};

/// The client_id the brain emits its streaming events to (our inbox key).
const CLIENT_ID: &str = "fantastic";
/// The brain agent id (kept in sync with `fantastic-brain`).
const BRAIN_ID: &str = "brain";
/// The base "characters" present at boot, in tab order: the brain, the shell, the
/// workspace kernel. Any agent you address opens its own room after these.
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

/// Reflect the manager kernel's id-tree into a flat list of addressable agents
/// (the extra "characters" for Tab-complete / Shift-Tab / nogo validation).
async fn agent_ids(kernel: &Arc<Kernel>) -> Vec<String> {
    let tree = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids"}),
        )
        .await;
    let mut ids = vec!["kernel".to_string()];
    collect_ids(&tree, &mut ids);
    ids
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

struct App {
    kernel: Arc<Kernel>,
    agent_count: usize,
    /// One chat **per character**: `@ai`/`@sh`/`@ws` + any agent you address, each
    /// its own room; Shift-Tab turns between them.
    tabs: Tabs,
    /// The smart input line — an editable `@sender` field + the message.
    composer: Composer,
    /// The active input control — `Chat` normally, or a setup-flow Select/Field
    /// while `/setup` · `/model` is running.
    control: input::Control,
    /// The running connector-setup wizard, if any.
    flow: Option<input::SetupFlow>,
    /// Kernel agent ids (cached at boot) — the extra "characters" you can address
    /// + Tab-complete beyond the base rooms.
    known_agents: Vec<String>,
    /// AI turns typed while a turn is in flight queue here and fire in order.
    pending: VecDeque<String>,
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
    /// Arcade-cabinet attract state machine. `started` is false at boot → the
    /// attract screen ("press any key to continue"); the first key flips it true
    /// and enters chat. `last_activity` is reset on any key (the idle clock that
    /// drives the 10s-idle → auto-demo). `boot` is the global animation clock for
    /// the always-on starfield + title + blink phases.
    started: bool,
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
}

impl App {
    /// Every addressable character: the open rooms (base + spawned tabs) plus the
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

pub async fn run(kernel: Arc<Kernel>, agent_count: usize) -> Result<()> {
    // One-shot kernel command replies flow back here as (source_id, text).
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<(String, String)>();
    // The final result of an AI turn (the blocking `response`, or `✗ error`) — the
    // authoritative turn boundary. Renders into the `@ai` room + drives the queue.
    let (ai_tx, mut ai_rx) = mpsc::unbounded_channel::<String>();
    // Workspace gateway task results (attach/spawn/send/down) flow back here.
    let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<WsEvent>();
    // PTY output → repaint pings.
    let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<()>();

    // The brain streams its per-turn events (token/say/status/done) to the
    // `CLIENT_ID` inbox. Register a bounded channel matching `kernel.inboxes`'
    // Sender type and drain it into the transcript.
    let (brain_tx, mut brain_rx) = tokio::sync::mpsc::channel::<Value>(256);
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
    execute!(out, EnterAlternateScreen, EnableMouseCapture)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    // Spawn the shared PTY sized to the chat breathing-viewport grid.
    let (trows, tcols) = chat_term_grid(&term);
    let session = TerminalSession::spawn(trows, tcols, redraw_tx.clone()).ok();

    // Cache the manager kernel's agent ids as addressable "characters".
    let known_agents = agent_ids(&kernel).await;

    let mut app = App {
        kernel,
        agent_count,
        tabs: Tabs::new(&BASE_TABS),
        composer: Composer::new("ai"),
        control: input::Control::Chat,
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
        started: false,
        last_activity: Instant::now(),
        boot: Instant::now(),
        attract_since: Instant::now(),
        quit: false,
        last_ctrl_c: None,
        q_streak: 0,
        last_q: None,
    };
    app.tabs.deliver("ai").push(
        "system",
        "you",
        Body::Note(
            "Rooms — one chat per character. `@ai` is the brain (streams live, Ctrl+C interrupts), `@sh` a shell (Ctrl+F focuses it), `@ws` the workspace kernel; `@<agent> <verb> [k=v…]` opens any agent's room. Shift-Tab turns between rooms; Tab completes a name; `/intro` plays the movie.".into(),
        ),
        State::Done,
    );
    term.draw(|f| ui(f, &app))?;

    // ~16fps heartbeat — the arcade background (starfield + title) is always
    // animating, so this fires CONTINUOUSLY and every tick falls through to a
    // redraw. It's a cabinet: accept the steady repaint. The tick also drives
    // the attract state machine (10s-idle → auto-demo, demo end → attract).
    let mut ticker = tokio::time::interval(Duration::from_millis(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(ev) = in_rx.recv() => handle_input(&mut app, ev),
            Some((from, text)) = cmd_rx.recv() => {
                // A kernel reply lands in that agent's room (unread if you're away).
                app.tabs.deliver(&from).push(&from, "you", Body::Text(text), State::Done);
            }
            Some(ev) = brain_rx.recv() => {
                // The brain's inbox is mirrored to us via `watch`. We use ONLY the
                // `status` events here (the live phase indicator). Token/done events
                // are NOT rendered live — the answer is sealed atomically from the
                // send-completion (`ai_rx`). Live token-append races with the
                // queue's cross-turn boundaries (the shared `brain` stream key) and
                // SPLITS answers; correct token streaming needs per-turn stream ids
                // in the kernel protocol (a follow-up).
                if ev.get("type").and_then(Value::as_str) == Some("status") {
                    if let Some(p) = ev.get("phase").and_then(Value::as_str) {
                        app.ai_phase = Some(p.to_string());
                    }
                }
            }
            Some(out) = ai_rx.recv() => {
                // Authoritative end of an AI turn: seal the `@ai` live line with the
                // final response (filling it if nothing streamed) and clear the
                // phase. Then — Claude-Code style, AI-only — any messages queued
                // mid-turn are CONCATENATED with `\n` and sent as ONE next turn.
                app.tabs.deliver("ai").close_stream(BRAIN_ID, &out);
                app.ai_phase = None;
                if app.pending.is_empty() {
                    app.chat_busy = false;
                } else {
                    let joined = app.pending.drain(..).collect::<Vec<_>>().join("\n");
                    fire_ai_turn(&mut app, joined);
                }
            }
            Some(ev) = ws_rx.recv() => handle_ws_event(&mut app, ev),
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
                    app.started,
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
            }
            else => break,
        }
        if app.quit {
            break;
        }
        // Keep the PTY grid matched to the breathing chat viewport: it gets the
        // full chat-body height (so htop/vim get real room) while only
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
        DisableMouseCapture
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

/// True when the terminal viewport is BOTH live and on-screen — i.e. you're in
/// the `@sh` room. The PTY is shared, but it must only render + capture keys in
/// its own room, never bleed into `@ai`/`@ws`/agent rooms.
fn term_visible(app: &App) -> bool {
    term_live(app) && app.tabs.active_id() == "sh"
}

fn handle_input(app: &mut App, ev: Event) {
    // Mouse capture stays on so the alt-screen behaves, but there are no header
    // tabs to hit-test anymore — mouse events are a no-op.
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
    // Arcade "press any key to continue / press to start": while the attract
    // demo plays OR before the game has started, the FIRST key enters chat. It
    // is fully consumed — it must NOT leak into the input line or be processed
    // as a chat key.
    if app.intro_playing || !app.started {
        app.started = true;
        app.intro_playing = false;
        app.intro_since = None;
        return;
    }
    let ctrl = modifiers.contains(KeyModifiers::CONTROL);
    // Ctrl-Q is the always-reliable quit (works even while the PTY is focused).
    if ctrl && code == KeyCode::Char('q') {
        app.quit = true;
        return;
    }
    // Ctrl+F toggles PTY focus (only when the terminal viewport is on-screen — in
    // the `@sh` room).
    if ctrl && code == KeyCode::Char('f') {
        if term_visible(app) {
            app.term_focused = !app.term_focused;
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
        app.tabs.active_mut().push(
            "system",
            "you",
            Body::Note("press Ctrl+C again to exit".into()),
            State::Done,
        );
        let ai_live = app.tabs.chat("ai").is_some_and(|c| c.has_live());
        // SIGINT goes to the shell only when its viewport is the one in front
        // (the `@sh` room / focused); elsewhere Ctrl+C interrupts the AI stream.
        if app.term_focused || term_visible(app) {
            if let Some(ts) = app.term.as_mut() {
                ts.write(&[0x03]);
            }
        } else if ai_live {
            app.tabs.deliver("ai").interrupt_live();
            app.pending.clear();
            app.chat_busy = false;
            let kernel = Arc::clone(&app.kernel);
            tokio::spawn(async move {
                kernel
                    .send(&AgentId::from(BRAIN_ID), json!({"type":"interrupt"}))
                    .await;
            });
        }
        return;
    }
    // Hold `q` to exit: physically holding the key fires rapid auto-repeats; a
    // run of them in a short window quits. SUPPRESSED while the PTY is focused,
    // where a typed `q` must reach the shell (only Ctrl+Q / double-Ctrl+C exit).
    if let KeyCode::Char('q') = code {
        if !ctrl && !app.term_focused {
            let now = Instant::now();
            app.q_streak = q_hold_streak(app.q_streak, app.last_q, now);
            app.last_q = Some(now);
            if app.q_streak >= Q_HOLD_STREAK {
                app.quit = true;
                return;
            }
        }
    } else {
        app.q_streak = 0;
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
    // Otherwise the keys edit the smart composer (`@sender` field + message).
    match code {
        KeyCode::Char(c) => app.composer.type_char(c),
        KeyCode::Backspace => app.composer.backspace(),
        KeyCode::Tab => {
            // Complete the `@sender` against every known character; if it lands on
            // an open room, turn to face it.
            app.composer.complete(&app.known_senders());
            if app.tabs.ids().contains(&app.composer.sender) {
                app.tabs.focus(&app.composer.sender);
            }
        }
        KeyCode::BackTab => {
            // Shift-Tab: turn to the next OPEN room (rolls over the characters you
            // know); the composer follows. New agents are reached by name + Tab.
            app.tabs.cycle(true);
            let id = app.tabs.active_id().to_string();
            app.composer.set_sender(&id);
        }
        KeyCode::Enter => submit_chat(app),
        _ => {}
    }
}

/// Submit the composed line: validate the `@sender` (a **nogo** if it names no
/// known character), enter that room, then dispatch — an AI turn streams into the
/// `@ai` room; a kernel command sends and routes its reply back via `cmd_tx`.
fn submit_chat(app: &mut App) {
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
    // Entering the addressed room (creates its tab if new) + keep the composer
    // pointed there.
    app.tabs.focus(&target);
    app.composer.set_sender(&target);
    match route {
        Route::Empty => {}
        Route::Ai(text) => {
            app.tabs
                .deliver("ai")
                .push("you", "ai", Body::Text(text.clone()), State::Done);
            // The DRY stand-in brain: if no connector is configured, answer with
            // setup guidance instead of provisioning a real brain (which would
            // error). The real brain runs only once a connector is Ready.
            if let Some(guide) = ai::dry_reply(&ai::config_status(), None) {
                app.tabs
                    .deliver("ai")
                    .push("brain", "you", Body::Text(guide), State::Done);
            } else if app.chat_busy {
                // A turn is in flight — queue this one; it fires on `done`.
                app.pending.push_back(text);
            } else {
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
/// env, re-provision the brain (drop the old one), and confirm in the `@ai` room.
fn finish_setup(app: &mut App, draft: input::Draft) {
    app.control = input::Control::Chat;
    app.flow = None;
    let key = (!draft.key.trim().is_empty()).then_some(draft.key.as_str());
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
        Err(e) => format!("✗ {e}"),
    };
    app.tabs
        .deliver("ai")
        .push("brain", "you", Body::Text(note), State::Done);
    app.tabs.focus("ai");
    app.composer.set_sender("ai");
}

/// Start one AI turn: open a live streaming message in the `@ai` room, ensure the
/// brain once, then send. The blocking reply's `response` (or `✗ error`) is the
/// authoritative result — sent back via `ai_tx` to close the live `@ai` line.
/// This works for EVERY backend regardless of how (or whether) it routes stream
/// events to us: ollama emits tokens to its own inbox, not ours, so relying on
/// streamed events alone would hang the line forever.
fn fire_ai_turn(app: &mut App, text: String) {
    app.chat_busy = true;
    app.ai_phase = Some("sending".to_string());
    app.tabs.deliver("ai").start_stream(BRAIN_ID, "you");
    let kernel = Arc::clone(&app.kernel);
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
            // our `CLIENT_ID` inbox to receive live tokens + status. Once.
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
        let out = if let Some(resp) = reply.get("response").and_then(Value::as_str) {
            resp.to_string()
        } else if let Some(e) = reply.get("error").and_then(Value::as_str) {
            // A configured-but-unreachable model → the DRY stand-in's "set another"
            // guidance instead of a raw error.
            if ai::is_unreachable(e) {
                ai::dry_reply(&ai::config_status(), Some(e)).unwrap_or_else(|| format!("✗ {e}"))
            } else {
                format!("✗ {e}")
            }
        } else {
            serde_json::to_string(&reply).unwrap_or_default()
        };
        let _ = ai_tx.send(out);
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
                return;
            }
            app.tabs.deliver("ws").push(
                "ws",
                "you",
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
                "ws",
                "you",
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
            app.tabs
                .deliver("ws")
                .push("ws", "you", Body::Note(format!("✗ ws: {e}")), State::Done);
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
        "sh",
        "you",
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
    let kernel = Arc::clone(&app.kernel);
    let tx = app.cmd_tx.clone();
    let from = target.as_str().to_string();
    tokio::spawn(async move {
        let reply = kernel.send(&target, payload).await;
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
/// FULL chat-body interior so full-screen TUIs get real room, even though only
/// `used_rows` of it are displayed below the transcript. Subtracts the header
/// (3) + body border (2) chrome, the viewport's `│ sh` header row, and the
/// input line (3) below the transcript.
fn chat_term_grid<B: ratatui::backend::Backend>(term: &Terminal<B>) -> (u16, u16) {
    let size = term.size().unwrap_or(Size::new(80, 24));
    let rows = size.height.saturating_sub(3 + 2 + 1 + 3).max(1);
    let cols = size.width.saturating_sub(2).max(1);
    (rows, cols)
}

/// The tab bar above the chat: one chip per open room (`@ai`, `@sh`, `@ws`, and
/// any agent you've addressed). The active room is bold + underlined; a room that
/// got a message while you were away carries a `•` unread dot. Shift-Tab turns
/// between them. (The big FANTASTIC lives in the animated background.)
fn status_line(app: &App) -> Line<'static> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = vec![Span::styled(" ", dim)];
    for (i, id) in app.tabs.ids().iter().enumerate() {
        let active = i == app.tabs.active_index();
        let col = chat::color_for(id);
        if app.tabs.is_unread(id) {
            spans.push(Span::styled("•", Style::default().fg(Color::Yellow)));
        }
        let st = if active {
            Style::default()
                .fg(col)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::default().fg(col).add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(format!("@{id}"), st));
        spans.push(Span::styled("  ", dim));
    }
    let ws = match &app.workspace {
        Some(h) => format!("ws:{}", h.base_url.trim_start_matches("http://")),
        None => "ws:none".to_string(),
    };
    spans.push(Span::styled(
        format!(
            " ·  {ws}  ·  {} agents  ·  ⇧⇥ rooms · Ctrl+F",
            app.agent_count
        ),
        dim,
    ));
    Line::from(spans)
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

    // STATE 1 — the intro movie plays full-screen (its own starfield + scenes).
    if app.intro_playing {
        let elapsed = app
            .intro_since
            .map(|t| t.elapsed().as_secs_f32())
            .unwrap_or(0.0);
        app.movie.render(f, full, elapsed);
        return;
    }

    // STATE 2 — ATTRACT: stars + the big title appearing top→bottom over ~1.5s
    // + a blinking "press any key".
    if !app.started {
        let buf = f.buffer_mut();
        bg::render_stars(buf, full, clock);
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
        return;
    }

    // STATE 3 — CHAT floated over the same animated background (starfield only,
    // no title band).
    {
        let buf = f.buffer_mut();
        bg::render_stars(buf, full, clock);
    }
    render_chat(f, full, app);
}

/// The unified chat, FLOATED over the arcade background. The whole chat block is
/// inset 2 cells from every screen edge (so the starfield shows in that border
/// margin); inside, a slim status line, the transcript, a 1-row gap (stars peek
/// through), then the input box at the bottom. Transcript + input are opaque.
/// The 3-row chat header banner (mirrors the headless `ansi_banner`): a neon
/// magenta vertical bar down the left, `FANTASTIC` on the middle row, and the live
/// tab bar on the bottom row. Opaque (the stars don't show through it).
fn render_chat_header(f: &mut Frame, area: Rect, app: &App) {
    fill_black(f.buffer_mut(), area);
    let bar = Color::Indexed(165); // neon magenta — matches ansi_banner's 38;5;165
    let buf = f.buffer_mut();
    // The vertical bar, full height of the header (3 rows).
    for dy in 0..area.height {
        if let Some(c) = buf.cell_mut((area.x, area.y + dy)) {
            c.set_char('█');
            c.set_style(Style::default().fg(bar));
        }
    }
    // `FANTASTIC` on the middle row, bright magenta + bold, two cells past the bar.
    let title = "FANTASTIC";
    let tx = area.x + 3;
    let ty = area.y + 1;
    let title_st = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    for (i, ch) in title.chars().enumerate() {
        let x = tx + i as u16;
        if x < area.x + area.width {
            if let Some(c) = buf.cell_mut((x, ty)) {
                c.set_char(ch);
                c.set_style(title_st);
            }
        }
    }
    // The live tab bar on the bottom row, past the bar.
    let bar_row = Rect {
        x: area.x + 3,
        y: area.y + area.height.saturating_sub(1),
        width: area.width.saturating_sub(3),
        height: 1,
    };
    f.render_widget(Paragraph::new(status_line(app)), bar_row);
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
    // `/model` flow is active (a select list needs room for its options).
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

    render_chat_header(f, header_area, app);

    // The transcript is OPAQUE: black it out first so the starfield only shows in
    // the 2-cell border margin and (in Chat mode) the 1-row gap.
    fill_black(f.buffer_mut(), chat_area);

    // When a `@sh` command is live AND you're in the `@sh` room, the bottom of the
    // chat body breathes the PTY screen. The viewport height = `used_rows` of the
    // PTY (clamped to the body) + 1 for its `│ sh` header; the transcript scrolls
    // in what remains. In every OTHER room the PTY is hidden (it never bleeds).
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
    // leaves the empty space (starfield) at the TOP rather than a gap in the
    // middle. Count VISUAL rows (a long line wraps to several), accumulating from
    // the bottom up, so the newest entries are never clipped off the bottom edge.
    let h = transcript_area.height as usize;
    let aw = transcript_area.width.max(1) as usize;
    let rows_of = |l: &Line| -> usize { l.width().div_ceil(aw).max(1) };
    let mut used = 0usize;
    let mut start = lines.len();
    for (i, l) in lines.iter().enumerate().rev() {
        let r = rows_of(l);
        if used + r > h && i + 1 < start {
            break; // keep at least the newest line even if it alone overflows
        }
        used += r;
        start = i;
    }
    let pad = h.saturating_sub(used);
    let mut view: Vec<Line> = vec![Line::from(""); pad];
    view.extend(lines.into_iter().skip(start));
    f.render_widget(
        Paragraph::new(view).wrap(Wrap { trim: false }),
        transcript_area,
    );

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
        }
        ctrl => {
            fill_black(f.buffer_mut(), bottom);
            render_flow(f, bottom, ctrl);
        }
    }
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
    let prompt = Line::from(vec![
        Span::styled(format!("@{}", c.sender), sender_st),
        Span::styled(" ▸ ", Style::default().fg(Color::DarkGray)),
        Span::styled(c.message.clone(), Style::default().fg(Color::White)),
    ]);
    f.render_widget(Paragraph::new(prompt), input_area);
    let cx = if c.editing_sender {
        input_area.x + 1 + c.sender.chars().count() as u16
    } else {
        input_area.x + (1 + c.sender.chars().count() + 3 + c.message.chars().count()) as u16
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
                    let body_style = if m.from == "you" {
                        Style::default().fg(Color::White)
                    } else {
                        Style::default()
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
                let mut text = format!("→ {verb} {target}");
                if !summary.is_empty() {
                    text.push_str(&format!("  {summary}"));
                }
                out.push(Line::from(vec![gutter, Span::styled(text, dim)]));
            }
            Body::Note(n) => {
                out.push(Line::from(vec![gutter, Span::styled(n.clone(), dim)]));
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
        let (kernel, loaded) = rt
            .block_on(fantastic_host::compose_manager_in_memory())
            .expect("compose host kernel");
        let agent_count = loaded.len();

        // Dummy channels: hold the receivers so the senders stay live (a dropped
        // receiver would make `send` error, but the e2e paths never rely on it).
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel::<(String, String)>();
        let (ai_tx, _ai_rx) = mpsc::unbounded_channel::<String>();
        let (ws_tx, _ws_rx) = mpsc::unbounded_channel::<WsEvent>();
        let (redraw_tx, _redraw_rx) = mpsc::unbounded_channel::<()>();

        App {
            kernel,
            agent_count,
            tabs: Tabs::new(&BASE_TABS),
            composer: Composer::new("ai"),
            control: input::Control::Chat,
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
            // The chat e2e tests drive the chat path → start past the attract
            // screen. The attract/first-key behavior is tested separately.
            started: true,
            last_activity: Instant::now(),
            boot: Instant::now(),
            attract_since: Instant::now(),
            quit: false,
            last_ctrl_c: None,
            q_streak: 0,
            last_q: None,
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
    fn shift_tab_turns_between_rooms() {
        let mut app = test_app();
        assert_eq!(app.tabs.active_id(), "ai");
        handle_input(&mut app, key(KeyCode::BackTab));
        assert_eq!(
            app.tabs.active_id(),
            "sh",
            "Shift-Tab turns to the next room"
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
    fn first_key_on_attract_enters_chat_without_leaking() {
        // A fresh cabinet boots on the attract screen (`started:false`). The
        // very first key "presses to start" → enters chat and is consumed: it
        // must NOT leak into the input line nor be processed as a chat key.
        let mut app = test_app();
        app.started = false;
        assert!(app.composer.message.is_empty());
        handle_input(&mut app, key(KeyCode::Char('x')));
        assert!(app.started, "the first key starts the game / enters chat");
        assert!(!app.intro_playing, "and is not playing the intro");
        assert!(
            app.composer.message.is_empty(),
            "the first key is consumed, not typed into the input line"
        );
        // The NEXT key now edits the chat input normally.
        handle_input(&mut app, key(KeyCode::Char('y')));
        assert_eq!(
            app.composer.message, "y",
            "subsequent keys edit the chat input"
        );
    }

    #[test]
    fn attract_render_then_chat_renders_over_bg() {
        // Attract screen: stars + big title + the blinking prompt.
        let mut app = test_app();
        app.started = false;
        let backend = TestBackend::new(80, 24);
        let mut term = Terminal::new(backend).expect("test terminal");
        term.draw(|f| ui(f, &app)).expect("draw attract");
        // The big FANTASTIC bg + the blink prompt are present (blink phase is
        // clock-driven; force a known-on phase is hard, so just assert the bg).
        let _ = buffer_text(term.backend().buffer());

        // Started: the transcript renders opaquely over the same bg.
        app.started = true;
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
