//! macOS: WKWebView, Liquid Glass (26+) or vibrancy behind a transparent
//! webview, a transparent title bar, folders from Finder and the Dock, SIGTERM.
//!
//! The canvas sidecar is `Contents/MacOS/fantastic`. The minimum is macOS 14.
//! ATS does not apply to `127.0.0.1`, and a loopback listener raises no firewall
//! prompt. A Finder-launched app has a minimal `PATH`; the library starts
//! canvases with the login shell's environment.

use std::path::PathBuf;

use fantastic_home::menu::{Os, System};
use tauri::menu::Submenu;
use tauri::webview::WebviewWindowBuilder;
use tauri::{AppHandle, RunEvent, WebviewWindow, Wry};
use window_vibrancy::{
    LiquidGlassOptions, NSGlassEffectViewStyle, NSVisualEffectMaterial, NSVisualEffectState,
};

use super::Backdrop;

pub const OS: &str = "macos";
pub const WEBVIEW: &str = "wkwebview";
pub const MENU_OS: Os = Os::Macos;
pub const EXE: &str = "";

/// `MIN_APPKIT_VERSION_LIQUID_GLASS` in window-vibrancy 0.8.1: macOS 26. Deciding
/// with the number the crate checks means `apply_liquid_glass` cannot answer
/// `UnsupportedPlatformVersion`.
const LIQUID_GLASS_APPKIT: f64 = 2685.0;

fn for_appkit(v: f64) -> Backdrop {
    if v >= LIQUID_GLASS_APPKIT {
        Backdrop::LiquidGlass
    } else {
        Backdrop::Vibrancy
    }
}

pub fn backdrop() -> Backdrop {
    // SAFETY: an immutable AppKit global, set before `main` runs.
    for_appkit(unsafe { objc2_app_kit::NSAppKitVersionNumber })
}

/// A transparent title bar with no title: native title-bar dragging stays.
pub fn style<'a>(
    b: WebviewWindowBuilder<'a, Wry, AppHandle>,
) -> WebviewWindowBuilder<'a, Wry, AppHandle> {
    b.title_bar_style(tauri::TitleBarStyle::Transparent)
        .hidden_title(true)
}

/// On the main thread (else the crate answers `NotMainThread`).
pub fn apply_backdrop(w: &WebviewWindow, b: Backdrop) -> Result<(), String> {
    let done = match b {
        Backdrop::LiquidGlass => window_vibrancy::apply_liquid_glass(
            w,
            LiquidGlassOptions::new(NSGlassEffectViewStyle::Regular),
        ),
        Backdrop::Vibrancy => window_vibrancy::apply_vibrancy(
            w,
            NSVisualEffectMaterial::UnderWindowBackground,
            Some(NSVisualEffectState::FollowsWindowActiveState),
            None,
        ),
        other => return Err(format!("no {} backdrop on macOS", other.name())),
    };
    done.map_err(|e| format!("the {} backdrop: {e}", b.name()))
}

/// Finder, a Dock drop and `open -a` (the bundle declares folders as documents).
pub fn opened_paths(e: &RunEvent) -> Vec<Result<PathBuf, String>> {
    match e {
        RunEvent::Opened { urls } => urls.iter().map(super::file_path).collect(),
        _ => Vec::new(),
    }
}

/// SIGTERM → `app.exit(0)`, so it reaches the orderly shutdown.
pub fn on_terminate(app: &AppHandle) {
    super::unix_terminate(app);
}

pub fn system_menu(sub: &Submenu<Wry>, system: System) -> tauri::Result<()> {
    match system {
        System::Window => sub.set_as_windows_menu_for_nsapp(),
        System::Help => sub.set_as_help_menu_for_nsapp(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liquid_glass_from_appkit_2685() {
        assert_eq!(for_appkit(2684.9), Backdrop::Vibrancy);
        assert_eq!(for_appkit(2685.0), Backdrop::LiquidGlass);
    }
}
