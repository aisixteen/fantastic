//! AISIXTEEN FANTASTIC, the desktop app: a thin Tauri 2 shell around the home
//! kernel, which it hosts in this process. Wiring only — no logic lives here.
//!
//! ```text
//!   args · runtime · home dir · one host per home (hand over and exit) · the canvas sidecar
//!   └─ Tauri: setup (window, backdrop, queued paths, SIGTERM, boot) · run(on_event)
//! ```
//!
//! ```text
//!   args      argv → {kiosk, paths}
//!   home      the in-process kernel: boot, calls, events, the orderly shutdown
//!   menu      menu spec → native menus; a click → its intent
//!   intents   the paths the OS hands the app → `open`; the folder dialog
//!   flag      the `__FANTASTIC_SHELL__` initialization script
//!   page      which top-level URLs the window may show
//!   fatal     the one failure path
//!   sys       every platform call
//! ```

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod args;
mod fatal;
mod flag;
mod home;
mod intents;
mod menu;
mod page;
mod sys;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use fantastic_home_kernel::Config;
use tauri::{
    App, AppHandle, DragDropEvent, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

use crate::args::Args;
use crate::fatal::fatal;
use crate::home::Shell;
use crate::sys::Backdrop;

fn main() -> ExitCode {
    let cwd = match std::env::current_dir() {
        Ok(d) => d,
        Err(e) => return die(1, &format!("the current directory: {e}")),
    };
    let args = match args::parse(std::env::args_os().skip(1), &cwd) {
        Ok(a) => a,
        Err(e) => return die(2, &e),
    };
    // THE RUNTIME, shared with Tauri before anything else; alive for all of `main`.
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => return die(1, &format!("the runtime: {e}")),
    };
    tauri::async_runtime::set(rt.handle().clone());

    let home = match fantastic_home_kernel::home_dir_from_env() {
        Ok(h) => h,
        Err(e) => return die(1, &e),
    };
    // ONE HOST PER HOME: a live one gets our folders and comes forward; we exit.
    match rt.block_on(fantastic_home_kernel::running(&home)) {
        Err(e) => return die(1, &e),
        Ok(Some(host)) => {
            let mut paths = Vec::new();
            for p in args.paths {
                match intents::text(p) {
                    Ok(t) => paths.push(t),
                    Err(e) => eprintln!("fantastic-app: {e}"),
                }
            }
            return match rt.block_on(fantastic_home_kernel::hand_over(&host, paths)) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => die(
                    1,
                    &format!(
                        "another app hosts {} (pid {}), and it did not answer: {e}",
                        home.display(),
                        host.pid
                    ),
                ),
            };
        }
        Ok(None) => {}
    }
    let canvas_bin = match home::canvas_bin() {
        Ok(b) => b,
        Err(e) => return die(1, &e),
    };

    let backdrop = sys::backdrop();
    let built = tauri::Builder::default()
        .manage(Arc::new(Shell::new(args.kiosk, backdrop)))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![intents::pick_folders])
        .setup(move |app| {
            setup(app, home, args, canvas_bin, backdrop);
            Ok(())
        })
        .build(tauri::generate_context!());
    match built {
        Ok(app) => app.run(on_event),
        Err(e) => return die(1, &format!("tauri: {e}")),
    }
    drop(rt);
    ExitCode::SUCCESS
}

fn die(code: u8, text: &str) -> ExitCode {
    eprintln!("fantastic-app: {text}");
    ExitCode::from(code)
}

/// On the main thread: the window and its backdrop, the queued paths, SIGTERM, the boot.
fn setup(app: &mut App, home: PathBuf, args: Args, canvas_bin: PathBuf, backdrop: Backdrop) {
    let handle = app.handle().clone();
    if let Err(e) = window(&handle, &args, backdrop) {
        home::shell(&handle).set_down();
        fatal(&handle, &e);
        return;
    }
    intents::open(&handle, args.paths);
    sys::on_terminate(&handle);
    home::boot(
        &handle,
        Config {
            home,
            port: None,
            fantastic_bin: canvas_bin,
        },
    );
}

/// The one window, on the boot page until the kernel serves.
fn window(app: &AppHandle, args: &Args, backdrop: Backdrop) -> Result<(), String> {
    let shell = home::shell(app);
    let mut b = WebviewWindowBuilder::new(app, home::WINDOW, WebviewUrl::App("index.html".into()))
        .title(home::TITLE)
        .inner_size(1280.0, 800.0)
        .min_inner_size(720.0, 480.0)
        .transparent(backdrop != Backdrop::None)
        .initialization_script(flag::script(
            sys::OS,
            sys::WEBVIEW,
            backdrop.flag(),
            !args.kiosk,
        ))
        .on_navigation(move |url| page::allowed(url, shell.port(), &shell.canvas_ports()));
    if args.kiosk {
        b = b.fullscreen(true).decorations(false);
    } else {
        b = sys::style(b);
    }
    let w = b.build().map_err(|e| format!("the window: {e}"))?;
    sys::apply_backdrop(&w, backdrop)
}

fn on_event(app: &AppHandle, event: RunEvent) {
    for opened in sys::opened_paths(&event) {
        match opened {
            Ok(p) => intents::open(app, vec![p]),
            Err(e) => home::warn(app, &e),
        }
    }
    match &event {
        RunEvent::MenuEvent(e) => menu::clicked(app, e.id().as_ref()),
        RunEvent::WindowEvent {
            event: WindowEvent::DragDrop(DragDropEvent::Drop { paths, .. }),
            ..
        } => intents::open(app, paths.clone()),
        RunEvent::ExitRequested { .. } => home::exit_requested(app, &event),
        _ => {}
    }
}
