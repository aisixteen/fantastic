//! `fantastic` — the CLI product: an AI coder + kernel manager for emerging
//! software. It embeds the kernel as a pure library and is the privileged
//! *host*: it composes an in-proc "brain" kernel and drives everything through
//! the one primitive — `kernel.send(target, payload)`.
//!
//! M1 scaffold: compose + boot the in-proc host kernel, and a **Shift+Tab**
//! 3-mode ratatui shell (AI / Terminal / Kernel-manager) with a live kernel
//! event log. Modes are stubs here; M2–M4 fill them in.
//!
//! Headless: `fantastic --smoke` (or non-tty stdout) composes the host, reflects
//! the root, and exits — for CI/build verification without a terminal.

use std::collections::VecDeque;
use std::io::{self, IsTerminal};
use std::sync::Arc;

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
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Tabs, Wrap};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// The privileged host bundle set. Mirrors the kernel binary's default
/// registration but UNCONDITIONAL — the product owns the runtime (runners +
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
    // Runtime/privileged bundles — these live in the PRODUCT now (the kernel
    // lib stays pure). Always on here.
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
    fn title(self) -> &'static str {
        match self {
            Mode::Ai => "AI",
            Mode::Terminal => "Terminal",
            Mode::Kernel => "Kernel manager",
        }
    }
}

struct App {
    #[allow(dead_code)] // M2+ drives the kernel via this handle
    kernel: Arc<Kernel>,
    agent_count: usize,
    mode: Mode,
    events: VecDeque<String>,
    quit: bool,
}

impl App {
    fn push(&mut self, line: String) {
        if self.events.len() >= 500 {
            self.events.pop_front();
        }
        self.events.push_back(line);
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

    // Compose + boot the in-proc host kernel (the brain). RAM-only — no disk,
    // no lock; this is the documented "embedding app's brain kernel" path.
    let booted = bootstrap::bootstrap(register_host_bundles(), BootstrapOptions::in_memory())?;
    let kernel = Arc::clone(&booted.kernel);
    for id in &booted.loaded {
        let _ = kernel.send(id, json!({"type": "boot"})).await;
    }

    let args: Vec<String> = std::env::args().skip(1).collect();
    let headless = args.first().map(String::as_str) == Some("--smoke") || !io::stdout().is_terminal();
    if headless {
        let reply = kernel
            .send(&AgentId::from("kernel"), json!({"type": "reflect", "tree": "ids"}))
            .await;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }

    run_tui(kernel, booted.loaded.len()).await
}

async fn run_tui(kernel: Arc<Kernel>, agent_count: usize) -> Result<()> {
    // Kernel state events → log pane (sync subscriber → unbounded channel).
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<String>();
    let _tok = kernel.add_state_subscriber(Arc::new(move |e: &Value| {
        let _ = evt_tx.send(fmt_event(e));
    }));

    // Terminal input on a blocking thread → channel (one tokio runtime drives UI).
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

    let mut app = App {
        kernel,
        agent_count,
        mode: Mode::Ai,
        events: VecDeque::new(),
        quit: false,
    };
    app.push("host kernel composed — Shift+Tab switches modes, q / Ctrl-C quits".into());
    term.draw(|f| ui(f, &app))?;

    loop {
        tokio::select! {
            Some(ev) = in_rx.recv() => handle_input(&mut app, ev),
            Some(line) = evt_rx.recv() => app.push(line),
            else => break,
        }
        if app.quit {
            break;
        }
        term.draw(|f| ui(f, &app))?;
    }

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    term.show_cursor()?;
    Ok(())
}

fn handle_input(app: &mut App, ev: Event) {
    if let Event::Key(KeyEvent {
        code,
        kind,
        modifiers,
        ..
    }) = ev
    {
        if kind != KeyEventKind::Press {
            return; // ignore key-release/repeat (Windows fires them)
        }
        match code {
            KeyCode::BackTab => app.mode = app.mode.next(), // Shift+Tab
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => app.quit = true,
            _ => {}
        }
    }
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

    let tabs = Tabs::new(vec![
        Line::from("AI"),
        Line::from("Terminal"),
        Line::from("Kernel manager"),
    ])
    .select(app.mode.idx())
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" fantastic · host: {} agents ", app.agent_count)),
    )
    .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD));
    f.render_widget(tabs, rows[0]);

    let body = Paragraph::new(format!(
        "{} mode\n\n(stub — filled in M2–M4)",
        app.mode.title()
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", app.mode.title())),
    )
    .wrap(Wrap { trim: true });
    f.render_widget(body, rows[1]);

    let items: Vec<ListItem> = app
        .events
        .iter()
        .rev()
        .take(6)
        .map(|e| ListItem::new(e.clone()))
        .collect();
    let log = List::new(items).block(Block::default().borders(Borders::ALL).title(" kernel events "));
    f.render_widget(log, rows[2]);
}
