//! The parsed shape ([`ToolCall`]) and the call-id minter.

use serde_json::{Map, Value};

/// One tool call a model named in text: `{id, name, arguments}`.
///
/// `arguments` is an OBJECT by construction — the grammar can only produce child
/// elements, and every consumer indexes it, so the type carries that invariant
/// instead of each read site re-checking it.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    /// Opaque id, minted per parsed call (python `_mint_id`).
    pub id: String,
    /// The tool the model named — one of
    /// [`crate::tool_parse::TOOLS`]. Any other name is a VERB the model
    /// compressed into the tool slot, and the parser refuses it by name.
    pub name: String,
    /// The call's arguments: top-level routing (`target_id`, `reply_to`) plus
    /// the `payload` that rides to the target.
    pub arguments: Map<String, Value>,
    /// The stream part this call was previewed as (python `call["stream_id"]`) —
    /// stamped by routing ([`crate::route`]) when the call's exact body matches a
    /// closed call part; the parser never sets it.
    pub stream_id: Option<String>,
}

impl ToolCall {
    /// A call with a freshly minted [`mint_call_id`].
    pub fn new(name: impl Into<String>, arguments: Map<String, Value>) -> Self {
        Self {
            id: mint_call_id(),
            name: name.into(),
            arguments,
            stream_id: None,
        }
    }

    /// The arguments as a JSON value — for handing the payload on to
    /// `kernel.send`, and for whole-object assertions.
    pub fn arguments_value(&self) -> Value {
        Value::Object(self.arguments.clone())
    }
}

/// Opaque id for one parsed tool call. Mirrors python's
/// `f"call_{secrets.token_hex(4)}"`: the same `call_` prefix and 8 hex digits.
///
/// MOVED here from `helpers.rs`, where it was `mint_tool_call_id` and had no
/// callers at all. The parser is the only thing that mints one — python keeps
/// `_mint_id` inside `tool_parse.py` for exactly that reason — so there is one
/// minter, in the module that needs it.
pub fn mint_call_id() -> String {
    let hex: String = crate::helpers::os_random(4)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("call_{hex}")
}
