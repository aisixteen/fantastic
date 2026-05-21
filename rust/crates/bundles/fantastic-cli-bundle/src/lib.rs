//! Ephemeral stdout renderer — composed per-process when stdin is a
//! tty. Never persisted.
//!
//! Hooks into the kernel's state-subscriber API and prints a
//! one-line-per-event summary to stdout. Useful for `fantastic` REPL
//! mode and as a sanity surface during one-shot CLI debugging.

#![deny(missing_docs)]

use fantastic_kernel::{Kernel, SubscriberToken};
use serde_json::Value;
use std::io::{self, Write};
use std::sync::{Arc, Mutex};

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Attach the stdout renderer to a kernel's state stream.
///
/// Returns a [`SubscriberToken`] the caller can detach later. The
/// closure prints one terse line per event:
///
/// ```text
/// send  <sender> → <target>  <verb>   <summary>
/// emit  <sender> → <target>  <verb>   <summary>
/// updated  <id>
/// created  <id> (<handler_module>)
/// removed  <id>
/// ```
///
/// Output is line-buffered through `stdout().lock()` so concurrent
/// emissions interleave cleanly. Errors during write are swallowed
/// — a renderer should never panic the kernel.
pub fn attach(kernel: &Kernel) -> SubscriberToken {
    // Hold the stdout lock across each event for atomic line writes.
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let stdout = Arc::clone(&stdout);
    kernel.add_state_subscriber(Arc::new(move |event: &Value| {
        let line = format_event(event);
        if let Ok(mut out) = stdout.lock() {
            // Best-effort write — ignore EPIPE / closed-stdin / etc.
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }
    }))
}

/// Format one state event as a single human-readable line.
pub fn format_event(event: &Value) -> String {
    let ty = event.get("type").and_then(Value::as_str).unwrap_or("?");
    let target = event.get("target").and_then(Value::as_str).unwrap_or("");
    let sender = event.get("sender").and_then(Value::as_str).unwrap_or("");
    let verb = event.get("verb").and_then(Value::as_str).unwrap_or("");
    let summary = event.get("summary").and_then(Value::as_str).unwrap_or("");
    let id = event.get("id").and_then(Value::as_str).unwrap_or("");
    let hm = event
        .get("handler_module")
        .and_then(Value::as_str)
        .unwrap_or("");
    match ty {
        "send" | "emit" => {
            format!("{ty:<5} {sender} → {target}  {verb:<14}  {summary}")
        }
        "created" => {
            if hm.is_empty() {
                format!("created  {id}")
            } else {
                format!("created  {id} ({hm})")
            }
        }
        "removed" => format!("removed  {id}"),
        "updated" => format!("updated  {id}"),
        _ => format!("{ty}  {}", event),
    }
}

#[cfg(test)]
mod tests;
