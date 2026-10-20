//! THE HOME KERNEL, in this process: its boot on the app's runtime, the calls
//! the shell makes to `home`, the events it keeps, and the orderly shutdown.
//!
//! ```text
//!   setup ──boot──▶ HomeKernel ──▶ capability · navigate · events task · menu · queued paths
//!   ExitRequested ──▶ (wait for the boot) ──▶ shutdown ──▶ app.exit(code)
//! ```
//!
//! The kernel sits behind one `RwLock`: the boot holds it for writing from the
//! moment `setup` spawns it, calls read it, and the shutdown takes it — so a
//! quit during the boot waits for the boot to finish, then shuts it down.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};

use fantastic_home::Intent;
use fantastic_home_kernel::{Config, HomeKernel};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager, RunEvent, Url};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tokio::sync::{mpsc, OwnedRwLockWriteGuard, RwLock};

use crate::fatal::fatal;
use crate::page;
use crate::sys::{self, Backdrop};
use crate::{intents, menu};

/// Every dialog's title, and the window's.
pub const TITLE: &str = "Aisixteen Fantastic";

/// The one window.
pub const WINDOW: &str = "main";

/// The shell's state, managed by Tauri.
pub struct Shell {
    pub kiosk: bool,
    pub backdrop: Backdrop,
    kernel: Arc<RwLock<Option<HomeKernel>>>,
    /// The kernel's port once it serves (0 before): the home's own loopback
    /// origin, which the window may show.
    port: AtomicU16,
    /// The ports of the canvases the home runs, from its latest state: the other
    /// loopback origins the window may show (the Home page's canvas frames).
    canvas_ports: Mutex<Vec<u16>>,
    /// The kernel is down (shut down, or it never booted): an exit passes through.
    down: AtomicBool,
    quitting: AtomicBool,
    failed: AtomicBool,
    /// The paths the OS handed the app, held until the home can take them.
    pub(crate) queue: Mutex<Option<Vec<PathBuf>>>,
    pub(crate) menu: menu::Applied,
}

impl Shell {
    pub fn new(kiosk: bool, backdrop: Backdrop) -> Shell {
        Shell {
            kiosk,
            backdrop,
            kernel: Arc::new(RwLock::new(None)),
            port: AtomicU16::new(0),
            canvas_ports: Mutex::new(Vec::new()),
            down: AtomicBool::new(false),
            quitting: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            queue: Mutex::new(Some(Vec::new())),
            menu: menu::Applied::default(),
        }
    }

    /// The home kernel's port, once it serves.
    pub fn port(&self) -> Option<u16> {
        match self.port.load(Ordering::SeqCst) {
            0 => None,
            p => Some(p),
        }
    }

    /// The ports of the canvases the home runs, as its latest state named them.
    pub fn canvas_ports(&self) -> Vec<u16> {
        self.canvas_ports
            .lock()
            .expect("the canvas ports are poisoned")
            .clone()
    }

    pub fn quitting(&self) -> bool {
        self.quitting.load(Ordering::SeqCst)
    }

    /// The first failure: true once, false after.
    pub(crate) fn fail(&self) -> bool {
        !self.failed.swap(true, Ordering::SeqCst)
    }

    /// Replace the running canvases' ports with a state's.
    pub(crate) fn set_canvas_ports(&self, ports: Vec<u16>) {
        *self
            .canvas_ports
            .lock()
            .expect("the canvas ports are poisoned") = ports;
    }

    /// No kernel will run: a quit passes straight through.
    pub fn set_down(&self) {
        self.down.store(true, Ordering::SeqCst);
    }

    /// Call `home` the way a page does (`user.forward`). A refusal is `Err`.
    pub async fn call(&self, payload: Value) -> Result<Value, String> {
        match self.kernel.read().await.as_ref() {
            Some(k) => k.call(payload).await,
            None => Err("the home kernel is not running".into()),
        }
    }
}

/// The shell's state.
pub fn shell(app: &AppHandle) -> Arc<Shell> {
    Arc::clone(&app.state::<Arc<Shell>>())
}

/// The canvas sidecar: `fantastic` beside this executable, where `externalBin`
/// puts it (and tauri-build copies it next to a dev build).
pub fn canvas_bin() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this executable: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| format!("{} has no directory", exe.display()))?;
    let bin = dir.join(format!("fantastic{}", sys::EXE));
    if !bin.is_file() {
        return Err(format!("the canvas binary {} is missing", bin.display()));
    }
    Ok(bin)
}

/// Spawn the boot. Call it once, from `setup`: it holds the kernel for writing
/// before it returns, so nothing can take the kernel before the boot ends.
pub fn boot(app: &AppHandle, cfg: Config) {
    let shell = shell(app);
    let guard = Arc::clone(&shell.kernel)
        .try_write_owned()
        .expect("the boot is the first to hold the kernel");
    let app = app.clone();
    tauri::async_runtime::spawn(async move { up(app, guard, cfg).await });
}

async fn up(app: AppHandle, mut guard: OwnedRwLockWriteGuard<Option<HomeKernel>>, cfg: Config) {
    let shell = shell(&app);
    let k = match HomeKernel::boot(cfg).await {
        Ok(k) => k,
        Err(e) => {
            shell.set_down();
            drop(guard);
            fatal(&app, &format!("the home kernel did not boot: {e}"));
            return;
        }
    };
    let url = k.url().to_string();
    let port = k.port();
    let events = k.events();
    *guard = Some(k);
    drop(guard);
    shell.port.store(port, Ordering::SeqCst);

    if !shell.kiosk {
        let grant = tauri::ipc::CapabilityBuilder::new("home-page")
            .window(WINDOW)
            .local(false)
            .remote(format!("http://127.0.0.1:{port}/*"))
            .permission("allow-pick-folders");
        if let Err(e) = app.add_capability(grant) {
            fatal(&app, &format!("the Home page's capability: {e}"));
            return;
        }
    }
    let Some(w) = app.get_webview_window(WINDOW) else {
        fatal(&app, "the window is gone");
        return;
    };
    let shown = Url::parse(&url)
        .map_err(|e| format!("{url}: {e}"))
        .and_then(|u| w.navigate(u).map_err(|e| format!("{url}: {e}")));
    if let Err(e) = shown {
        fatal(&app, &format!("the window cannot show the Home page: {e}"));
        return;
    }
    let watcher = app.clone();
    tauri::async_runtime::spawn(async move { watch(watcher, events).await });
    if !shell.kiosk {
        let applied = match shell.call(json!({"type": "menu"})).await {
            Ok(spec) => menu::apply(&app, spec).await,
            Err(e) => Err(e),
        };
        if let Err(e) = applied {
            fatal(&app, &format!("the home's menu: {e}"));
            return;
        }
    }
    intents::ready(&app).await;
    eprintln!("fantastic-app: the home: {url}");
    eprintln!("fantastic-app: the backdrop: {}", shell.backdrop.name());
}

/// The events task: `menu_spec` (applied, outside the kiosk), `state` (the
/// running canvases' ports; the home's problems to stderr in the kiosk) and
/// `focus`; every other payload is a mirrored call, and is dropped.
async fn watch(app: AppHandle, mut events: mpsc::Receiver<Value>) {
    let shell = shell(&app);
    let mut problem = None;
    while let Some(payload) = events.recv().await {
        announce(shell.kiosk, &payload, &mut problem, &mut std::io::stderr());
        match payload.get("type").and_then(Value::as_str) {
            Some(fantastic_home::menu::MENU_SPEC) if !shell.kiosk => {
                if let Err(e) = menu::apply(&app, payload).await {
                    fatal(&app, &format!("the home's menu: {e}"));
                }
            }
            Some("state") => shell.set_canvas_ports(page::running_canvas_ports(&payload)),
            Some("focus") => focus(&app),
            _ => {}
        }
    }
    if !shell.quitting() {
        fatal(&app, "the home kernel stopped");
    }
}

/// In the kiosk, a `state` payload whose problem is new (not the one `last`
/// holds) is written to `out` as `fantastic-app: <problem>`: nobody sees a toast
/// there, so the session journal holds it. A cleared problem is forgotten, so
/// the same failure after it is new again.
fn announce(
    kiosk: bool,
    payload: &Value,
    last: &mut Option<String>,
    out: &mut impl std::io::Write,
) {
    if !kiosk || payload.get("type").and_then(Value::as_str) != Some("state") {
        return;
    }
    let problem = payload
        .get("problem")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(text) = problem.as_ref().filter(|p| Some(*p) != last.as_ref()) {
        let _ = writeln!(out, "fantastic-app: {text}");
    }
    *last = problem;
}

/// Bring the window forward.
fn focus(app: &AppHandle) {
    let Some(w) = app.get_webview_window(WINDOW) else {
        return;
    };
    let shown = w
        .unminimize()
        .and_then(|_| w.show())
        .and_then(|_| w.set_focus());
    if let Err(e) = shown {
        eprintln!("fantastic-app: focus: {e}");
    }
}

/// Send `intent` to `home`; a refusal is shown, not fatal (the home changed nothing).
pub async fn send(app: &AppHandle, intent: Intent) {
    let payload = serde_json::to_value(&intent).expect("an intent serializes");
    if let Err(e) = shell(app).call(payload).await {
        warn(app, &e);
    }
}

/// A refusal of the shell's own: a warning dialog, or stderr in the kiosk.
pub fn warn(app: &AppHandle, text: &str) {
    if shell(app).kiosk {
        eprintln!("fantastic-app: {text}");
        return;
    }
    app.dialog()
        .message(text)
        .kind(MessageDialogKind::Warning)
        .title(TITLE)
        .show(|_| {});
}

/// `RunEvent::ExitRequested`: while the kernel may run, the first request is
/// held, the kernel shut down in order (after its boot, if it is booting), and
/// the app exits with `code`. A request once the kernel is down passes through.
pub fn exit_requested(app: &AppHandle, event: &RunEvent) {
    let RunEvent::ExitRequested { code, api, .. } = event else {
        return;
    };
    let shell = shell(app);
    if shell.down.load(Ordering::SeqCst) {
        return;
    }
    api.prevent_exit();
    if shell.quitting.swap(true, Ordering::SeqCst) {
        return;
    }
    let code = code.unwrap_or(0);
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let shell = self::shell(&app);
        let k = shell.kernel.write().await.take();
        shell.set_down();
        let code = match k {
            None => code,
            Some(k) => match k.shutdown().await {
                Ok(()) => code,
                Err(e) => {
                    eprintln!("fantastic-app: shutdown: {e}");
                    1
                }
            },
        };
        app.exit(code);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(problem: Value) -> Value {
        json!({"type": "state", "problem": problem})
    }

    fn said(kiosk: bool, payloads: &[Value]) -> String {
        let mut last = None;
        let mut out = Vec::new();
        for p in payloads {
            announce(kiosk, p, &mut last, &mut out);
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_new_problem_is_one_stderr_line_in_the_kiosk() {
        let why = "Could not add alpha: it did not start — no login environment: SHELL is not set";
        assert_eq!(
            said(
                true,
                &[state(Value::Null), state(json!(why)), state(json!(why))]
            ),
            format!("fantastic-app: {why}\n")
        );
    }

    #[test]
    fn outside_the_kiosk_stderr_stays_quiet() {
        assert_eq!(said(false, &[state(json!("a"))]), "");
    }

    #[test]
    fn a_problem_after_a_cleared_one_is_new_again() {
        let out = said(
            true,
            &[state(json!("a")), state(Value::Null), state(json!("a"))],
        );
        assert_eq!(out, "fantastic-app: a\nfantastic-app: a\n");
    }

    #[test]
    fn no_problem_and_other_payloads_say_nothing() {
        assert_eq!(said(true, &[state(Value::Null)]), "");
        assert_eq!(said(true, &[json!({"type": "focus", "problem": "x"})]), "");
    }
}
