//! `fantastic` — the CLI product: an AI coder + kernel manager for emerging
//! software. It embeds the kernel as a pure library and is the privileged
//! *host*: it composes an in-proc "brain" kernel and drives everything through
//! the one primitive — `kernel.send(target, payload)`.
//!
//! Modes (Shift+Tab cycles): AI / Terminal / Kernel-manager.
//! - **Kernel manager** (M2): a command line whose sugar verbs drive the host
//!   (`tree`, `reflect [id]`, `create <handler> [k=v…]`, `update <id> k=v…`,
//!   `delete <id>`, `send <id> <verb> [k=v…]`).
//! - AI / Terminal are stubs here (M3–M4).
//!
//! Headless: `fantastic --smoke` (or non-tty stdout) composes the host, reflects
//! the root, and exits — for CI/build verification without a terminal.

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fantastic_kernel::bootstrap::{self, BootstrapOptions};
use fantastic_kernel::{AgentId, BundleRegistry, Kernel};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::*;
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

mod ai;
mod term;
use term::TerminalSession;

/// The privileged host bundle set. The product owns the runtime (runners +
/// terminal + AI backends), so it always registers the full set into its host.
fn register_host_bundles() -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    reg.register("file_bridge.tools", fantastic_file::FileBundle);
    reg.register("yaml_state.tools", fantastic_yaml_state::YamlStateBundle);
    reg.register("web.tools", fantastic_web::WebBundle);
    reg.register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    reg.register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    reg.register("scheduler.tools", fantastic_scheduler::SchedulerBundle);
    reg.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
    );
    reg.register(
        "nvidia_nim_backend.tools",
        fantastic_nvidia_nim_backend::NvidiaNimBundle,
    );
    reg.register("ws_bridge.tools", fantastic_bridge::WsBridgeBundle);
    reg.register(
        "relay_connector.tools",
        fantastic_bridge::RelayConnectorBundle,
    );
    reg.register(
        fantastic_proxy_agent::HANDLER_MODULE,
        fantastic_proxy_agent::ProxyAgentBundle::new(),
    );
    reg.register(
        fantastic_tools::HANDLER_MODULE,
        fantastic_tools::ToolsBundle::new(),
    );
    reg.register(
        "terminal_backend.tools",
        fantastic_terminal_backend::TerminalBackendBundle,
    );
    reg.register(
        "python_runtime.tools",
        fantastic_python_runtime::PythonRuntimeBundle,
    );
    reg.register(
        "local_runner.tools",
        fantastic_local_runner::LocalRunnerBundle,
    );
    reg.register("ssh_runner.tools", fantastic_ssh_runner::SshRunnerBundle);
    reg
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ai,
    Terminal,
    Kernel,
}

impl Mode {
    fn next(self) -> Self {
        match self {
            Mode::Ai => Mode::Terminal,
            Mode::Terminal => Mode::Kernel,
            Mode::Kernel => Mode::Ai,
        }
    }
    fn idx(self) -> usize {
        match self {
            Mode::Ai => 0,
            Mode::Terminal => 1,
            Mode::Kernel => 2,
        }
    }
}

struct App {
    kernel: Arc<Kernel>,
    agent_count: usize,
    mode: Mode,
    events: VecDeque<String>,
    /// Kernel-manager command line + scrollback.
    input: String,
    km_output: VecDeque<String>,
    cmd_tx: mpsc::UnboundedSender<String>,
    /// AI mode: conversation log + input + completion channel.
    ai_input: String,
    ai_output: VecDeque<String>,
    ai_tx: mpsc::UnboundedSender<String>,
    ai_busy: bool,
    /// Terminal-proxy mode PTY (spawned at startup).
    term: Option<TerminalSession>,
    quit: bool,
}

impl App {
    fn push_event(&mut self, line: String) {
        cap_push(&mut self.events, line, 500);
    }
    fn km_line(&mut self, line: String) {
        cap_push(&mut self.km_output, line, 1000);
    }
    fn ai_line(&mut self, line: String) {
        cap_push(&mut self.ai_output, line, 1000);
    }
}

fn cap_push(buf: &mut VecDeque<String>, line: String, cap: usize) {
    if buf.len() >= cap {
        buf.pop_front();
    }
    buf.push_back(line);
}

/// k=v value coercion (mirrors the kernel CLI): bool → int → float → JSON
/// object/array literal → string.
fn parse_kv(v: &str) -> Value {
    match v.to_ascii_lowercase().as_str() {
        "true" => return json!(true),
        "false" => return json!(false),
        _ => {}
    }
    if let Ok(n) = v.parse::<i64>() {
        return json!(n);
    }
    if let Ok(f) = v.parse::<f64>() {
        return json!(f);
    }
    let looks_json =
        (v.starts_with('{') && v.ends_with('}')) || (v.starts_with('[') && v.ends_with(']'));
    if looks_json {
        if let Ok(parsed) = serde_json::from_str::<Value>(v) {
            return parsed;
        }
    }
    Value::String(v.to_string())
}

fn add_kvs(payload: &mut Map<String, Value>, kvs: &[&str]) {
    for kv in kvs {
        if let Some((k, v)) = kv.split_once('=') {
            payload.insert(k.to_string(), parse_kv(v));
        }
    }
}

/// Parse a kernel-manager sugar command into `(target, payload)` for `send`.
fn parse_command(line: &str) -> Result<(AgentId, Value), String> {
    let toks: Vec<&str> = line.split_whitespace().collect();
    let mut p = Map::new();
    match toks.as_slice() {
        [] => Err("empty".into()),
        ["tree"] | ["reflect"] => {
            Ok((AgentId::from("kernel"), json!({"type":"reflect","tree":"ids"})))
        }
        ["reflect", id] => Ok((AgentId::from(*id), json!({"type":"reflect"}))),
        ["create", handler, kvs @ ..] => {
            p.insert("type".into(), json!("create_agent"));
            p.insert("handler_module".into(), json!(*handler));
            add_kvs(&mut p, kvs);
            Ok((AgentId::from("kernel"), Value::Object(p)))
        }
        ["update", id, kvs @ ..] => {
            p.insert("type".into(), json!("update_agent"));
            p.insert("id".into(), json!(*id));
            add_kvs(&mut p, kvs);
            Ok((AgentId::from("kernel"), Value::Object(p)))
        }
        ["delete", id] => Ok((AgentId::from("kernel"), json!({"type":"delete_agent","id":id}))),
        ["send", id, verb, kvs @ ..] => {
            p.insert("type".into(), json!(*verb));
            add_kvs(&mut p, kvs);
            Ok((AgentId::from(*id), Value::Object(p)))
        }
        _ => Err(format!(
            "unknown: {} (try: tree | reflect [id] | create <handler> [k=v] | update <id> k=v | delete <id> | send <id> <verb> [k=v])",
            toks[0]
        )),
    }
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let booted = bootstrap::bootstrap(register_host_bundles(), BootstrapOptions::in_memory())?;
    let kernel = Arc::clone(&booted.kernel);
    for id in &booted.loaded {
        let _ = kernel.send(id, json!({"type": "boot"})).await;
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        // A→Z headless demo: assemble → serve → terminal, printing each step.
        Some("demo") => return demo_flow(&kernel).await,
        // Compose host + reflect root + exit (CI/build check).
        Some("--smoke") => {
            eprint!("{}", ansi_banner(booted.loaded.len()));
            let reply = kernel
                .send(&AgentId::from("kernel"), json!({"type":"reflect","tree":"ids"}))
                .await;
            println!("{}", serde_json::to_string_pretty(&reply)?);
            return Ok(());
        }
        _ => {}
    }
    // No tty → headless smoke; tty → the TUI.
    if !io::stdout().is_terminal() {
        let reply = kernel
            .send(&AgentId::from("kernel"), json!({"type":"reflect","tree":"ids"}))
            .await;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }
    run_tui(kernel, booted.loaded.len()).await
}

/// Headless A→Z demo of what the product drives: compose host → assemble a
/// web-serving kernel via kernel-manager sugar → prove it serves over HTTP →
/// run a terminal PTY command. Each step prints, so the loop is legible.
async fn demo_flow(kernel: &Arc<Kernel>) -> Result<()> {
    eprint!("{}", ansi_banner(1));

    println!("── A · reflect the bare host ──");
    let t = kernel
        .send(&AgentId::from("kernel"), json!({"type":"reflect","tree":"ids"}))
        .await;
    println!("   agents: {}\n", t.get("tree").cloned().unwrap_or(Value::Null));

    println!("── B · assemble a web-serving kernel (the kernel-manager sugar) ──");
    let port = free_port();
    let r = kernel
        .send(&AgentId::from("kernel"), json!({"type":"create_agent","handler_module":"file_bridge.tools","id":"files","root":".","ingress_rule":"allow_all"}))
        .await;
    println!("   create file_bridge files → {}", short(&r));
    let r = kernel
        .send(&AgentId::from("kernel"), json!({"type":"create_agent","handler_module":"web.tools","id":"web","port":port}))
        .await;
    println!("   create web (port {port}) → {}", short(&r));
    let r = kernel.send(&AgentId::from("web"), json!({"type":"boot"})).await;
    println!("   boot web → {}\n", short(&r));

    println!("── C · reflect the assembled host ──");
    let t = kernel
        .send(&AgentId::from("kernel"), json!({"type":"reflect","tree":"ids"}))
        .await;
    println!("   agents: {}\n", t.get("tree").cloned().unwrap_or(Value::Null));

    println!("── D · HTTP GET / on the live host (it actually serves) ──");
    tokio::time::sleep(Duration::from_millis(400)).await;
    match http_get("127.0.0.1", port, "/") {
        Ok((status, len, head)) => {
            println!("   GET http://127.0.0.1:{port}/ → {status}  ({len} bytes)");
            println!("   ‹{head}›\n");
        }
        Err(e) => println!("   GET failed: {e}\n"),
    }

    println!("── E · terminal PTY: run `uname -a` ──");
    let (tx, _rx) = mpsc::unbounded_channel::<()>();
    if let Ok(mut ts) = TerminalSession::spawn(12, 100, tx) {
        ts.write(b"uname -a\r");
        tokio::time::sleep(Duration::from_millis(1300)).await;
        if let Ok(p) = ts.parser.lock() {
            for line in p
                .screen()
                .contents()
                .lines()
                .filter(|l| !l.trim().is_empty())
                .take(5)
            {
                println!("   {line}");
            }
        }
    } else {
        println!("   (pty unavailable)");
    }

    println!("\n── F · shutdown web ──");
    let _ = kernel.send(&AgentId::from("web"), json!({"type":"shutdown"})).await;
    println!("\n   that's the loop: compose host → assemble agents via `send` → serve + drive,");
    println!("   all from one product. AI mode adds a brain that drives this same `send`.");
    Ok(())
}

fn short(v: &Value) -> String {
    if let Some(e) = v.get("error").and_then(Value::as_str) {
        return format!("error: {e}");
    }
    if let Some(id) = v.get("id").and_then(Value::as_str) {
        return format!("id={id}");
    }
    if v.get("booted").is_some() || v.get("already").is_some() {
        return "ok".into();
    }
    v.to_string().chars().take(80).collect()
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|l| l.local_addr().ok())
        .map(|a| a.port())
        .unwrap_or(8099)
}

fn http_get(host: &str, port: u16, path: &str) -> Result<(String, usize, String)> {
    let mut s = std::net::TcpStream::connect((host, port))?;
    s.set_read_timeout(Some(Duration::from_secs(3))).ok();
    write!(s, "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n")?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    let status = buf.lines().next().unwrap_or("").to_string();
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("");
    let head: String = body.chars().take(70).collect();
    Ok((status, body.len(), head.replace('\n', " ")))
}

async fn run_tui(kernel: Arc<Kernel>, agent_count: usize) -> Result<()> {
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<String>();
    let _tok = kernel.add_state_subscriber(Arc::new(move |e: &Value| {
        let _ = evt_tx.send(fmt_event(e));
    }));

    // Command results (async kernel.send completions) flow back here.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();
    // PTY output → repaint pings.
    let (redraw_tx, mut redraw_rx) = mpsc::unbounded_channel::<()>();
    // AI turn completions.
    let (ai_tx, mut ai_rx) = mpsc::unbounded_channel::<String>();

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
    execute!(out, EnterAlternateScreen)?;
    let mut term = Terminal::new(CrosstermBackend::new(out))?;

    // Spawn the terminal-proxy PTY at the body size.
    let (trows, tcols) = term_grid(&term);
    let session = TerminalSession::spawn(trows, tcols, redraw_tx.clone()).ok();

    let mut app = App {
        kernel,
        agent_count,
        mode: Mode::Ai,
        events: VecDeque::new(),
        input: String::new(),
        km_output: VecDeque::new(),
        cmd_tx,
        ai_input: String::new(),
        ai_output: VecDeque::new(),
        ai_tx,
        ai_busy: false,
        term: session,
        quit: false,
    };
    app.push_event("host kernel composed — Shift+Tab switches modes, Ctrl-Q quits".into());
    app.km_line("kernel manager — type a command, Enter to run. `tree` to list agents.".into());
    app.ai_line("AI — type a message, Enter to send. Backend: FANTASTIC_AI_BACKEND (default ollama).".into());
    term.draw(|f| ui(f, &app))?;

    loop {
        tokio::select! {
            Some(ev) = in_rx.recv() => handle_input(&mut app, ev),
            Some(line) = evt_rx.recv() => app.push_event(line),
            Some(out) = cmd_rx.recv() => {
                for l in out.split('\n') { app.km_line(l.to_string()); }
            }
            Some(()) = redraw_rx.recv() => {}
            Some(resp) = ai_rx.recv() => {
                app.ai_busy = false;
                for l in resp.split('\n') { app.ai_line(l.to_string()); }
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
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

fn handle_input(app: &mut App, ev: Event) {
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
    // Global keys. Ctrl-Q quits (NOT Ctrl-C — that must reach the shell in
    // terminal mode). Shift+Tab cycles modes.
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('q') {
        app.quit = true;
        return;
    }
    if code == KeyCode::BackTab {
        app.mode = app.mode.next();
        return;
    }
    match app.mode {
        Mode::Terminal => {
            if let Some(ts) = app.term.as_mut() {
                if let Some(bytes) = encode_key(code, modifiers) {
                    ts.write(&bytes);
                }
            }
        }
        Mode::Kernel => match code {
            KeyCode::Char(c) => app.input.push(c),
            KeyCode::Backspace => {
                app.input.pop();
            }
            KeyCode::Enter => submit_command(app),
            _ => {}
        },
        Mode::Ai => match code {
            KeyCode::Char(c) => app.ai_input.push(c),
            KeyCode::Backspace => {
                app.ai_input.pop();
            }
            KeyCode::Enter => submit_ai(app),
            _ => {}
        },
    }
}

fn submit_ai(app: &mut App) {
    let text = std::mem::take(&mut app.ai_input).trim().to_string();
    if text.is_empty() || app.ai_busy {
        return;
    }
    app.ai_line(format!("› {text}"));
    app.ai_busy = true;
    let kernel = Arc::clone(&app.kernel);
    let tx = app.ai_tx.clone();
    tokio::spawn(async move {
        ai::run_turn(kernel, text, tx).await;
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

fn submit_command(app: &mut App) {
    let line = std::mem::take(&mut app.input);
    let line = line.trim().to_string();
    if line.is_empty() {
        return;
    }
    app.km_line(format!("› {line}"));
    match parse_command(&line) {
        Ok((target, payload)) => {
            let kernel = Arc::clone(&app.kernel);
            let tx = app.cmd_tx.clone();
            tokio::spawn(async move {
                let reply = kernel.send(&target, payload).await;
                let rendered =
                    serde_json::to_string_pretty(&reply).unwrap_or_else(|_| reply.to_string());
                let _ = tx.send(rendered);
            });
        }
        Err(e) => app.km_line(format!("  ✗ {e}")),
    }
}

/// The classic banner as raw ANSI (for the headless/greeting path) — the exact
/// neon-magenta `█` + bright-magenta bold FANTASTIC from the first commit.
fn ansi_banner(agents: usize) -> String {
    let nm = "\x1b[38;5;165m"; // neon magenta
    let bm = "\x1b[95m"; // bright magenta
    let b = "\x1b[1m";
    let d = "\x1b[2m";
    let r = "\x1b[0m";
    format!(
        "\n  {nm}█{r}\n  {nm}█{r}  {bm}{b}FANTASTIC{r}\n  {nm}█{r}     {d}host · {agents} agents{r}\n\n"
    )
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
    for (i, t) in ["AI", "Terminal", "Kernel manager"].iter().enumerate() {
        let st = if i == app.mode.idx() {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            dim
        };
        l_push(&mut mid, format!(" {t} "), st);
        if i < 2 {
            mid.push(Span::styled(" · ", dim));
        }
    }
    vec![
        Line::from(Span::styled("  █", bar)),
        Line::from(mid),
        Line::from(Span::styled("  █", bar)),
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
        Mode::Kernel => {
            render_console(f, rows[1], &app.km_output, &app.input, " output ", " command ")
        }
        Mode::Terminal => render_terminal(f, app, rows[1]),
        Mode::Ai => render_console(
            f,
            rows[1],
            &app.ai_output,
            &app.ai_input,
            " conversation ",
            if app.ai_busy {
                " thinking… "
            } else {
                " message — Enter to send "
            },
        ),
    }

    let items: Vec<ListItem> = app
        .events
        .iter()
        .rev()
        .take(6)
        .map(|e| ListItem::new(e.clone()))
        .collect();
    let log =
        List::new(items).block(Block::default().borders(Borders::ALL).title(" kernel events "));
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

/// A scrollback + input-line console, shared by the Kernel-manager and AI modes.
fn render_console(
    f: &mut Frame,
    area: Rect,
    lines: &VecDeque<String>,
    input: &str,
    out_title: &str,
    in_title: &str,
) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let h = parts[0].height.saturating_sub(2) as usize;
    let items: Vec<ListItem> = lines
        .iter()
        .rev()
        .take(h.max(1))
        .rev()
        .map(|l| ListItem::new(l.clone()))
        .collect();
    let out = List::new(items).block(Block::default().borders(Borders::ALL).title(out_title.to_string()));
    f.render_widget(out, parts[0]);

    let prompt = format!("› {input}");
    let line = Paragraph::new(prompt)
        .block(Block::default().borders(Borders::ALL).title(in_title.to_string()));
    f.render_widget(line, parts[1]);
    let cx = parts[1].x + 1 + (2 + input.chars().count()) as u16;
    let cy = parts[1].y + 1;
    f.set_cursor_position((cx.min(parts[1].x + parts[1].width.saturating_sub(2)), cy));
}
