//! Windows: WebView2, Mica on Windows 11 (opaque on 10), the native title bar.
//!
//! Release builds use the GUI subsystem (no console). Canvases start in hidden
//! consoles of their own and stop through the CLI's `__ctrl_break` helper (the
//! library's `sys/windows.rs`). Folders arrive as argv.

use std::path::PathBuf;

use fantastic_home::menu::{Os, System};
use tauri::menu::Submenu;
use tauri::webview::WebviewWindowBuilder;
use tauri::{AppHandle, RunEvent, WebviewWindow, Wry};

use super::Backdrop;

pub const OS: &str = "windows";
pub const WEBVIEW: &str = "webview2";
pub const MENU_OS: Os = Os::Windows;
pub const EXE: &str = ".exe";

/// The first Windows 11 build, the lowest at which window-vibrancy 0.8.1's
/// `apply_mica` succeeds. Acrylic is not used: window-vibrancy documents poor
/// performance for it while resizing or dragging.
const MICA_BUILD: u32 = 22000;

fn for_build(b: u32) -> Backdrop {
    if b >= MICA_BUILD {
        Backdrop::Mica
    } else {
        Backdrop::None
    }
}

pub fn backdrop() -> Backdrop {
    for_build(windows_version::OsVersion::current().build)
}

/// The native title bar.
pub fn style<'a>(
    b: WebviewWindowBuilder<'a, Wry, AppHandle>,
) -> WebviewWindowBuilder<'a, Wry, AppHandle> {
    b
}

/// Mica follows the system's dark or light mode.
pub fn apply_backdrop(w: &WebviewWindow, b: Backdrop) -> Result<(), String> {
    match b {
        Backdrop::Mica => {
            window_vibrancy::apply_mica(w, None).map_err(|e| format!("the mica backdrop: {e}"))
        }
        Backdrop::None => Ok(()),
        other => Err(format!("no {} backdrop on Windows", other.name())),
    }
}

/// Folders arrive as argv on Windows.
pub fn opened_paths(_e: &RunEvent) -> Vec<Result<PathBuf, String>> {
    Vec::new()
}

/// Nothing: a GUI process gets no console control events, and Windows ends a
/// session through the window.
pub fn on_terminate(_app: &AppHandle) {}

/// NSApp's menus exist on macOS only.
pub fn system_menu(_sub: &Submenu<Wry>, _system: System) -> tauri::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mica_from_build_22000() {
        assert_eq!(for_build(21999), Backdrop::None);
        assert_eq!(for_build(22000), Backdrop::Mica);
    }
}
