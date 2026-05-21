//! UniFFI-bound Swift surface for embedding the kernel.
//!
//! See `fantastic.udl` for the typed API exported to Swift. The
//! canonical Swift↔kernel data path stays HTTP + WS on
//! `127.0.0.1:<port>`; this crate only owns LIFECYCLE — bootstrap +
//! port discovery + send_json shortcut + clean shutdown.
//!
//! Build via `scripts/build-xcframework.sh`; the resulting
//! `Fantastic.xcframework` is wrapped in the SPM package at
//! `rust/packaging/FantasticKernel/`.

#![deny(missing_docs)]

use fantastic_kernel::bootstrap::{self, BootstrapOptions};
use fantastic_kernel::{AgentId, BundleRegistry};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Callback bridge for the kernel's state stream. UniFFI generates a
/// Swift protocol of the same name; consumers implement it on their
/// side and hand an instance to [`Kernel::subscribe`].
///
/// `on_event` fires inline on the kernel's dispatching task. Keep the
/// implementation short — off-thread heavy work via Swift's
/// structured concurrency (`Task.detached`, `MainActor.run`, etc.).
pub trait StateListener: Send + Sync {
    /// One JSON-serialized state event per call. Shapes:
    /// `{"type":"send",  sender, target, verb, summary}`
    /// `{"type":"emit",  sender, target, verb, summary}`
    /// `{"type":"created", id, parent_id, handler_module}`
    /// `{"type":"removed", id}`
    /// `{"type":"updated", id}`
    fn on_event(&self, event_json: String);
}

/// Errors surfaced to Swift via UniFFI's `[Throws=KernelError]`.
#[derive(Debug, Error)]
pub enum KernelError {
    /// `workdir` doesn't exist or isn't readable.
    #[error("workdir invalid: {0}")]
    WorkdirInvalid(String),
    /// axum couldn't bind to the requested port.
    #[error("port bind failed: {0}")]
    PortBindFailed(String),
    /// Substrate boot failed (lock contention, bad agent.json, etc.).
    #[error("boot failed: {0}")]
    BootFailed(String),
    /// `start_kernel` called more than once for the same workdir
    /// without an intervening `shutdown()`.
    #[error("already running")]
    AlreadyRunning,
    /// Catch-all for unexpected failures.
    #[error("internal: {0}")]
    Internal(String),
}

/// Default bundle set linked into the Swift-embedded build.
///
/// Always-available bundles compile under both the `embedded` (iOS /
/// sandboxed) and `desktop` (unsandboxed) features. Full-tier-only
/// bundles are added via the `#[cfg(feature = "full")]` gate below
/// — currently none, but the placeholder keeps the contract explicit
/// so the iOS-sandbox guarantee doesn't silently regress when a
/// subprocess-using bundle gets ported.
fn register_default_bundles() -> BundleRegistry {
    let mut reg = BundleRegistry::new();

    // ── Always-available (iOS-safe).
    reg.register("file.tools", fantastic_file::FileBundle);
    reg.register("web.tools", fantastic_web::WebBundle);
    reg.register("web_ws.tools", fantastic_web_ws::WebWsBundle);
    reg.register("web_rest.tools", fantastic_web_rest::WebRestBundle);
    reg.register("html_agent.tools", fantastic_html_agent::HtmlAgentBundle);
    reg.register(
        "canvas_backend.tools",
        fantastic_canvas_backend::CanvasBackendBundle,
    );
    reg.register(
        "canvas_webapp.tools",
        fantastic_canvas_webapp::CanvasWebappBundle,
    );
    reg.register("scheduler.tools", fantastic_scheduler::SchedulerBundle);
    reg.register("gl_agent.tools", fantastic_gl_agent::GlAgentBundle);
    reg.register(
        "telemetry_pane.tools",
        fantastic_telemetry_pane::TelemetryPaneBundle,
    );
    reg.register(
        "ai_chat_webapp.tools",
        fantastic_ai_chat_webapp::AiChatWebappBundle,
    );
    reg.register(
        "terminal_webapp.tools",
        fantastic_terminal_webapp::TerminalWebappBundle,
    );
    reg.register(
        "ollama_backend.tools",
        fantastic_ollama_backend::OllamaBackendBundle,
    );
    reg.register(
        "kernel_bridge.tools",
        fantastic_kernel_bridge::KernelBridgeBundle,
    );
    reg.register(
        "nvidia_nim_backend.tools",
        fantastic_nvidia_nim_backend::NvidiaNimBundle,
    );

    // ── Full-tier-only (subprocess / fork / dynamic loading).
    //
    // None ported yet. Sandboxed iOS builds must NOT see these. When
    // adding one, gate both the dep in Cargo.toml AND the register
    // call here on `feature = "full"`.
    #[cfg(feature = "full")]
    {
        // Desktop / unsandboxed bundles. PTY + subprocess + dynamic
        // loading; iOS sandbox forbids these so they're feature-gated.
        // Pro Mac links the `full` XCFramework and gets the registrations
        // below; Lite (any platform) compiles without these crates at all.
        reg.register(
            "terminal_backend.tools",
            fantastic_terminal_backend::TerminalBackendBundle,
        );
        reg.register(
            "local_runner.tools",
            fantastic_local_runner::LocalRunnerBundle,
        );
        reg.register(
            "python_runtime.tools",
            fantastic_python_runtime::PythonRuntimeBundle,
        );
        reg.register("ssh_runner.tools", fantastic_ssh_runner::SshRunnerBundle);
    }

    reg
}

/// Public bootstrap function. Constructs a kernel, hydrates persisted
/// agents (weak-load skip+log for unknown bundles), spins up the
/// axum listener via the web bundle's `boot` verb.
///
/// `async_runtime = "tokio"` is critical — without it, UniFFI's
/// Rust-side scaffolding polls this future on its default executor
/// which has no Tokio reactor. The first internal `tokio::spawn` /
/// `tokio::net::TcpListener::bind` / `axum::serve` then panics with
/// "there is no reactor running, must be called from the context of
/// a Tokio 1.x runtime". The proc-macro form is the only way to set
/// `async_runtime` per-function — uniffi 0.29's UDL grammar doesn't
/// support `[Async=tokio]`, and the `uniffi.toml [bindings.swift]
/// async_runtime` knob only configures the FOREIGN-language bindgen,
/// not the Rust scaffolding poll loop.
///
/// Docs: https://mozilla.github.io/uniffi-rs/latest/futures.html
#[uniffi::export(async_runtime = "tokio")]
pub async fn start_kernel(workdir: String, port_hint: u16) -> Result<Arc<Kernel>, KernelError> {
    let workdir_path = PathBuf::from(&workdir);
    if !workdir_path.exists() {
        std::fs::create_dir_all(&workdir_path)
            .map_err(|e| KernelError::WorkdirInvalid(format!("create {}: {e}", workdir)))?;
    }
    let opts = BootstrapOptions::daemon(&workdir_path);
    let booted = bootstrap::bootstrap(register_default_bundles(), opts)
        .map_err(|e| KernelError::BootFailed(format!("{e}")))?;
    let kernel_arc = Arc::clone(&booted.kernel);

    // If the workdir already has a web agent, boot it (the listener
    // spawns inside the bundle). Otherwise create + boot a default
    // web agent on `port_hint` so the embedding app has SOMETHING to
    // point WKWebView at.
    let web_id = ensure_web_agent(&kernel_arc, port_hint).await?;
    // boot fires the listener.
    let boot_reply = kernel_arc.send(&web_id, json!({"type": "boot"})).await;
    if let Some(err) = boot_reply.get("error").and_then(Value::as_str) {
        return Err(KernelError::PortBindFailed(err.to_string()));
    }
    let actual_port = boot_reply
        .get("port")
        .and_then(Value::as_u64)
        .map(|p| p as u16)
        .unwrap_or(port_hint);

    // Also seed core's readme so callers that reflect with
    // return_readme=true on the root get useful output.
    let _ = fantastic_core::seed_root_readme(&workdir_path);

    Ok(Arc::new(Kernel::new_inner(
        booted.kernel,
        workdir_path,
        web_id,
        actual_port,
    )))
}

async fn ensure_web_agent(
    kernel: &Arc<fantastic_kernel::Kernel>,
    port_hint: u16,
) -> Result<AgentId, KernelError> {
    // Reuse an existing web.tools agent if any.
    if let Some(existing) = kernel
        .agents
        .iter()
        .find(|e| e.value().handler_module.as_deref() == Some("web.tools"))
    {
        return Ok(existing.key().clone());
    }
    // Otherwise create a new one under core with the requested port.
    let reply = kernel
        .send(
            &AgentId::from("core"),
            json!({
                "type": "create_agent",
                "handler_module": "web.tools",
                "id": "web",
                "port": port_hint,
            }),
        )
        .await;
    if let Some(err) = reply.get("error").and_then(Value::as_str) {
        return Err(KernelError::BootFailed(format!("create web agent: {err}")));
    }
    Ok(AgentId::from("web"))
}

/// Swift-facing handle. Holds the workdir + bound port + the kernel
/// Arc so handler closures (send_json) can talk to substrate.
pub struct Kernel {
    inner: Arc<fantastic_kernel::Kernel>,
    workdir: PathBuf,
    web_id: AgentId,
    port: u16,
    /// `true` after `shutdown()` so a second call is a no-op.
    stopped: Mutex<bool>,
}

/// Private impl block — methods NOT exported to Swift. The exported
/// surface lives in the two `#[uniffi::export]` blocks below.
impl Kernel {
    /// Wrap a freshly-booted substrate. Used by [`start_kernel`].
    fn new_inner(
        inner: Arc<fantastic_kernel::Kernel>,
        workdir: PathBuf,
        web_id: AgentId,
        port: u16,
    ) -> Self {
        Self {
            inner,
            workdir,
            web_id,
            port,
            stopped: Mutex::new(false),
        }
    }
}

/// Sync methods exposed to Swift. Declared via proc-macro so the
/// UDL stays type-only — see the matching `interface Kernel` block
/// in `fantastic.udl` which only declares the type, not its methods.
#[uniffi::export]
impl Kernel {
    /// The bound port. Calling this before [`start_kernel`] finishes
    /// is a bug; the async bootstrap guarantees the port is known by
    /// the time the future resolves.
    pub fn http_port(&self) -> u16 {
        self.port
    }

    /// Register a [`StateListener`] for the kernel's state stream.
    /// Returns an opaque token consumers pass to [`Self::unsubscribe`]
    /// to detach. The closure runs inline on the dispatching task —
    /// keep it short.
    pub fn subscribe(&self, listener: Box<dyn StateListener>) -> u64 {
        let listener: Arc<dyn StateListener> = Arc::from(listener);
        let cb: fantastic_kernel::StateSubscriber = Arc::new(move |event: &Value| {
            let s = serde_json::to_string(event).unwrap_or_else(|_| "null".to_string());
            listener.on_event(s);
        });
        let token = self.inner.add_state_subscriber(cb);
        token.0
    }

    /// Detach a listener previously registered via [`Self::subscribe`].
    /// No-op if `token` isn't (or no longer is) registered.
    pub fn unsubscribe(&self, token: u64) {
        self.inner
            .remove_state_subscriber(fantastic_kernel::kernel::SubscriberToken(token));
    }

    /// Stop the listener, release the workdir lock. Idempotent.
    pub fn shutdown(&self) {
        let mut g = self.stopped.lock().expect("stopped poisoned");
        if *g {
            return;
        }
        *g = true;
        // Block briefly on the web bundle's `shutdown` verb. We're in
        // a sync method (UniFFI doesn't async-bridge `shutdown`); use
        // a one-shot runtime for the await.
        let inner = Arc::clone(&self.inner);
        let web_id = self.web_id.clone();
        std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                rt.block_on(async {
                    let _ = inner.send(&web_id, json!({"type": "shutdown"})).await;
                });
            }
        })
        .join()
        .ok();
        // Release the workdir lock.
        let _ = bootstrap::shutdown(&self.workdir);
    }
}

/// Async method — needs its OWN proc-macro impl block with
/// `async_runtime = "tokio"` so UniFFI wraps the Rust-side
/// scaffolding's `.poll()` in a Tokio runtime context. Without this
/// the future panics on its first internal `tokio::spawn` /
/// `tokio::mpsc` op (kernel.send awaits both). uniffi 0.29 doesn't
/// support per-method runtime tagging inside a shared impl block,
/// so the async fn lives here alone.
#[uniffi::export(async_runtime = "tokio")]
impl Kernel {
    /// JSON-in, JSON-out shortcut. Equivalent to the WS `call` frame.
    pub async fn send_json(&self, target_id: String, payload_json: String) -> String {
        let payload: Value = serde_json::from_str(&payload_json)
            .unwrap_or_else(|_| json!({"error": "send_json: payload not valid JSON"}));
        let reply = self
            .inner
            .send(&AgentId::from(target_id.as_str()), payload)
            .await;
        serde_json::to_string(&reply).unwrap_or_else(|_| "null".to_string())
    }
}

impl Drop for Kernel {
    fn drop(&mut self) {
        self.shutdown();
    }
}

uniffi::include_scaffolding!("fantastic");

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counting {
        count: Arc<AtomicUsize>,
    }
    impl StateListener for Counting {
        fn on_event(&self, _event_json: String) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn subscribe_callback_fires_for_each_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kernel = start_kernel(tmp.path().to_string_lossy().to_string(), 0)
            .await
            .expect("boot");
        let count = Arc::new(AtomicUsize::new(0));
        let listener = Counting {
            count: Arc::clone(&count),
        };
        let token = kernel.subscribe(Box::new(listener));

        // Trigger a few state events. Every send publishes one;
        // create_agent also publishes a "created" event on its own.
        let _ = kernel
            .send_json(
                "core".into(),
                r#"{"type":"create_agent","handler_module":"file.tools","id":"t1","root":"/tmp"}"#
                    .into(),
            )
            .await;
        let _ = kernel
            .send_json("t1".into(), r#"{"type":"reflect"}"#.into())
            .await;
        let _ = kernel
            .send_json("core".into(), r#"{"type":"delete_agent","id":"t1"}"#.into())
            .await;

        // Each send fires at least one state event; some fire two
        // (created + send, removed + send, etc.). Assert lower bound.
        let after = count.load(Ordering::SeqCst);
        assert!(
            after >= 3,
            "expected >=3 state events fired through listener, got {after}",
        );

        // Detach + confirm the counter stops climbing.
        kernel.unsubscribe(token);
        let frozen = count.load(Ordering::SeqCst);
        let _ = kernel
            .send_json("kernel".into(), r#"{"type":"reflect"}"#.into())
            .await;
        assert_eq!(
            count.load(Ordering::SeqCst),
            frozen,
            "events still firing after unsubscribe",
        );

        kernel.shutdown();
    }

    #[tokio::test]
    async fn unsubscribe_unknown_token_is_noop() {
        let tmp = tempfile::TempDir::new().unwrap();
        let kernel = start_kernel(tmp.path().to_string_lossy().to_string(), 0)
            .await
            .expect("boot");
        kernel.unsubscribe(99_999_999);
        kernel.shutdown();
    }
}
