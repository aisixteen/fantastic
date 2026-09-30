//! `ui` — THE VIEWMODEL'S FACE IN THE KERNEL (`ui.tools`).
//!
//! A View outside this process (the browser, through `web_ui`) reaches the one
//! ViewModel through this agent, gated like any call:
//!
//! - `key {code, ch?, ctrl?, alt?, shift?}` · `paste {text}` · `click {x, y}` · `resize {w, h}`
//!   are INTENTS; they go to the ViewModel exactly as the terminal's own do.
//! - `state` answers the newest frame WHOLE (`{type:"frame", full:true, …}`) —
//!   the resync after a `seq` gap.
//! - Every new frame is `emit`ted on this agent as changed rows (`kern/web.rs`);
//!   a View `watch`es `ui` to get them.
//!
//! In a process with no ViewModel (`fantastic mcp`) every intent is refused,
//! loudly, rather than dropped.

use std::sync::Arc;

use async_trait::async_trait;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

use crate::vm::VmIn;

pub const HANDLER_MODULE: &str = "ui.tools";
pub const UI_ID: &str = "ui";

#[derive(Default)]
pub struct UiBundle;

fn refuse(msg: impl Into<String>) -> Value {
    json!({"error": msg.into(), "reason": "unavailable"})
}

/// A browser key, as the page names it, to the terminal's own event.
pub fn key_event(p: &Value) -> Result<Event, String> {
    let name = p.get("code").and_then(Value::as_str).ok_or("key: `code` is required")?;
    let code = match name {
        "Char" => {
            let ch = p.get("ch").and_then(Value::as_str).and_then(|s| s.chars().next()).ok_or("key: Char needs `ch`")?;
            KeyCode::Char(ch)
        }
        "Enter" => KeyCode::Enter,
        "Esc" => KeyCode::Esc,
        "Backspace" => KeyCode::Backspace,
        "Tab" => KeyCode::Tab,
        "BackTab" => KeyCode::BackTab,
        "Up" => KeyCode::Up,
        "Down" => KeyCode::Down,
        "Left" => KeyCode::Left,
        "Right" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Delete" => KeyCode::Delete,
        f if f.starts_with('F') => KeyCode::F(f[1..].parse().map_err(|_| format!("key: bad code {f:?}"))?),
        other => return Err(format!("key: unknown code {other:?}")),
    };
    let flag = |k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(false);
    let mut m = KeyModifiers::NONE;
    if flag("ctrl") {
        m |= KeyModifiers::CONTROL;
    }
    if flag("alt") {
        m |= KeyModifiers::ALT;
    }
    if flag("shift") {
        m |= KeyModifiers::SHIFT;
    }
    Ok(Event::Key(KeyEvent::new(code, m)))
}

#[async_trait]
impl Bundle for UiBundle {
    fn name(&self) -> &str {
        "ui"
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, _kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(verb, "reflect" | "boot") {
            return Ok(Some(match verb {
                "boot" => json!({"booted": true}),
                _ => json!({
                    "id": agent_id.as_str(),
                    "sentence": "The ONE ViewModel's face: every View draws its frames; intents go in here.",
                    "live": crate::vm::handle().is_some(),
                    "sized_by": crate::vm::sized_by(),
                    // Which process holds the ViewModel: one per directory.
                    "pid": std::process::id(),
                    "verbs": {
                        "key": "args: code:str (Char|Enter|Esc|Backspace|Tab|BackTab|Up|Down|Left|Right|Home|End|PageUp|PageDown|Delete|F1..), ch:str? (for Char), ctrl/alt/shift:bool?",
                        "paste": "args: text:str",
                        "click": "args: x:int, y:int — a left click at that cell: focuses the panel under it",
                        "resize": "args: w:int, h:int — the ViewModel's size (last one wins)",
                        "state": "No args. The newest frame whole: {type:'frame', full:true, seq, w, h, rows, cursor}",
                    },
                    "emits": {"frame": "{type:'frame', seq, full, w, h, rows:{<y>:[{t, fg?, bg?, m?}]}, cursor} — changed rows since seq-1"},
                }),
            }));
        }
        let Some(vm) = crate::vm::handle() else {
            return Ok(Some(refuse("ui: no ViewModel runs in this process")));
        };
        let intent = match verb {
            "key" => match key_event(payload) {
                Ok(e) => VmIn::Event(e),
                Err(e) => return Ok(Some(refuse(e))),
            },
            "click" => {
                let n = |k: &str| payload.get(k).and_then(Value::as_u64).filter(|v| *v < 1000);
                match (n("x"), n("y")) {
                    (Some(x), Some(y)) => VmIn::Event(Event::Mouse(crossterm::event::MouseEvent {
                        kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
                        column: x as u16,
                        row: y as u16,
                        modifiers: KeyModifiers::NONE,
                    })),
                    _ => return Ok(Some(refuse("click: `x` and `y` (cells) are required"))),
                }
            }
            "paste" => match payload.get("text").and_then(Value::as_str) {
                Some(t) => VmIn::Event(Event::Paste(t.to_string())),
                None => return Ok(Some(refuse("paste: `text` is required"))),
            },
            "resize" => {
                let n = |k: &str| payload.get(k).and_then(Value::as_u64).filter(|v| (1..=1000).contains(v));
                match (n("w"), n("h")) {
                    (Some(w), Some(h)) => VmIn::Resize(w as u16, h as u16),
                    _ => return Ok(Some(refuse("resize: `w` and `h` are required, 1..1000"))),
                }
            }
            "state" => {
                return Ok(Some(match crate::kern::web::state() {
                    Some(f) => f,
                    None => refuse("state: no frame yet — the ViewModel has not been given a size"),
                }))
            }
            other => return Ok(Some(json!({"error": format!("ui: unknown type {other:?}"), "reason": "unhandled"}))),
        };
        Ok(Some(match vm.input.send(intent).await {
            Ok(()) => json!({"ok": true}),
            Err(_) => refuse("ui: the ViewModel has stopped"),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_key_becomes_the_terminals_own_event() {
        let e = key_event(&json!({"code":"Char","ch":"c","ctrl":true})).unwrap();
        assert_eq!(e, Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert_eq!(key_event(&json!({"code":"F1"})).unwrap(), Event::Key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE)));
        assert!(key_event(&json!({"code":"Nope"})).is_err());
        assert!(key_event(&json!({"code":"Char"})).is_err(), "Char needs its character");
    }
}
