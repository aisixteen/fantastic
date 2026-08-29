//! `cli` singleton — terminal render sink.
//!
//! Mirrors python's `bundled_agents/cli` and swift's `FantasticCliBundle`.
//! Receives `token` / `done` / `say` / `error` / `status` / `context` and
//! prints them to stdout.
//!
//! Rust had no such bundle: `fantastic-cli` is the HOST BINARY, not a render
//! agent. That was a real gap rather than a naming difference — the AI backends
//! stream their turn to an agent called `cli`, so on rust that stream had
//! nowhere to land.
//!
//! Two properties worth stating, because both are deliberate:
//!
//! - **Unknown verbs are DROPPED, not refused.** A render surface that answered
//!   `unknown verb` would turn a cosmetic gap into a failed send for whoever was
//!   streaming to it.
//! - **It never inspects the tree.** Producers push what they know; this sink
//!   only formats. The kernel stays decoupled — it sends verbs, it never
//!   imports this.

use async_trait::async_trait;
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::Arc;

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "cli.tools";

pub const README: &str = include_str!("readme.md");

/// The events this sink renders. Anything else is dropped.
pub const ACCEPTS: [&str; 6] = ["token", "done", "say", "error", "status", "context"];

fn text_of(payload: &Value) -> &str {
    payload.get("text").and_then(Value::as_str).unwrap_or("")
}

fn source_of(payload: &Value) -> &str {
    payload.get("source").and_then(Value::as_str).unwrap_or("")
}

/// `  [source] ` when a source is named, else two spaces — matching python.
fn say_prefix(src: &str) -> String {
    if src.is_empty() {
        "  ".to_string()
    } else {
        format!("  [{src}] ")
    }
}

/// `  [source]` / ` ` — the status/context variant, which puts its own space
/// after the prefix. Kept distinct from `say_prefix` because python's two
/// differ by exactly that trailing space.
fn line_prefix(src: &str) -> String {
    if src.is_empty() {
        " ".to_string()
    } else {
        format!("  [{src}]")
    }
}

fn render_status(payload: &Value) {
    let phase = payload.get("phase").and_then(Value::as_str).unwrap_or("");
    let detail = payload.get("detail").cloned().unwrap_or(Value::Null);
    let prefix = line_prefix(source_of(payload));
    match phase {
        "queued" => {
            let ahead = detail.get("ahead").and_then(Value::as_u64).unwrap_or(0);
            println!("{prefix} queued ({ahead} ahead)");
        }
        "thinking" => {
            if detail.get("waiting_on").and_then(Value::as_str) == Some("rate_limit") {
                let wait = detail
                    .get("wait_s")
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "?".into());
                println!("{prefix} rate-limited; waiting {wait}s");
            } else {
                println!("{prefix} thinking…");
            }
        }
        "tool_calling" => {
            let tool = detail.get("tool").cloned().unwrap_or(Value::Null);
            let verb = tool.get("verb").and_then(Value::as_str).unwrap_or("");
            let target = tool.get("target").and_then(Value::as_str).unwrap_or("");
            if let Some(preview) = tool.get("reply_preview").and_then(Value::as_str) {
                // exit
                println!("{prefix} ← {verb}({target})  {preview}");
            } else {
                // entry — args summary on the line
                let args = tool.get("args").cloned().unwrap_or(json!({}));
                println!("{prefix} → {verb}({target})  {args}");
            }
        }
        // `streaming` and `done` are SILENT here: the token + done verbs
        // already cover them, and printing twice would double the stream.
        _ => {}
    }
}

fn render_context(payload: &Value) {
    let phase = payload.get("phase").and_then(Value::as_str).unwrap_or("");
    let detail = payload.get("detail").cloned().unwrap_or(Value::Null);
    let prefix = line_prefix(source_of(payload));
    match phase {
        "compacted" => {
            let strat = detail
                .get("strategy")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let dropped = detail
                .get("dropped_turns")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            println!(
                "{prefix} ⟲ context compacted ({strat}, {dropped} dropped; recall to page back)"
            );
        }
        "too_small" => {
            let hint = detail.get("hint").and_then(Value::as_str).unwrap_or("");
            println!("{prefix} ✗ context too small — {hint}");
        }
        _ => {}
    }
}

/// The cli render-sink bundle.
pub struct CliBundle;

#[async_trait]
impl Bundle for CliBundle {
    fn name(&self) -> &str {
        "cli"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "reflect" => {
                return Ok(Some(json!({
                    "id": agent_id.as_str(),
                    "sentence": "Terminal renderer. Prints incoming events to stdout.",
                    "accepts": ACCEPTS,
                    "verbs": {
                        "reflect": "Identity + accepted event types. No args.",
                        "token": "args: text:str. Writes to stdout (no newline). Returns null.",
                        "done": "args: none. Emits a single newline; closes a token-stream cleanly.",
                        "say": "args: text:str, source:str?. Prints `  [source] text` on its own line.",
                        "error": "args: text:str. Prints `  ERROR: text` on its own line.",
                        "status": "args: phase:str, source:str?, detail:dict?. One-line phase markers for queued / thinking / tool_calling. `streaming` and `done` are silent.",
                        "context": "args: phase:str ('compacted'|'too_small'), source:str?, detail:dict?. One-line context-protocol notice.",
                    },
                })));
            }
            "boot" | "shutdown" => return Ok(Some(Value::Null)),
            "token" => {
                print!("{}", text_of(payload));
                let _ = std::io::stdout().flush();
            }
            "done" => println!(),
            "say" => println!("{}{}", say_prefix(source_of(payload)), text_of(payload)),
            "error" => println!("  ERROR: {}", text_of(payload)),
            "status" => render_status(payload),
            "context" => render_context(payload),
            // A sink drops what it does not render — see the module note.
            _ => {}
        }
        Ok(Some(Value::Null))
    }
}

#[cfg(test)]
mod tests;
