//! THE PATHS THE OS HANDS THE APP, each batch one `open {paths}` call; and the
//! folder dialog.
//!
//! ```text
//!   argv · Finder / Dock / `open -a` (macOS, `sys::opened_paths`) · a drop on the window ──▶ open(paths)
//!   ⌘O / Open Folder (the menu) · the Home card's pickFolder() ──▶ ask_folders
//! ```
//!
//! Paths go to the home absolute and as given: the home decides what a folder
//! is. They are held until the kernel is up and its first menu is applied,
//! then sent in order.

use std::path::PathBuf;

use fantastic_home::Intent;
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;

use crate::home;

/// A path as the home takes it: UTF-8, or refused.
pub fn text(path: PathBuf) -> Result<String, String> {
    path.into_os_string()
        .into_string()
        .map_err(|p| format!("{} is not a UTF-8 path", PathBuf::from(p).display()))
}

/// Open `paths` (one `open` call), now or once the home can take them.
pub fn open(app: &AppHandle, paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    let shell = home::shell(app);
    {
        let mut queue = shell.queue.lock().expect("the queue is poisoned");
        if let Some(held) = queue.as_mut() {
            held.extend(paths);
            return;
        }
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move { send(&app, paths).await });
}

/// The home can take paths: send the held ones, and every later one at once.
pub async fn ready(app: &AppHandle) {
    let held = home::shell(app)
        .queue
        .lock()
        .expect("the queue is poisoned")
        .take()
        .unwrap_or_default();
    send(app, held).await;
}

async fn send(app: &AppHandle, paths: Vec<PathBuf>) {
    let mut texts = Vec::with_capacity(paths.len());
    for p in paths {
        match text(p) {
            Ok(t) => texts.push(t),
            Err(e) => home::warn(app, &e),
        }
    }
    if !texts.is_empty() {
        home::send(app, Intent::Open { paths: texts }).await;
    }
}

/// Ask the user for folders with the OS's own dialog (GTK 3's on Linux, in
/// process). Cancel is `[]`.
pub async fn ask_folders(app: &AppHandle) -> Result<Vec<String>, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Open Folder")
        .pick_folders(move |picked| {
            let _ = tx.send(picked);
        });
    let picked = rx
        .await
        .map_err(|_| "the folder dialog closed without an answer".to_string())?;
    let mut out = Vec::new();
    for f in picked.unwrap_or_default() {
        let path = f
            .into_path()
            .map_err(|e| format!("the folder dialog: {e}"))?;
        out.push(text(path)?);
    }
    Ok(out)
}

/// The Home page's `__FANTASTIC_SHELL__.pickFolder()`: the chosen folders; the
/// page sends `open` itself. Only the Home origin may call it (one capability).
#[tauri::command]
pub async fn pick_folders(app: AppHandle) -> Result<Vec<String>, String> {
    ask_folders(&app).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_utf8_path_is_text() {
        let p = std::env::temp_dir().join("notes");
        assert_eq!(text(p.clone()), Ok(p.to_str().unwrap().to_string()));
    }
}
