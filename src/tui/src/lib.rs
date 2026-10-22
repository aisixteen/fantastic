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
//!
//! A LIBRARY (#248): the `fantastic` binary (`src/main.rs`) is
//! `main_with(args, &[])`. Another binary (aisixteen/fantastic_shift: a headless
//! one, an app) runs the same shell with its own bundles: each [`Extra`] it
//! passes registers them on the host's registry AFTER the stock set, in every
//! mode that boots a kernel (the UI, `--headless`, `canvas`, `mcp`).

mod app;
mod atomic;
mod boot;
mod env;
mod ev;
mod kern;
mod keys;
mod mcp;
mod migrate;
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

use std::io::IsTerminal;
use std::sync::OnceLock;

use anyhow::Result;

/// The kernel this shell is built on, re-exported so an embedder writes its
/// bundles against the SAME crate (`fantastic_tui::fantastic_kernel::bundle::Bundle`).
pub use fantastic_kernel;
pub use fantastic_kernel::BundleRegistry;

/// AN EMBEDDER'S BUNDLES: registers them on the host's registry, after the stock
/// set (`kern/host.rs`), so a record naming one hydrates into a live agent.
/// Registering a stock `handler_module` again replaces the stock bundle.
pub type Extra = fn(&mut BundleRegistry);

/// This process's extras: set once, by `main_with`, before any kernel boots.
/// Process-wide because the host is booted from two independent paths (the
/// `recipe` modes through `run`/`vm`, and `mcp`).
static EXTRAS: OnceLock<Vec<Extra>> = OnceLock::new();

/// Set this process's extras. The first call wins; a later one is ignored.
pub(crate) fn set_extras(extras: &[Extra]) {
    let _ = EXTRAS.set(extras.to_vec());
}

/// This process's extras — none unless `main_with` was given some.
pub(crate) fn extras() -> &'static [Extra] {
    EXTRAS.get().map(Vec::as_slice).unwrap_or(&[])
}

/// The whole `fantastic` command line. `args` WITHOUT the program name
/// (`std::env::args().skip(1)`); `extras`: the embedder's bundles (`&[]` for
/// `fantastic` itself). The mode dispatch and nothing else: the loop lives in
/// `run`, the state in `app`, the headless carrier in `mcp`.
///
/// A PLAIN fn, not async: some modes must rearrange the process (stdio, env)
/// before any runtime thread exists — call it from a plain `fn main`.
pub fn main_with(args: Vec<String>, extras: &[Extra]) -> Result<()> {
    set_extras(extras);
    // NOT ON A TERMINAL, NO ARGUMENTS: the readme, and nothing runs.
    if args.is_empty() && !std::io::stdout().is_terminal() {
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
    if args.first().map(String::as_str) == Some("migrate-home") {
        migrate::main(&args[1..])
    }
    // THE STOP HELPER (Windows): a hidden mode the home kernel runs to stop a
    // canvas in another console. Not in `fantastic readme`.
    if args.first().map(String::as_str) == Some("__ctrl_break") {
        sys::ctrl_break_main(&args[1..])
    }
    match recipe::parse(&args) {
        // THE UI NEEDS A TERMINAL (it draws on `/dev/tty`, `sys::TTY`): without
        // one, say so here — before fds 1 and 2 are pointed at the log file — and stop
        Ok(o) if !o.headless && sys::open_tty().is_err() => {
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
