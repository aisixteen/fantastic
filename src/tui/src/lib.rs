//! `fantastic-tui` — the ratatui terminal UI for the product.
//!
//! Modes — Shift+Tab cycles, or click a header tab: Chat / Terminal / Intro.
//! - **Chat**: ONE transcript that unifies the AI brain and the kernel manager.
//!   Every line routes by `@target`: `@ai …`/`@brain …` streams an AI turn;
//!   `@<agent>` reflects it; `@<agent> <verb> [k=v…]` sends a sugar command.
//!   With no `@` the line goes to the sticky target. Per-source colored rails
//!   keep agents distinct; AI turns stream live and Ctrl+C interrupts.
//! - **Terminal**: a real PTY (`$SHELL`).
//! - **Intro**: a scripted retro "movie" of how Fantastic works (see `movie.rs`).

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
        KeyModifiers, MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use serde_json::{json, Value};
use tokio::sync::mpsc;

mod chat;
mod intro;
mod movie;
use chat::{Body, Route, State, Transcript};
use fantastic_brain as ai;
use fantastic_term::TerminalSession;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Chat,
    Terminal,
    Intro,
}

impl Mode {
    fn next(self) -> Self {
        match self {
            Mode::Chat => Mode::Terminal,
            Mode::Terminal => Mode::Intro,
            Mode::Intro => Mode::Chat,
        }
    }
}

/// The mode tabs, in header order — the single source of truth shared by the
/// header renderer (`brand_header`) and the click hit-test (`tab_at`), so their
/// column math can never drift apart.
const MODE_TABS: [(&str, Mode); 3] = [
    ("Chat", Mode::Chat),
    ("Terminal", Mode::Terminal),
    ("Intro", Mode::Intro),
];

/// The client_id the brain emits its streaming events to (our inbox key).
const CLIENT_ID: &str = "fantastic";
/// The brain agent id (kept in sync with `fantastic-brain`).
const BRAIN_ID: &str = "brain";

/// The frame row the tab labels render on (header line 2 of 3: bar / tabs / hint).
const TAB_ROW: u16 = 1;
/// Fixed prefix on the tab line: `"  █  "` (5) + `"FANTASTIC"` (9).
const TAB_PREFIX: usize = 14;

/// Hit-test a click at `(col, row)` against the mode tabs. Mirrors the exact
/// span widths laid down by `brand_header`. Returns the clicked `Mode`, if any.
fn tab_at(agent_count: usize, col: u16, row: u16) -> Option<Mode> {
    if row != TAB_ROW {
        return None;
    }
    let host = format!("    host: {agent_count} agents    ");
    let mut x = TAB_PREFIX + host.chars().count();
    for (i, (label, mode)) in MODE_TABS.iter().enumerate() {
        let w = label.chars().count() + 2; // " label "
        if (col as usize) >= x && (col as usize) < x + w {
            return Some(*mode);
        }
        x += w;
        if i < MODE_TABS.len() - 1 {
            x += 3; // " · " separator
        }
    }
    None
}

struct App {
    kernel: Arc<Kernel>,
    agent_count: usize,
    mode: Mode,
    events: VecDeque<String>,
    /// Chat mode: the one unified transcript + its input line + sticky target.
    chat: Transcript,
    input: String,
    sticky: String,
    chat_busy: bool,
    /// Reply channel for one-shot kernel commands (reflect / sugar verbs).
    cmd_tx: mpsc::UnboundedSender<(String, String)>,
    /// True once the brain has been provisioned (so we only ensure it once).
    brain_ready: bool,
    /// Terminal-proxy mode PTY (spawned at startup).
    term: Option<TerminalSession>,
    /// Intro mode: the scripted movie + when it (re)started (for its clock).
    movie: movie::Movie,
    intro_since: Option<Instant>,
    quit: bool,
    /// Exit affordances. `last_ctrl_c`: a second Ctrl+C within the window quits
    /// (a single one still reaches the shell in terminal mode). `q_streak`:
    /// count of rapid `q` auto-repeats (holding q) — normal typing resets it.
    last_ctrl_c: Option<Instant>,
    q_streak: u8,
    last_q: Option<Instant>,
}

impl App {
    fn push_event(&mut self, line: String) {
        cap_push(&mut self.events, line, 500);
    }
}

fn cap_push(buf: &mut VecDeque<String>, line: String, cap: usize) {
    if buf.len() >= cap {
        buf.pop_front();
    }
    buf.push_back(line);
}

/// One-line render of a kernel state event for the log pane.
fn fmt_event(e: &Value) -> String {
    let t = e.get("type").and_then(Value::as_str).unwrap_or("?");
    let id = e
        .get("id")
        .or_else(|| e.get("agent_id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let target = e.get("target").and_then(Value::as_str).unwrap_or("");
    let mut s = format!("[{t}]");
    if !id.is_empty() {
        s.push(' ');
        s.push_str(id);
    }
    if !target.is_empty() {
        s.push_str(" → ");
        s.push_str(target);
    }
    s
}

pub async fn run(kernel: Arc<Kernel>, agent_count: usize) -> Result<()> {
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<String>();
    let _tok = kernel.add_state_subscriber(Arc::new(move |e: &Value| {
        let _ = evt_tx.send(fmt_event(e));
    }));

    // One-shot kernel command replies flow back here as (source_id, text).
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<(String, String)>();
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
    intro::play(); // modular startup flourish — remove this line + intro.rs to disable
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    // Spawn the terminal-proxy PTY at the body size.
    let (trows, tcols) = term_grid(&term);
    let session = TerminalSession::spawn(trows, tcols, redraw_tx.clone()).ok();

    let mut app = App {
        kernel,
        agent_count,
        mode: Mode::Chat,
        events: VecDeque::new(),
        chat: Transcript::new(),
        input: String::new(),
        sticky: "ai".into(),
        chat_busy: false,
        cmd_tx,
        brain_ready: false,
        term: session,
        movie: movie::Movie::storyboard(),
        intro_since: None,
        quit: false,
        last_ctrl_c: None,
        q_streak: 0,
        last_q: None,
    };
    app.push_event(
        "host kernel composed — Shift+Tab switches modes · double Ctrl+C / hold q / Ctrl-Q to exit"
            .into(),
    );
    app.chat.push(
        "system",
        "you",
        Body::Note(
            "Chat — `@ai …` talks to the brain (streams live, Ctrl+C interrupts); `@<agent>` reflects it; `@<agent> <verb> [k=v…]` sends a command. No `@` reuses the last target.".into(),
        ),
        State::Done,
    );
    term.draw(|f| ui(f, &app))?;

    // ~16fps heartbeat that only matters in Intro mode (the movie's frame clock).
    // In every other mode the tick `continue`s before the redraw, so it costs
    // nothing — those modes still repaint on their own events.
    let mut ticker = tokio::time::interval(Duration::from_millis(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(ev) = in_rx.recv() => handle_input(&mut app, ev),
            Some(line) = evt_rx.recv() => app.push_event(line),
            Some((from, text)) = cmd_rx.recv() => {
                app.chat.push(&from, "you", Body::Text(text), State::Done);
            }
            Some(ev) = brain_rx.recv() => {
                app.chat.on_event(&ev);
                if !app.chat.has_live() {
                    app.chat_busy = false;
                }
            }
            Some(()) = redraw_rx.recv() => {}
            _ = ticker.tick() => {
                if app.mode != Mode::Intro { continue; }
            }
            else => break,
        }
        if app.quit {
            break;
        }
        // Keep the PTY grid matched to the terminal pane.
        if app.mode == Mode::Terminal {
            let (r, c) = term_grid(&term);
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

fn handle_input(app: &mut App, ev: Event) {
    // Mouse: a left-click on a header mode tab switches to that mode.
    if let Event::Mouse(m) = ev {
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(mode) = tab_at(app.agent_count, m.column, m.row) {
                app.mode = mode;
                app.intro_since = (app.mode == Mode::Intro).then(Instant::now);
            }
        }
        return;
    }
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
    // Global keys. Ctrl-Q is the reliable quit. Shift+Tab cycles modes.
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('q') {
        app.quit = true;
        return;
    }
    if code == KeyCode::BackTab {
        app.mode = app.mode.next();
        // Start (or stop) the movie clock as we enter/leave Intro.
        app.intro_since = (app.mode == Mode::Intro).then(Instant::now);
        return;
    }
    // Ctrl+C: a SECOND press within the window exits the app. A single press
    // still does its normal job — in terminal mode it's forwarded to the shell
    // as SIGINT (0x03) — so this stays terminal-compatible. Other modes just arm
    // the double-press and show the hint.
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        let now = Instant::now();
        if ctrl_c_exits(app.last_ctrl_c, now) {
            app.quit = true;
            return;
        }
        app.last_ctrl_c = Some(now);
        app.push_event("press Ctrl+C again to exit".into());
        match app.mode {
            // In the live PTY, forward SIGINT to the shell.
            Mode::Terminal => {
                if let Some(ts) = app.term.as_mut() {
                    ts.write(&[0x03]);
                }
            }
            // In chat, interrupt an in-flight AI stream (do NOT exit).
            Mode::Chat if app.chat.has_live() => {
                app.chat.interrupt_live();
                app.chat_busy = false;
                let kernel = Arc::clone(&app.kernel);
                tokio::spawn(async move {
                    kernel
                        .send(&AgentId::from(BRAIN_ID), json!({"type":"interrupt"}))
                        .await;
                });
            }
            _ => {}
        }
        return;
    }
    // Hold `q` to exit: physically holding the key fires rapid auto-repeats; a
    // run of them in a short window quits. A normal `q` (followed by any other
    // key, which resets the streak) types as usual, so prompts stay usable.
    if let KeyCode::Char('q') = code {
        if !modifiers.contains(KeyModifiers::CONTROL) {
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
    match app.mode {
        Mode::Terminal => {
            if let Some(ts) = app.term.as_mut() {
                if let Some(bytes) = encode_key(code, modifiers) {
                    ts.write(&bytes);
                }
            }
        }
        Mode::Chat => match code {
            KeyCode::Char(c) => app.input.push(c),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Enter => submit_chat(app),
            _ => {}
        },
        Mode::Intro => {
            // Space/Enter replays the movie from the top.
            if matches!(code, KeyCode::Char(' ') | KeyCode::Enter) {
                app.intro_since = Some(Instant::now());
            }
        }
    }
}

/// Submit the chat input line: resolve its `@`-route, update the sticky target,
/// and dispatch — an AI turn streams into the transcript; a kernel command sends
/// and routes its reply back via `cmd_tx`.
fn submit_chat(app: &mut App) {
    let line = std::mem::take(&mut app.input);
    let (sticky, route) = chat::route(&line, &app.sticky);
    app.sticky = sticky;
    match route {
        Route::Empty => {}
        Route::Ai(text) => {
            if app.chat_busy {
                // Restore the line so the user doesn't lose it.
                app.input = line;
                return;
            }
            app.chat
                .push("you", "ai", Body::Text(text.clone()), State::Done);
            app.chat_busy = true;
            // Open the live streaming message; events fill it via the inbox.
            app.chat.start_stream(BRAIN_ID, "you");
            let kernel = Arc::clone(&app.kernel);
            let ensure = !app.brain_ready;
            app.brain_ready = true;
            tokio::spawn(async move {
                if ensure {
                    if let Err(e) = ai::ensure_brain(&kernel).await {
                        // The interrupt path also clears `live`; surface the
                        // provisioning error to the same inbox so it renders.
                        if let Some(tx) = kernel.inboxes.get(&AgentId::from(CLIENT_ID)) {
                            let _ = tx
                                .send(json!({
                                    "type":"say","source":BRAIN_ID,"client_id":CLIENT_ID,
                                    "text": format!("✗ {e}"),
                                }))
                                .await;
                            let _ = tx
                                .send(
                                    json!({"type":"done","source":BRAIN_ID,"client_id":CLIENT_ID}),
                                )
                                .await;
                        }
                        return;
                    }
                }
                kernel
                    .send(
                        &AgentId::from(BRAIN_ID),
                        json!({"type":"send","text":text,"client_id":CLIENT_ID}),
                    )
                    .await;
            });
        }
        Route::Reflect(target) => dispatch_kernel(app, target, json!({"type":"reflect"})),
        Route::Kernel(target, payload) => dispatch_kernel(app, target, payload),
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
    app.chat.push(
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

/// PTY grid size = the terminal-mode body pane (full width, minus header/events
/// rows), inside its border.
fn term_grid<B: ratatui::backend::Backend>(term: &Terminal<B>) -> (u16, u16) {
    let size = term.size().unwrap_or(Size::new(80, 24));
    let rows = size.height.saturating_sub(3 + 8 + 2).max(1);
    let cols = size.width.saturating_sub(2).max(1);
    (rows, cols)
}

/// The classic Diia-style asymmetric banner from the very first commit: a
/// neon-magenta `█` vertical bar with bright-magenta bold FANTASTIC, plus host
/// status + the mode tabs on the middle line.
fn brand_header(app: &App) -> Vec<Line<'static>> {
    let bar = Style::default().fg(Color::Indexed(165)); // neon magenta
    let name = Style::default()
        .fg(Color::LightMagenta)
        .add_modifier(Modifier::BOLD);
    let dim = Style::default().fg(Color::DarkGray);

    let mut mid = vec![
        Span::styled("  █  ", bar),
        Span::styled("FANTASTIC", name),
        Span::styled(format!("    host: {} agents    ", app.agent_count), dim),
    ];
    for (i, (label, mode)) in MODE_TABS.iter().enumerate() {
        let st = if *mode == app.mode {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        l_push(&mut mid, format!(" {label} "), st);
        if i < MODE_TABS.len() - 1 {
            mid.push(Span::styled(" · ", dim));
        }
    }
    vec![
        Line::from(Span::styled("  █", bar)),
        Line::from(mid),
        Line::from(vec![
            Span::styled("  █  ", bar),
            Span::styled("Shift+Tab: change mode", dim),
        ]),
    ]
}

fn l_push(line: &mut Vec<Span<'static>>, text: String, style: Style) {
    line.push(Span::styled(text, style));
}

fn ui(f: &mut Frame, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(8),
        ])
        .split(f.area());

    f.render_widget(Paragraph::new(brand_header(app)), rows[0]);

    match app.mode {
        Mode::Chat => render_chat(f, rows[1], app),
        Mode::Terminal => render_terminal(f, app, rows[1]),
        Mode::Intro => {
            let elapsed = app
                .intro_since
                .map(|t| t.elapsed().as_secs_f32())
                .unwrap_or(0.0);
            app.movie.render(f, rows[1], elapsed);
        }
    }

    let items: Vec<ListItem> = app
        .events
        .iter()
        .rev()
        .take(6)
        .map(|e| ListItem::new(e.clone()))
        .collect();
    let log = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" kernel events "),
    );
    f.render_widget(log, rows[2]);
}

fn render_terminal(f: &mut Frame, app: &App, area: Rect) {
    let blk = Block::default()
        .borders(Borders::ALL)
        .title(" terminal · $SHELL — Ctrl-Q quits app ");
    if let Some(ts) = &app.term {
        if let Ok(p) = ts.parser.lock() {
            let pt = tui_term::widget::PseudoTerminal::new(p.screen()).block(blk);
            f.render_widget(pt, area);
            return;
        }
    }
    f.render_widget(Paragraph::new("terminal unavailable").block(blk), area);
}

/// The unified chat: a scrolling transcript with per-source colored `│` rails,
/// plus the sticky-targeted input line below it.
fn render_chat(f: &mut Frame, area: Rect, app: &App) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let title = if app.chat_busy {
        " chat · thinking… (Ctrl+C interrupts) ".to_string()
    } else {
        " chat ".to_string()
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(parts[0]);
    f.render_widget(block, parts[0]);

    let lines = transcript_lines(app);
    // Show the tail that fits the pane height.
    let h = inner.height as usize;
    let skip = lines.len().saturating_sub(h.max(1));
    let view: Vec<Line> = lines.into_iter().skip(skip).collect();
    f.render_widget(Paragraph::new(view).wrap(Wrap { trim: false }), inner);

    // Input line, prefixed with the sticky target (e.g. `@ai ▸ `).
    let prefix = format!("@{} ▸ ", app.sticky);
    let prompt = Line::from(vec![
        Span::styled(
            prefix.clone(),
            Style::default().fg(chat::color_for(&app.sticky)),
        ),
        Span::raw(app.input.clone()),
    ]);
    let line = Paragraph::new(prompt).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" message — @target to retarget, Enter to send "),
    );
    f.render_widget(line, parts[1]);
    let cx = parts[1].x + 1 + (prefix.chars().count() + app.input.chars().count()) as u16;
    let cy = parts[1].y + 1;
    f.set_cursor_position((cx.min(parts[1].x + parts[1].width.saturating_sub(2)), cy));
}

/// Flatten the transcript into styled lines: each message renders a colored `│`
/// gutter in its source color, the source label, then the wrapped body. `you`
/// is bold/white; tool/note lines are dim; a live stream shows a trailing `▌`.
fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut out: Vec<Line> = Vec::new();
    for m in app.chat.msgs() {
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
                let mut body = t.clone();
                if live {
                    body.push('▌');
                }
                if matches!(m.state, State::Interrupted) {
                    body.push_str(" ⊘");
                }
                let body_style = if m.from == "you" {
                    Style::default().fg(Color::White)
                } else {
                    Style::default()
                };
                out.push(Line::from(vec![
                    gutter,
                    label,
                    Span::styled(body, body_style),
                ]));
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
