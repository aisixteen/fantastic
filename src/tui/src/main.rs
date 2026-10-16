//! `fantastic` — the kernel as a terminal shell.
//!
//! THIN, on purpose: this binary renders agents and sends verbs. Every piece of
//! agentic intelligence — the loop, the prompt, the tools, memory, folds —
//! lives in the kernel substrate, and this process holds none of it.
//!
//! The shape: the left pane is the WHOLE kernel tree, and selecting an agent
//! changes the right pane to that agent's own UI, keyed by its `handler_module`.
//! Threads are PARSED from rows — the model never emits one and never knows they
//! exist — and the gutter shows what the model can actually see, straight off its
//! own view row.

mod app;
mod atomic;
mod boot;
mod env;
mod ev;
mod kern;
mod keys;
mod mcp;
mod panel;
mod presence;
mod readme;
mod recipe;
mod run;
mod seen;
mod send;
mod share;
mod store;
mod sys;
mod tags;
mod theme;
mod theme_gen;
mod thread;
mod tty;
mod ui;
mod vm;

use anyhow::Result;

/// The entry point, and nothing else: the loop lives in `run`, the state in
/// `app`, the headless carrier in `mcp`. A binary whose `main` grows logic is a
/// binary nobody can test.
///
/// A PLAIN `fn main`, not `#[tokio::main]`: some modes must rearrange the
/// process (stdio, env) before any runtime thread exists.
fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // NOT ON A TERMINAL, NO ARGUMENTS: the readme, and nothing runs.
    if args.is_empty() && !sys::stdout_is_tty() {
        print!("{}", readme::text());
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("mcp") {
        mcp::main(&args[1..])
    }
    if args.first().map(String::as_str) == Some("readme") {
        readme::main(&args[1..])
    }
    if args.first().map(String::as_str) == Some("send") {
        send::main(&args[1..])
    }
    match recipe::parse(&args) {
        // THE UI NEEDS A TERMINAL (it draws on `/dev/tty`): without one, say so
        // here — before fds 1 and 2 are pointed at the log file — and stop
        Ok(o) if !o.headless && std::fs::OpenOptions::new().read(true).write(true).open(sys::TTY).is_err() => {
            eprintln!("fantastic: the UI needs a terminal — where there is none, run `fantastic canvas` or `fantastic --headless`");
            std::process::exit(1);
        }
        Ok(o) => shell(o),
        Err(e) => {
            eprintln!("fantastic: {e}");
            std::process::exit(2);
        }
    }
}

/// The interactive shell.
fn shell(opts: recipe::Opts) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            // Tracing goes to stderr, which `tty::open` has pointed at the kernel's
            // log file — the same place the kernel's own prints land. One log, one
            // screen.
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
                )
                .init();
            run::main_loop(opts).await
        })
}
