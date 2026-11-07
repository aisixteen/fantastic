//! THE ONE FAILURE PATH. Nothing retries and nothing continues without the
//! kernel: the text goes to stderr, then (outside fullscreen mode) to an error
//! dialog whose dismissal exits 1. In fullscreen mode it exits 1 at once — a modal
//! dialog would block the screen, and the session manager restarts the app.

use tauri::AppHandle;
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::home;

/// Fail with `text`. While the app is quitting, or after the first failure, it does nothing.
pub fn fatal(app: &AppHandle, text: &str) {
    let shell = home::shell(app);
    if shell.quitting() || !shell.fail() {
        return;
    }
    eprintln!("fantastic-app: {text}");
    if shell.fullscreen {
        app.exit(1);
        return;
    }
    let handle = app.clone();
    app.dialog()
        .message(text)
        .kind(MessageDialogKind::Error)
        .title(home::TITLE)
        .show(move |_| handle.exit(1));
}
