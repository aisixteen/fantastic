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

/// Identity line for the PTY intro: `rust · env=<…> · v<…>? · root=<…> · pid <…>`
/// — the same deployment context the root reflect carries, rendered for a tty.
fn identity(kernel: &Kernel) -> String {
    let env = std::env::var("FANTASTIC_ENV").unwrap_or_else(|_| "host".to_string());
    let root = kernel
        .root()
        .map(|r| r.id.as_str().to_string())
        .unwrap_or_else(|| "core".to_string());
    let mut parts = vec!["rust".to_string(), format!("env={env}")];
    if let Ok(ver) = std::env::var("FANTASTIC_VERSION") {
        if !ver.is_empty() {
            parts.push(ver);
        }
    }
    parts.push(format!("root={root}"));
    parts.push(format!("pid {}", std::process::id()));
    parts.join(" · ")
}

/// First PTY push (printed BEFORE boot): identity + the pull/push control-plane
/// map. Port-independent, so it prints instantly. The binary prints this only
/// when stdin is a tty; the text lives here in the bundle.
pub fn intro_booting(kernel: &Kernel) -> String {
    let mut s = format!("[fantastic] {} — booting…\n", identity(kernel));
    s.push_str("  one envelope: send(<id>, {\"type\":\"<verb>\", …})   ·   kernel = root   ·   full map: reflect readme=true\n");
    s.push_str(
        "  PULL  ask → reply        REST POST /<rest>/<id>        ·  this REPL: @<id> <verb> k=v\n",
    );
    s.push_str("  PUSH  async stream/emit  WS /<id>/ws : watch{src} · emit{target,payload} · state_subscribe\n");
    s.push_str("  REACH one call by id, any unit: compute(python_runtime) · infer(ai) · memory(yaml_state) · shell(terminal_backend)");
    s
}

/// Final PTY push (printed AFTER the boot loop): the kernel's "all booted"
/// close. The renderer is a DUMB SINK — it does NOT inspect the tree for
/// ports/surfaces. Each agent announces its OWN endpoints during its boot
/// (e.g. `web` publishes a `say` state event with its listening URL, rendered
/// by `format_event` below). Best-effort: a race / no renderer is fine — the
/// full map is in the intro + `reflect readme=true`.
pub fn booted() -> String {
    "[kernel] up — all booted. attach via the map above, or reflect readme=true".to_string()
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
        // An agent announcing itself to the terminal (the boot-event convention,
        // e.g. web's listening URL). The producer owns the text; we just render.
        "say" => {
            let src = event.get("source").and_then(Value::as_str).unwrap_or("");
            let text = event.get("text").and_then(Value::as_str).unwrap_or("");
            if src.is_empty() {
                format!("  {text}")
            } else {
                format!("  [{src}] {text}")
            }
        }
        _ => format!("{ty}  {}", event),
    }
}

#[cfg(test)]
mod tests;
