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
//! A LIBRARY (#248): the `fantastic` binary's `main` (`src/main.rs`) is
//! `if starts_kernel(&argv) { seed_dir_with(&state_dir_of(&argv)?, &[]) }` then
//! `main_with(argv, &[], &[])`. Another binary (aisixteen/fantastic_shift: a headless
//! one, an app) follows the same pattern with its own blueprints, extras and readme sections:
//! each [`Extra`] it passes registers on the host's registry AFTER the stock
//! set, in every mode that boots a kernel (the UI, `--headless`, `canvas`,
//! `mcp`). Each readme section is printed after fantastic's own readme (`readme`,
//! and the readme printed with no terminal), in order. THE LAUNCHER SEEDS, before the kernel starts. `main_with` and the
//! kernel never seed. A folder that is not fresh is left alone (no-op), and the
//! fantastic home folder is refused inside [`seed_dir_with`]. The blueprints are
//! written after the stock one, ids as written, and every later run rehydrates
//! from disk. A blueprint entry naming a stock agent (or the root, `@root`) may
//! add `callable_by` / `can_call` grants to it: merged as a union, never
//! narrowing a stock grant.
//!
//! A HOST THAT BOOTS ITS OWN KERNEL (a window shell, its unit tests) seeds with
//! [`seed_dir_with`], then calls [`boot_kernel`] and, at the end, [`stop_kernel`]: the one
//! boot and the one stop the binary's modes run, with the stock bundles plus its own
//! [`Extra`]s. It never seeds and never reads `main_with`'s extras. The MCP channel and the
//! share socket are opened only by the binary's modes, never by these two.

mod app;
mod atomic;
mod boot;
mod env;
mod ev;
mod import;
mod kern;
mod keys;
mod local;
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
#[cfg(test)]
mod share_tests;
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
/// Seed a directory like the `fantastic` CLI's first run: writes the blueprint
/// tree (the `user` record and its children) when `.fantastic` is absent and
/// returns `Ok(true)`; a no-op returning `Ok(false)` once it exists.
/// [`seed_dir_with`] also writes an embedder's own blueprints, ids as written.
/// A launcher calls these before `main_with`, gated by [`starts_kernel`].
///
/// They take the STATE folder: a launcher derives it from the command line with
/// [`state_dir_of`] (`--statedir DIR`, else `<--workdir DIR or the current dir>/.fantastic`).
pub use boot::seed::{seed_dir, seed_dir_with};

/// BOOT ONE STATE FOLDER IN THIS PROCESS: the stock registry plus `extras`, the kernel's start, the
/// folder's recorded agents booted and the grant watch started: the same boot the modes run
/// (`kern::host::boot_all`), with no canvas. Returns the running kernel; end it with
/// [`stop_kernel`]. It never seeds: the caller has run [`seed_dir_with`]. `web_port` /
/// `web_bind` are what `--port` / `--host` pass: set on the folder's recorded `web` before its
/// listener boots (a `web` with no port and none given picks a free one and records it); with
/// no `web` record in the folder they are refused. A folder a kernel already holds is refused
/// with the lock error. `state_dir` and `work_dir` are what the kernel reports as
/// `state_dir()` and `work_dir()`: its records and loader live in the first, the
/// bundles clamp their non-state paths against the second.
pub async fn boot_kernel(
    state_dir: &std::path::Path,
    work_dir: &std::path::Path,
    extras: &[Extra],
    web_port: Option<u16>,
    web_bind: Option<String>,
) -> Result<std::sync::Arc<fantastic_kernel::Kernel>, String> {
    let paths = env::Paths::of(state_dir.to_path_buf(), work_dir.to_path_buf());
    paths.refuse_the_home()?;
    let (host, _) = kern::host::boot_all(&paths, extras, web_port, web_bind, false).await?;
    Ok(std::sync::Arc::clone(&host.kernel))
}

/// STOP A KERNEL [`boot_kernel`] returned, in the kernel's order: the pages, the drain, the
/// store close, then the lock release. The same stop the modes end in.
pub async fn stop_kernel(kernel: &std::sync::Arc<fantastic_kernel::Kernel>) {
    kern::host::stop(kernel).await
}

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

/// THE STATE FOLDER a command line names, for a launcher that seeds it
/// ([`seed_dir_with`]) before [`main_with`]: `--statedir DIR` when given, else
/// `<--workdir DIR, or the current dir>/.fantastic`, for the UI, `--headless`,
/// `canvas` and `mcp`. The one derivation is the kernel's (`state_dir_for`).
///
/// # Errors
/// A command line that does not parse, or the current dir cannot be read.
pub fn state_dir_of(args: &[String]) -> Result<std::path::PathBuf, String> {
    let (statedir, workdir) = match args.first().map(String::as_str) {
        Some("mcp") => mcp::flags(&args[1..])?,
        _ => {
            let o = recipe::parse(args)?;
            (o.statedir, o.workdir)
        }
    };
    fantastic_kernel::bootstrap::state_dir_for(statedir.as_deref(), workdir.as_deref()).map_err(|e| e.to_string())
}

/// THE LAUNCHER'S PREDICATE. True when `main_with(args, …)` would boot a kernel
/// in the current directory: the UI (on a terminal), `--headless`, `canvas`, and
/// an `mcp` with no flag but `--statedir` and `--workdir`. False for the one-shot commands (`send`, `readme`, `import`,
/// `migrate-home`, `__ctrl_break`), for the readme printed when there is no
/// terminal and no argument, and for a refused command line (a parse error,
/// `mcp` with another argument, the UI with no terminal). A launcher calls it before
/// `main_with` and, when it is true, seeds with `seed_dir_with(&state_dir_of(argv)?, its
/// blueprints)`. Nothing in the lib or the kernel calls it, and the kernel never
/// seeds.
pub fn starts_kernel(args: &[String]) -> bool {
    kernel_mode(args, std::io::stdout().is_terminal(), &|| sys::open_tty().is_ok())
}

/// [`starts_kernel`] with the environment injected; mirrors the dispatch order
/// of [`main_with`].
fn kernel_mode(args: &[String], stdout_is_terminal: bool, has_tty: &dyn Fn() -> bool) -> bool {
    if args.is_empty() && !stdout_is_terminal {
        return false;
    }
    match args.first().map(String::as_str) {
        Some("mcp") => return mcp::flags(&args[1..]).is_ok(),
        Some("readme" | "send" | "import" | "migrate-home" | "__ctrl_break") => return false,
        _ => {}
    }
    match recipe::parse(args) {
        Ok(o) => o.headless || has_tty(),
        Err(_) => false,
    }
}

/// The whole `fantastic` command line. `args` WITHOUT the program name
/// (`std::env::args().skip(1)`); `extras`: the embedder's bundles (`&[]` for
/// `fantastic` itself). `readme_sections`: the embedder's readme sections (`&[]`
/// for `fantastic`): `readme` and the no-terminal readme print fantastic's text,
/// then each, in order. The mode dispatch and nothing else: the loop lives in
/// `run`, the state in `app`, the headless carrier in `mcp`. It never seeds; the
/// launcher has done that.
///
/// A PLAIN fn, not async: some modes must rearrange the process (stdio, env)
/// before any runtime thread exists — call it from a plain `fn main`.
pub fn main_with(args: Vec<String>, extras: &[Extra], readme_sections: &[&str]) -> Result<()> {
    set_extras(extras);
    // NOT ON A TERMINAL, NO ARGUMENTS: the readme, and nothing runs.
    if args.is_empty() && !std::io::stdout().is_terminal() {
        print!("{}", readme::text(readme_sections));
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("mcp") {
        mcp::main(&args[1..])
    }
    if args.first().map(String::as_str) == Some("readme") {
        readme::main(&args[1..], readme_sections)
    }
    if args.first().map(String::as_str) == Some("send") {
        send::main(&args[1..])
    }
    if args.first().map(String::as_str) == Some("import") {
        import::main(&args[1..])
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

#[cfg(test)]
mod tests {
    use super::{kernel_mode, state_dir_of};

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn starts_kernel_is_true_for_each_mode_that_boots_a_kernel() {
        for s in ["", "--headless", "canvas", "canvas --port 8123", "mcp", "mcp --statedir s", "mcp --workdir w --statedir s", "ai", "run htop"] {
            assert!(kernel_mode(&a(s), true, &|| true), "{s:?}");
        }
        for s in ["--headless", "canvas"] {
            assert!(kernel_mode(&a(s), true, &|| false), "{s:?} needs no terminal");
        }
    }

    #[test]
    fn starts_kernel_is_false_for_one_shots_and_refusals() {
        for s in [
            "readme", "readme mcp", "send x", "import p --parent user", "migrate-home", "migrate-home --dry-run",
            "__ctrl_break 1", "mcp --can-call files", "mcp --bogus", "bogus", "--headless canvas",
        ] {
            assert!(!kernel_mode(&a(s), true, &|| true), "{s:?}");
        }
        assert!(!kernel_mode(&a(""), false, &|| true), "the bare readme");
        assert!(!kernel_mode(&a(""), true, &|| false), "the UI with no terminal");
    }

    #[test]
    fn state_dir_of_derives_from_workdir_or_takes_statedir() {
        let v = |s: &str| state_dir_of(&a(s)).unwrap();
        assert_eq!(v("--headless --workdir /w"), std::path::PathBuf::from("/w/.fantastic"));
        assert_eq!(v("canvas --statedir /s"), std::path::PathBuf::from("/s"));
        assert_eq!(v("mcp"), std::env::current_dir().unwrap().join(".fantastic"));
        assert_eq!(v("mcp --workdir /w --statedir /s"), std::path::PathBuf::from("/s"));
        assert!(state_dir_of(&a("mcp --bogus")).is_err());
        assert!(state_dir_of(&a("bogus")).is_err());
    }
}
