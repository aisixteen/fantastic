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
//! Three properties worth stating, because all three are deliberate:
//!
//! - **Unknown verbs are DROPPED, not refused.** A render surface that answered
//!   `unknown verb` would turn a cosmetic gap into a failed send for whoever was
//!   streaming to it.
//! - **It never inspects the tree.** Producers push what they know; this sink
//!   only formats. The kernel stays decoupled — it sends verbs, it never
//!   imports this.
//! - **Python's raise paths are raises here too.** A non-string `token` text or
//!   a truthy non-dict `detail`/`tool` dies in python's `stdout.write` /
//!   `.get`, and the caller reads `{"error": …, "raised": true}`. Rendering
//!   something plausible instead would hide a producer's bug.

use async_trait::async_trait;
use fantastic_bundle::{py_json_dumps, py_str, py_truthy, py_type_name};
use fantastic_kernel::bootstrap::DEFAULT_ROOT_ID;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::io::Write;
use std::sync::Arc;

/// The handler module id a record names to select this bundle.
pub const HANDLER_MODULE: &str = "cli.tools";

pub const README: &str = include_str!("readme.md");

/// The CLI cheatsheet `fantastic --help` prints — python's `cli/help.md`, byte
/// for byte (A14.1). It lives in THIS bundle because the CLI is what renders it:
/// file-backed, editable markdown, not a hardcoded string. The binary prints it
/// `rstrip`ped plus one `\n`, exactly as python's `print(src.read_text().rstrip())`.
pub const HELP: &str = include_str!("help.md");

/// The events this sink renders. Anything else is dropped.
pub const ACCEPTS: [&str; 6] = ["token", "done", "say", "error", "status", "context"];

/// python's `VERBS`, in order, each doc the first line of the python handler's
/// docstring — that is exactly what python's `_reflect` builds
/// (`(f.__doc__ or "").strip().splitlines()[0]`), cut mid-sentence and all.
pub const VERBS: [(&str, &str); 9] = [
    ("reflect", "Identity + accepted event types. No args."),
    (
        "token",
        "args: text:str. Writes to stdout (no newline). Returns None.",
    ),
    (
        "done",
        "args: none. Emits a single newline; closes a token-stream cleanly. Returns None.",
    ),
    (
        "say",
        "args: text:str, source:str?. Prints `  [source] text` on its own line. Returns None.",
    ),
    (
        "error",
        "args: text:str. Prints `  ERROR: text` on its own line. Returns None.",
    ),
    (
        "status",
        "args: phase:str, source:str?, detail:dict?. Renders one-line phase markers for queued / thinking / tool_calling (entry+exit). `streaming` and `done` produce no output (token + done handlers cover them). Returns None.",
    ),
    (
        "context",
        "args: phase:str ('compacted'|'too_small'), source:str?, detail:dict?. Renders a one-line context-protocol notice.",
    ),
    (
        "intro_booting",
        "First PTY push, BEFORE boot. Identity + the pull/push control-plane map — port-independent, so it prints instantly. No args. Returns None.",
    ),
    (
        "booted",
        "Final PTY push — the kernel's \"all booted\" signal, sent AFTER the boot loop.",
    ),
];

/// python's raised reply: the kernel renders `f"{type(e).__name__}: {e}"`.
fn raised(class: &str, message: impl std::fmt::Display) -> BundleError {
    format!("{class}: {message}").into()
}

/// python `<x>.get(key)` where `<x>` is whatever the payload carried. A truthy
/// non-dict has no `.get`, so python dies with an `AttributeError`.
fn py_get<'a>(v: &'a Value, key: &str) -> Result<Option<&'a Value>, BundleError> {
    match v {
        Value::Object(m) => Ok(m.get(key)),
        other => Err(raised(
            "AttributeError",
            format!("'{}' object has no attribute 'get'", py_type_name(other)),
        )),
    }
}

/// python `payload.get(key) or {}` — a falsy value (absent, `null`, `{}`, `0`,
/// `""`) becomes an empty dict; anything else is carried through as it is, so
/// the raise lands on the `.get` that follows.
fn or_empty_dict(v: Option<&Value>) -> Value {
    match v {
        Some(x) if py_truthy(x) => x.clone(),
        _ => json!({}),
    }
}

/// python `payload.get(key, "")` — the default is keyed on PRESENCE, so a
/// present `null` stays `None` and renders as `None`.
fn get_or_blank(payload: &Value, key: &str) -> Value {
    payload.get(key).cloned().unwrap_or_else(|| json!(""))
}

/// `  [source] ` when a source is truthy, else two spaces — python's `say`.
fn say_prefix(src: &Value) -> String {
    if py_truthy(src) {
        format!("  [{}] ", py_str(src))
    } else {
        "  ".to_string()
    }
}

/// `  [source]` / ` ` — the status/context variant, which puts its own space
/// after the prefix. Python's two differ by exactly that trailing space.
fn line_prefix(src: &Value) -> String {
    if py_truthy(src) {
        format!("  [{}]", py_str(src))
    } else {
        " ".to_string()
    }
}

/// python's `str()` of `payload.get(key, default)`, the default keyed on
/// presence.
fn str_or(v: Option<&Value>, default: &str) -> String {
    v.map_or_else(|| default.to_string(), py_str)
}

/// `sys.stdout.write(x)` on a non-`str`. Note the type name: the clinic
/// converter says `None`, not `NoneType`.
fn write_type_error(v: &Value) -> BundleError {
    let name = if v.is_null() { "None" } else { py_type_name(v) };
    raised(
        "TypeError",
        format!("write() argument must be str, not {name}"),
    )
}

/// python `_status` — one line, or `None` for the silent phases.
fn render_status(payload: &Value) -> Result<Option<String>, BundleError> {
    let phase = payload.get("phase").and_then(Value::as_str).unwrap_or("");
    let detail = or_empty_dict(payload.get("detail"));
    let prefix = line_prefix(&get_or_blank(payload, "source"));
    let line = match phase {
        "queued" => {
            let ahead = str_or(py_get(&detail, "ahead")?, "0");
            format!("{prefix} queued ({ahead} ahead)")
        }
        "thinking" => format!("{prefix} thinking…"),
        "tool_calling" => {
            let tool = or_empty_dict(py_get(&detail, "tool")?);
            let verb = str_or(py_get(&tool, "verb")?, "");
            let target = str_or(py_get(&tool, "target")?, "");
            if let Some(preview) = py_get(&tool, "reply_preview")? {
                // exit — decided by KEY PRESENCE, so a null preview still exits
                format!("{prefix} ← {verb}({target})  {}", py_str(preview))
            } else {
                // entry — args summary on the line, python's `json.dumps`
                let args = or_empty_dict(py_get(&tool, "args")?);
                format!(
                    "{prefix} → {verb}({target})  {}",
                    py_json_dumps(&args, true, None)
                )
            }
        }
        // `streaming` and `done` are SILENT here: the token + done verbs
        // already cover them, and printing twice would double the stream.
        _ => return Ok(None),
    };
    Ok(Some(line))
}

/// python `_context` — one line, or `None` for an unknown phase.
fn render_context(payload: &Value) -> Result<Option<String>, BundleError> {
    let phase = payload.get("phase").and_then(Value::as_str).unwrap_or("");
    let detail = or_empty_dict(payload.get("detail"));
    let prefix = line_prefix(&get_or_blank(payload, "source"));
    let line = match phase {
        "compacted" => {
            let strat = str_or(py_get(&detail, "strategy")?, "?");
            let dropped = str_or(py_get(&detail, "dropped_messages")?, "0");
            format!(
                "{prefix} ⟲ context compacted ({strat}, {dropped} dropped; recall to page back)"
            )
        }
        "too_small" => {
            let hint = str_or(py_get(&detail, "hint")?, "");
            format!("{prefix} ✗ context too small — {hint}")
        }
        _ => return Ok(None),
    };
    Ok(Some(line))
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
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match verb {
            "reflect" => {
                let mut verbs = Map::new();
                for (name, doc) in VERBS {
                    verbs.insert(name.to_string(), json!(doc));
                }
                return Ok(Some(json!({
                    "id": agent_id.as_str(),
                    "sentence": "Terminal renderer. Prints incoming events to stdout.",
                    "verbs": verbs,
                    "accepts": ACCEPTS,
                })));
            }
            "token" => {
                let text = match payload.get("text") {
                    None => "",
                    Some(Value::String(s)) => s.as_str(),
                    Some(other) => return Err(write_type_error(other)),
                };
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            "done" => println!(),
            "say" => println!(
                "{}{}",
                say_prefix(&get_or_blank(payload, "source")),
                py_str(&get_or_blank(payload, "text"))
            ),
            "error" => println!("  ERROR: {}", py_str(&get_or_blank(payload, "text"))),
            "status" => {
                if let Some(line) = render_status(payload)? {
                    println!("{line}");
                }
            }
            "context" => {
                if let Some(line) = render_context(payload)? {
                    println!("{line}");
                }
            }
            "intro_booting" => {
                for line in intro_booting_lines(kernel) {
                    println!("{line}");
                }
            }
            "booted" => println!("{BOOTED_LINE}"),
            // A sink drops what it does not render — see the module note.
            // `boot` and `shutdown` land here too, exactly as on python.
            _ => {}
        }
        Ok(Some(Value::Null))
    }
}

#[cfg(test)]
mod tests;
