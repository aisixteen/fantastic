//! Linux: WebKitGTK 4.1, an opaque window (window-vibrancy does not support
//! Linux; the compositor owns blur), a GTK menubar outside the kiosk, SIGTERM
//! (how systemd and `cage` stop the kiosk).
//!
//! The app needs no D-Bus session of its own, no tray, no portal and no desktop
//! environment: the folder dialog is GTK 3's own, in process.

use std::path::PathBuf;

use fantastic_home::menu::{Os, System};
use tauri::menu::Submenu;
use tauri::webview::WebviewWindowBuilder;
use tauri::{AppHandle, RunEvent, WebviewWindow, Wry};

use super::Backdrop;

pub const OS: &str = "linux";
pub const WEBVIEW: &str = "webkitgtk";
pub const MENU_OS: Os = Os::Linux;
pub const EXE: &str = "";

pub fn backdrop() -> Backdrop {
    Backdrop::None
}

pub fn style<'a>(
    b: WebviewWindowBuilder<'a, Wry, AppHandle>,
) -> WebviewWindowBuilder<'a, Wry, AppHandle> {
    b
}

pub fn apply_backdrop(_w: &WebviewWindow, b: Backdrop) -> Result<(), String> {
    match b {
        Backdrop::None => Ok(()),
        _ => Err("no native backdrop on Linux".into()),
    }
}

pub fn opened_paths(_e: &RunEvent) -> Vec<Result<PathBuf, String>> {
    Vec::new()
}

/// SIGTERM → `app.exit(0)`, so it reaches the orderly shutdown.
pub fn on_terminate(app: &AppHandle) {
    super::unix_terminate(app);
}

pub fn system_menu(_sub: &Submenu<Wry>, _system: System) -> tauri::Result<()> {
    Ok(())
}
