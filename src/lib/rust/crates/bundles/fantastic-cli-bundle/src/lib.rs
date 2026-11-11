//! `cli` singleton — terminal render sink.
//!
//! Mirrors python's `bundled_agents/cli` (canonical; on disagreement this file
//! is wrong). Receives `token` / `done` / `say` / `error` / `status` /
//! `context` and prints them to stdout, and renders the two-phase PTY intro
//! (`intro_booting` / `booted`).
//!
//! Rust had no such bundle: `fantastic-cli` is the HOST BINARY, not a render
//! agent. That was a real gap rather than a naming difference — the AI backends
//! stream their reply to an agent called `cli`, so on rust that stream had
//! nowhere to land.
//!
//! Two properties worth stating, because both are deliberate:
//!
//! - **It never inspects the tree.** Producers push what they know; this sink
//!   only formats. The kernel stays decoupled — it sends verbs, it never
//!   imports this.
//! - **A call that is not one of its declared verbs is refused by its own
//!   entry**, like any agent's, by name: a producer's mistake is told to the
//!   producer.

use async_trait::async_trait;
use fantastic_bundle::{py_json_dumps, py_str};
use fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use schemars::JsonSchema;
use serde::Deserialize;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::sync::Arc;

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "cli.tools";

/// The CLI cheatsheet `fantastic --help` prints — python's `cli/help.md`, byte
/// for byte. It lives in THIS bundle because the CLI is what renders it:
/// file-backed, editable markdown, not a hardcoded string. The binary prints it
/// `rstrip`ped plus one `\n`, exactly as python's `print(src.read_text().rstrip())`.
pub const HELP: &str = include_str!("help.md");

/// The events this sink renders. Anything else is dropped.
pub const ACCEPTS: [&str; 6] = ["token", "done", "say", "error", "status", "context"];

// ─── the verbs ───────────────────────────────────────────────────────

/// Writes `text` to stdout (no newline).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Token {
    /// What to write.
    pub text: String,
    /// Who produced it.
    pub source: Option<String>,
    /// Whose conversation it belongs to.
    pub client_id: Option<String>,
}

/// Emits a single newline; closes a token-stream cleanly.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Done {
    /// Who produced it.
    pub source: Option<String>,
    /// Whose conversation it belongs to.
    pub client_id: Option<String>,
}

/// Prints `  [source] text` on its own line.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Say {
    /// The line.
    pub text: String,
    /// Who says it.
    pub source: Option<String>,
    /// Whose conversation it belongs to.
    pub client_id: Option<String>,
}

/// Prints `  ERROR: text` on its own line.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ErrorLine {
    /// The error.
    pub text: String,
    /// Who reports it.
    pub source: Option<String>,
    /// Whose conversation it belongs to.
    pub client_id: Option<String>,
}

/// Renders one-line phase markers for queued / thinking / tool_calling (entry and exit); `streaming` and `done` produce no output (token and done cover them).
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Status {
    /// The phase.
    pub phase: String,
    /// Who is in the phase.
    pub source: Option<String>,
    /// Whose conversation it belongs to.
    pub client_id: Option<String>,
    /// When.
    pub ts: Option<f64>,
    /// What the phase carries: `ahead` of a queued one, the `tool` of a tool_calling one.
    pub detail: Option<Map<String, Value>>,
}

/// Renders a one-line context-protocol notice for the phase `compacted` or `too_small`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct Context {
    /// `compacted` or `too_small`.
    pub phase: String,
    /// Who it concerns.
    pub source: Option<String>,
    /// Whose conversation it belongs to.
    pub client_id: Option<String>,
    /// When.
    pub ts: Option<f64>,
    /// What the notice carries.
    pub detail: Option<Map<String, Value>>,
}

/// First PTY push, BEFORE boot: identity and the pull/push control-plane map — port-independent, so it prints instantly.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntroBooting {}

/// Final PTY push — the kernel's "all booted" signal, sent AFTER the boot loop.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Booted {}

/// The fields a cli agent's record carries: none of its own.
#[derive(Debug, JsonSchema)]
pub struct CliRecord {}

fantastic_kernel::record!(CliRecord);

mod declared {
    use super::{Booted, Context, Done, ErrorLine, IntroBooting, Say, Status, Token};

    fantastic_kernel::verbs! {
        "token" => Token,
        "done" => Done,
        "say" => Say,
        "error" => ErrorLine,
        "status" => Status,
        "context" => Context,
        "intro_booting" => IntroBooting,
        "booted" => Booted,
    }
}

/// What the bundle is for — its one short man text.
const MAN: &str = "The terminal renderer: it prints the events an agent streams to it (tokens, lines, phase markers, context notices) to stdout, and the two-phase intro of a PTY.";

/// `  [source] ` when a source is named, else two spaces — python's `say`.
fn say_prefix(src: Option<&str>) -> String {
    match src.filter(|s| !s.is_empty()) {
        Some(s) => format!("  [{s}] "),
        None => "  ".to_string(),
    }
}

/// `  [source]` / ` ` — the status/context variant, which puts its own space
/// after the prefix. Python's two differ by exactly that trailing space.
fn line_prefix(src: Option<&str>) -> String {
    match src.filter(|s| !s.is_empty()) {
        Some(s) => format!("  [{s}]"),
        None => " ".to_string(),
    }
}

/// The text of one key of a detail map, `default` when absent.
fn text_or(map: &Map<String, Value>, key: &str, default: &str) -> String {
    map.get(key).map_or_else(|| default.to_string(), py_str)
}

/// `text` without its C0/C1 control characters (and DEL), newline and tab kept —
/// python's `_CONTROL`: what a terminal INTERPRETS instead of printing (an OSC
/// title or clipboard write, a cursor move, a line erase). Agent and model text
/// reaches this sink from callers it does not know.
fn terminal_safe(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

/// THE one write to the terminal — python `_write`: every line this sink prints
/// goes through [`terminal_safe`].
fn write(text: &str) {
    print!("{}", terminal_safe(text));
}

/// `_status` — one line, or `None` for the silent phases.
fn render_status(args: &Status) -> Option<String> {
    let detail = args.detail.clone().unwrap_or_default();
    let prefix = line_prefix(args.source.as_deref());
    let line = match args.phase.as_str() {
        "queued" => {
            let ahead = text_or(&detail, "ahead", "0");
            format!("{prefix} queued ({ahead} ahead)")
        }
        "thinking" => format!("{prefix} thinking…"),
        "tool_calling" => {
            let tool = detail
                .get("tool")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let verb = text_or(&tool, "verb", "");
            let target = text_or(&tool, "target", "");
            if let Some(preview) = tool.get("reply_preview") {
                // exit — decided by KEY PRESENCE, so a null preview still exits
                format!("{prefix} ← {verb}({target})  {}", py_str(preview))
            } else {
                // entry — args summary on the line, python's `json.dumps`
                let args = tool
                    .get("args")
                    .filter(|v| !v.is_null())
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                format!(
                    "{prefix} → {verb}({target})  {}",
                    py_json_dumps(&args, true, None)
                )
            }
        }
        // `streaming` and `done` are SILENT here: the token + done verbs
        // already cover them, and printing twice would double the stream.
        _ => return None,
    };
    Some(line)
}

/// `_context` — one line, or `None` for an unknown phase.
fn render_context(args: &Context) -> Option<String> {
    let detail = args.detail.clone().unwrap_or_default();
    let prefix = line_prefix(args.source.as_deref());
    let line = match args.phase.as_str() {
        "compacted" => {
            let strat = text_or(&detail, "strategy", "?");
            let dropped = text_or(&detail, "dropped_messages", "0");
            format!(
                "{prefix} ⟲ context compacted ({strat}, {dropped} dropped; recall to page back)"
            )
        }
        "too_small" => {
            let hint = text_or(&detail, "hint", "");
            format!("{prefix} ✗ context too small — {hint}")
        }
        _ => return None,
    };
    Some(line)
}

// ─── two-phase PTY intro (first contact) ────────────────────────

/// `rust · env=<…> · v<…>? · root=<…> · pid <…>` — the same deployment context
/// the root reflect carries, rendered for the terminal.
fn identity(kernel: &Kernel) -> String {
    let env = std::env::var("FANTASTIC_ENV").unwrap_or_else(|_| "host".to_string());
    let ver = std::env::var("FANTASTIC_VERSION")
        .ok()
        .filter(|v| !v.is_empty());
    let root = kernel.root().map_or_else(
        || DEFAULT_ROOT_ID.to_string(),
        |r| r.id.as_str().to_string(),
    );
    let mut parts = vec!["rust".to_string(), format!("env={env}")];
    if let Some(ver) = ver {
        parts.push(ver);
    }
    parts.push(format!("root={root}"));
    parts.push(format!("pid {}", std::process::id()));
    parts.join(" · ")
}

/// The five lines python's `_intro_booting` prints.
pub fn intro_booting_lines(kernel: &Kernel) -> Vec<String> {
    vec![
        format!("[fantastic] {} — booting…", identity(kernel)),
        concat!(
            "  one envelope: send(<id>, {\"type\":\"<verb>\", …})",
            "   ·   kernel = root   ·   full map: reflect readme=true"
        )
        .to_string(),
        "  PULL  ask → reply        REST POST /<rest>/<id>        ·  this REPL: @<id> <verb> k=v"
            .to_string(),
        "  PUSH  async stream/emit  WS /<id>/ws : watch{src} · emit{target,payload} · state_subscribe"
            .to_string(),
        concat!(
            "  REACH one call by id, any unit: ",
            "compute(exec_bridge) · infer(ai) · memory(file_bridge) · shell(exec_bridge pty)"
        )
        .to_string(),
    ]
}

/// python's `_booted` line — the "all booted" close.
pub const BOOTED_LINE: &str =
    "[kernel] up — all booted. attach via the map above, or reflect readme=true";

/// The cli render-sink bundle.
pub struct CliBundle;

#[async_trait]
impl Handler for CliBundle {
    type Call = declared::Call;

    fn name(&self) -> &str {
        "cli"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        declared::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        declared::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        use declared::Call;
        match call {
            Call::Reflect(_) => {
                return Ok(Some(json!({
                    "id": agent_id.as_str(),
                    "sentence": "Terminal renderer. Prints incoming events to stdout.",
                    "accepts": ACCEPTS,
                })));
            }
            Call::Token(a) => {
                write(&a.text);
                let _ = std::io::stdout().flush();
            }
            Call::Done(_) => write("\n"),
            Call::Say(a) => write(&format!("{}{}\n", say_prefix(a.source.as_deref()), a.text)),
            Call::ErrorLine(a) => write(&format!("  ERROR: {}\n", a.text)),
            Call::Status(a) => {
                if let Some(line) = render_status(&a) {
                    write(&format!("{line}\n"));
                }
            }
            Call::Context(a) => {
                if let Some(line) = render_context(&a) {
                    write(&format!("{line}\n"));
                }
            }
            Call::IntroBooting(_) => {
                for line in intro_booting_lines(kernel) {
                    write(&format!("{line}\n"));
                }
            }
            Call::Booted(_) => write(&format!("{BOOTED_LINE}\n")),
        }
        Ok(Some(Value::Null))
    }
}

#[cfg(test)]
mod tests;
