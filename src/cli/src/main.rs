//! `fantastic` — the CLI product: an AI coder + kernel manager for emerging
//! software. It embeds the kernel as a pure library and is the privileged
//! *host*: it composes an in-proc "brain" kernel and drives everything through
//! the one primitive — `kernel.send(target, payload)`.
//!
//! The TUI is ONE chat with a room per character (see `fantastic-tui`):
//! `@ai` streams the brain, `@sh` breathes a real PTY, `@ws` drives the
//! out-of-process workspace kernel, `@<agent> <verb> [k=v…]` opens any agent's
//! room; Shift-Tab turns between rooms; `/intro` plays the scripted movie.
//!
//! Headless: `fantastic --smoke` (or non-tty stdout) composes the host, reflects
//! the root, and exits — for CI/build verification without a terminal. One-shot
//! `fantastic ai "<prompt>"` runs a single AI turn; `fantastic demo` plays the
//! A→Z flow. `fantastic --help` lists the full surface.

use std::io::{self, IsTerminal, Read, Write};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use fantastic_host::gateway::{Runtime, Workspace};
use fantastic_kernel::{AgentId, Kernel};
use fantastic_term::TerminalSession;
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Hydrate persisted settings (`<app_home>/settings.json`) into the AI env —
    // before anything reads the backend/model — so the tool works across runs
    // without re-exporting env. An explicit env still overrides the file.
    fantastic_host::hydrate_ai_env();

    // ── Config + gateway subcommands. These don't compose the in-proc host
    // kernel, so handle them BEFORE the heavy `compose_manager`.
    match args.first().map(String::as_str) {
        Some("config") => return cmd_config(&args[1..]),
        Some("up") => return cmd_up(&args[1..]).await,
        Some("k") => return cmd_k(&args[1..]).await,
        Some("down") => return cmd_down().await,
        Some("--help") | Some("-h") | Some("help") => {
            print!("{}", usage());
            return Ok(());
        }
        // Unknown subcommand: refuse loudly (exit 2) instead of silently
        // launching the TUI with the arg ignored. Bare `fantastic` (no args)
        // still opens the TUI / headless reflect below.
        Some(other) if !KNOWN_SUBCOMMANDS.contains(&other) => {
            eprintln!("fantastic: unknown subcommand `{other}`\n");
            eprint!("{}", usage());
            std::process::exit(2);
        }
        _ => {}
    }

    let (kernel, loaded) = fantastic_host::compose_manager().await?;

    match args.first().map(String::as_str) {
        // A→Z headless demo: assemble → serve → terminal, printing each step.
        Some("demo") => return demo_flow(&kernel).await,
        // Compose host + reflect root + exit (CI/build check).
        Some("--smoke") => {
            eprint!("{}", ansi_banner(loaded.len()));
            let reply = kernel
                .send(
                    &AgentId::from("kernel"),
                    json!({"type":"reflect","tree":"ids"}),
                )
                .await;
            println!("{}", serde_json::to_string_pretty(&reply)?);
            return Ok(());
        }
        // Headless one-shot AI turn: `fantastic ai "<prompt>"`. Drives the same
        // brain as AI mode (the universal `send` tool lets it reach the kernel),
        // and prints the final response. Backend + model are REQUIRED and explicit
        // (FANTASTIC_AI_BACKEND=ollama|nvidia|anthropic, FANTASTIC_AI_MODEL=<id>);
        // nothing is guessed — unset → a clear `✗ set FANTASTIC_AI_…` message.
        Some("ai") | Some("ask") => {
            let prompt = args[1..].join(" ");
            if prompt.trim().is_empty() {
                eprintln!("usage: fantastic ai \"<prompt>\"");
                std::process::exit(2);
            }
            eprint!("{}", ansi_banner(loaded.len()));
            let (tx, mut rx) = mpsc::unbounded_channel::<String>();
            let k = Arc::clone(&kernel);
            let turn = tokio::spawn(async move { fantastic_brain::run_turn(k, prompt, tx).await });
            while let Some(line) = rx.recv().await {
                println!("{line}");
            }
            let _ = turn.await;
            return Ok(());
        }
        _ => {}
    }
    // No tty → headless smoke; tty → the TUI.
    if !io::stdout().is_terminal() {
        let reply = kernel
            .send(
                &AgentId::from("kernel"),
                json!({"type":"reflect","tree":"ids"}),
            )
            .await;
        println!("{}", serde_json::to_string_pretty(&reply)?);
        return Ok(());
    }
    fantastic_tui::run(kernel, loaded.len()).await
}

/// Every recognized top-level subcommand / flag. Anything else is refused with
/// the usage text (exit 2) instead of silently launching the TUI.
const KNOWN_SUBCOMMANDS: [&str; 8] = ["config", "up", "k", "down", "demo", "--smoke", "ai", "ask"];

/// The `--help` / unknown-subcommand usage text — the headless surface in one
/// screen (the interactive surface is documented in-app via `/help`).
fn usage() -> String {
    "\
fantastic — AI coder + kernel manager for emerging software

usage: fantastic [subcommand]

  (no args)             open the TUI (a chat with a room per agent); non-tty
                        stdout prints the reflected agent tree instead
  ai|ask \"<prompt>\"     one-shot AI turn (requires an explicit connector:
                        `config set ai.backend …` + `config set ai.model …`)
  config show|set|clear persisted settings (keys: ai.backend, ai.model,
                        ai.num_ctx; ai.key goes to the OS keychain)
  up [--container] [--image X] [--runtime rust|python|swift]
                        attach to (or spawn) the workspace kernel in cwd
  k <id> <verb> [k=v …] send a verb to a workspace agent (over the gateway)
  down                  shut the workspace kernel down
  demo                  play the headless A→Z composition demo
  --smoke               compose the host, print the reflect tree, exit
  --help | -h | help    this text
"
    .to_string()
}

/// Parse `--runtime <name>` out of the subcommand args (default Rust). Only the
/// rust substrate is wired end-to-end today; the flag accepts the others so the
/// surface is stable.
fn runtime_from_args(args: &[String]) -> Result<Runtime> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--runtime" {
            let name = args.get(i + 1).map(String::as_str).unwrap_or("");
            return match name {
                "rust" | "" => Ok(Runtime::Rust),
                "python" => Ok(Runtime::Python),
                "swift" => Ok(Runtime::Swift),
                other => Err(anyhow::anyhow!("unknown --runtime {other}")),
            };
        }
        i += 1;
    }
    Ok(Runtime::Rust)
}

/// `fantastic up [--container] [--image X] [--runtime rust]` — attach to (or
/// spawn) the workspace kernel in cwd, then print attached/spawned + the base
/// URL + the agent tree. `--container` runs it as a podman/docker container
/// (same loopback-HTTP attach path); without it, the kernel is a native process.
async fn cmd_up(args: &[String]) -> Result<()> {
    let runtime = runtime_from_args(args)?;
    let ws = Workspace::new(std::env::current_dir()?);

    if args.iter().any(|a| a == "--container") {
        // Honor `--image X` by setting the env the gateway reads (single source).
        if let Some(img) = flag_value(args, "--image") {
            std::env::set_var("FANTASTIC_IMAGE", img);
        }
        let (handle, name) = ws.spawn_container(runtime).await?;
        println!("container {name} at {}", handle.base_url);
        let tree = handle.reflect(None).await?;
        println!("{}", serde_json::to_string_pretty(&tree)?);
        return Ok(());
    }

    let was_running = ws.attach().await?.is_some();
    let handle = ws.attach_or_spawn(runtime).await?;
    println!(
        "{} fantastic kernel at {}",
        if was_running { "attached" } else { "spawned" },
        handle.base_url
    );
    let tree = handle.reflect(None).await?;
    println!("{}", serde_json::to_string_pretty(&tree)?);
    Ok(())
}

/// Read `--flag <value>` out of the subcommand args (None if absent).
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// `fantastic k <id> <verb> [k=v ...]` — attach-or-spawn, send the verb over
/// HTTP, pretty-print the reply.
async fn cmd_k(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        eprintln!("usage: fantastic k <id> <verb> [k=v ...]");
        std::process::exit(2);
    }
    let id = &args[0];
    let verb = &args[1];
    let mut payload = Map::new();
    payload.insert("type".into(), json!(verb));
    for kv in &args[2..] {
        if let Some((k, v)) = kv.split_once('=') {
            payload.insert(k.to_string(), fantastic_host::parse_kv(v));
        }
    }
    let ws = Workspace::new(std::env::current_dir()?);
    let handle = ws.attach_or_spawn(Runtime::Rust).await?;
    let reply = handle.send(id, Value::Object(payload)).await?;
    println!("{}", serde_json::to_string_pretty(&reply)?);
    Ok(())
}

/// `fantastic down` — container-aware. If `<cwd>/.fantastic/launch.json` records
/// a `container`, stop+remove it via its engine and drop launch.json. Otherwise
/// fall back to the native graceful `shutdown_kernel`.
async fn cmd_down() -> Result<()> {
    let cwd = std::env::current_dir()?;
    let launch_path = cwd.join(".fantastic").join("launch.json");
    if let Ok(s) = std::fs::read_to_string(&launch_path) {
        if let Ok(v) = serde_json::from_str::<Value>(&s) {
            if let Some(name) = v.get("container").and_then(Value::as_str) {
                let engine = v.get("engine").and_then(Value::as_str).unwrap_or("podman");
                fantastic_host::gateway::stop_container(engine, name);
                let _ = std::fs::remove_file(&launch_path);
                println!("stopped container {name}");
                return Ok(());
            }
        }
    }

    let ws = Workspace::new(cwd);
    match ws.attach().await? {
        Some(handle) => {
            let reply = handle
                .send("core", json!({"type":"shutdown_kernel"}))
                .await?;
            println!("shutdown requested → {}", short(&reply));
            Ok(())
        }
        None => {
            println!("no running kernel in this workspace");
            Ok(())
        }
    }
}

/// `fantastic config show | set <key> <value>` — the persisted, hydrated settings
/// at `<app_home>/settings.json`. Dotted keys nest (e.g. `ai.model`). This is how
/// you set/reset the AI backend + model once, instead of exporting env each run.
fn cmd_config(args: &[String]) -> Result<()> {
    let mut s = fantastic_host::load_settings();
    match args.first().map(String::as_str) {
        None | Some("show") => {
            println!("# {}", fantastic_host::settings_path().display());
            println!("{}", serde_json::to_string_pretty(&s)?);
        }
        Some("set") => {
            let key = args
                .get(1)
                .ok_or_else(|| anyhow::anyhow!("usage: config set <key> <value>"))?;
            let value = args.get(2).cloned().unwrap_or_default();
            // The API key NEVER goes into settings.json — route it to the OS
            // keychain for the configured backend ("raw key is retarded").
            if key == "ai.key" {
                let backend = fantastic_host::ai_config().backend.ok_or_else(|| {
                    anyhow::anyhow!("set ai.backend first (config set ai.backend …)")
                })?;
                fantastic_host::secret::set_key(&backend, &value)
                    .map_err(|e| anyhow::anyhow!(e))?;
                println!("stored {backend} key in the OS keychain (not on disk)");
                return Ok(());
            }
            // Coerce ints (e.g. ai.num_ctx); everything else stays a string.
            let val = value
                .parse::<u64>()
                .map(|n| json!(n))
                .unwrap_or_else(|_| json!(value));
            fantastic_host::settings_set(&mut s, key, val);
            fantastic_host::save_settings(&s)?;
            println!(
                "set {key} = {value}  →  {}",
                fantastic_host::settings_path().display()
            );
        }
        Some("clear") => {
            fantastic_host::clear_ai_connector().map_err(|e| anyhow::anyhow!(e))?;
            println!("cleared the AI connector (settings + keychain key)");
        }
        Some(other) => {
            anyhow::bail!(
                "unknown config subcommand `{other}` (use: show | set <key> <value> | clear)"
            )
        }
    }
    Ok(())
}

/// Headless A→Z demo of what the product drives: compose host → assemble a
/// web-serving kernel via kernel-manager sugar → prove it serves over HTTP →
/// run a terminal PTY command. Each step prints, so the loop is legible.
async fn demo_flow(kernel: &Arc<Kernel>) -> Result<()> {
    eprint!("{}", ansi_banner(1));

    println!("── A · reflect the bare host ──");
    let t = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids"}),
        )
        .await;
    println!(
        "   agents: {}\n",
        t.get("tree").cloned().unwrap_or(Value::Null)
    );

    println!("── B · assemble a web-serving kernel (the kernel-manager sugar) ──");
    let port = free_port();
    let r = kernel
        .send(&AgentId::from("kernel"), json!({"type":"create_agent","handler_module":"file_bridge.tools","id":"files","root":".","ingress_rule":"allow_all"}))
        .await;
    println!("   create file_bridge files → {}", short(&r));
    let r = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"create_agent","handler_module":"web.tools","id":"web","port":port}),
        )
        .await;
    println!("   create web (port {port}) → {}", short(&r));
    let r = kernel
        .send(&AgentId::from("web"), json!({"type":"boot"}))
        .await;
    println!("   boot web → {}\n", short(&r));

    println!("── C · reflect the assembled host ──");
    let t = kernel
        .send(
            &AgentId::from("kernel"),
            json!({"type":"reflect","tree":"ids"}),
        )
        .await;
    println!(
        "   agents: {}\n",
        t.get("tree").cloned().unwrap_or(Value::Null)
    );

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
    let _ = kernel
        .send(&AgentId::from("web"), json!({"type":"shutdown"}))
        .await;
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
    write!(
        s,
        "GET {path} HTTP/1.0\r\nHost: {host}\r\nConnection: close\r\n\r\n"
    )?;
    let mut buf = String::new();
    s.read_to_string(&mut buf)?;
    let status = buf.lines().next().unwrap_or("").to_string();
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("");
    let head: String = body.chars().take(70).collect();
    Ok((status, body.len(), head.replace('\n', " ")))
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
