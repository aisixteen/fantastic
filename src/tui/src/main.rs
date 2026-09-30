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
mod manifest;
mod mcp;
mod panel;
mod presence;
mod recipe;
mod run;
mod seen;
mod store;
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
    // CRITICAL TODO(dev-llm): the key comes from the keychain, BEFORE any
    // runtime thread exists — setting the environment later is a race.
    #[cfg(feature = "dev-llm")]
    if let Err(e) = kern::dev_llm::load_key() {
        eprintln!("{e}");
        std::process::exit(2);
    }
    // Provider keys stored from the set-up page, same rule: before any thread.
    kern::keys::load();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp") {
        mcp::main(&args[1..])
    }
    match recipe::parse(&args) {
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
