//! [`RunnerCore`] — the shared lifecycle verb dispatcher.
//!
//! This is the deduplicated body of both runner bundles' `handle`. It
//! takes a fully-built [`Transport`] (constructed per call by the
//! runner bundle from the agent record) and routes the lifecycle verb
//! to it:
//!
//! - `reflect` / `start` / `stop` / `status` / `get_webapp` → the
//!   transport's matching method (which owns the concrete reply).
//! - `boot` → `Value::Null` (no auto-start; `start` is explicit).
//! - `shutdown` → alias of `stop`.
//! - `restart` → `stop` then `start` (the stop reply is discarded,
//!   matching both runners' prior behaviour).
//! - anything else → `{"error": "<name>: unknown type <verb>"}`.
//!
//! No wire/verb/event behaviour changes here vs. the pre-refactor
//! per-runner `handle`.

use crate::transport::Transport;
use serde_json::{json, Value};

/// Stateless dispatcher over a [`Transport`].
pub struct RunnerCore;

impl RunnerCore {
    /// Dispatch one lifecycle verb through `transport`.
    ///
    /// `name` is the bundle's short name (`"local_runner"` /
    /// `"ssh_runner"`) used only in the unknown-verb error string, so
    /// the message stays byte-identical to the pre-refactor reply.
    pub async fn handle_via(transport: &dyn Transport, name: &str, verb: &str) -> Value {
        match verb {
            "reflect" => transport.reflect().await,
            "boot" => Value::Null,
            "start" => transport.start().await,
            "stop" | "shutdown" => transport.stop().await,
            "restart" => {
                let _ = transport.stop().await;
                transport.start().await
            }
            "status" => transport.status().await,
            "get_webapp" => transport.get_webapp().await,
            other => json!({ "error": format!("{name}: unknown type {other:?}") }),
        }
    }
}
