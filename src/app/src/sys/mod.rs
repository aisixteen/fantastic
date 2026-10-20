//! THE OS LAYER: every platform call of the shell, one small named function
//! each, one file per OS family (the CLI's `src/tui/src/sys` pattern). Call
//! sites say `sys::…`; only this module knows the platform
//! (`tests/sys_only.rs` holds the rest of the crate to that).
//!
//! Each family exports the same names:
//!
//! ```text
//!   OS, WEBVIEW, MENU_OS, EXE
//!   backdrop()                 the material behind the window, from the OS version, once
//!   style(builder)             the non-kiosk window
//!   apply_backdrop(w, b)       on the main thread
//!   opened_paths(event)        the folders a Finder / Dock / `open -a` hands the app (macOS)
//!   on_terminate(app)          the OS's polite stop → app.exit(0)
//!   system_menu(submenu, s)    NSApp's Window / Help menu (macOS)
//! ```

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
compile_error!("the desktop app supports macOS, Windows and Linux only");

/// The material behind the window. Plain data.
#[allow(dead_code)] // each OS constructs only its own materials
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backdrop {
    /// macOS 26+.
    LiquidGlass,
    /// macOS 14 and 15.
    Vibrancy,
    /// Windows 11.
    Mica,
    /// An opaque window: the page paints its own backdrop.
    None,
}

impl Backdrop {
    /// What `__FANTASTIC_SHELL__.backdrop` says: `native` for every material.
    pub fn flag(self) -> &'static str {
        match self {
            Backdrop::None => "web",
            _ => "native",
        }
    }

    /// Its name on stderr.
    pub fn name(self) -> &'static str {
        match self {
            Backdrop::LiquidGlass => "liquid-glass",
            Backdrop::Vibrancy => "vibrancy",
            Backdrop::Mica => "mica",
            Backdrop::None => "none",
        }
    }
}

/// A `file:` URL the OS opened (macOS), as a path; any other URL is refused.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))] // only macOS hands the app URLs
pub fn file_path(url: &tauri::Url) -> Result<std::path::PathBuf, String> {
    if url.scheme() != "file" {
        return Err(format!("{url} is not a file"));
    }
    url.to_file_path()
        .map_err(|()| format!("{url} is not a local file"))
}

/// SIGTERM → `app.exit(0)` (macOS and Linux).
#[cfg(unix)]
fn unix_terminate(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                if term.recv().await.is_some() {
                    app.exit(0);
                }
            }
            Err(e) => crate::fatal::fatal(&app, &format!("SIGTERM: {e}")),
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{file_path, Backdrop};
    use tauri::Url;

    #[test]
    fn file_urls_map_to_paths_and_a_non_file_url_is_refused() {
        let dir = std::env::temp_dir().join("a folder");
        let url = Url::from_file_path(&dir).expect("an absolute path");
        assert_eq!(file_path(&url), Ok(dir));
        let web = Url::parse("https://example.com/x").unwrap();
        assert_eq!(
            file_path(&web),
            Err("https://example.com/x is not a file".into())
        );
    }

    #[test]
    fn every_material_is_native_and_none_is_web() {
        for b in [Backdrop::LiquidGlass, Backdrop::Vibrancy, Backdrop::Mica] {
            assert_eq!(b.flag(), "native");
        }
        assert_eq!(Backdrop::None.flag(), "web");
        assert_eq!(Backdrop::LiquidGlass.name(), "liquid-glass");
    }
}
