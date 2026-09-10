//! The parsed shape ([`ToolCall`]) and the call-id minter.

use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

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
}

impl ToolCall {
    /// A call with a freshly minted [`mint_call_id`].
    pub fn new(name: impl Into<String>, arguments: Map<String, Value>) -> Self {
        Self {
            id: mint_call_id(),
            name: name.into(),
            arguments,
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
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut stack: u64 = 0;
    let stack_ptr = &mut stack as *mut u64 as u64;
    let mix = nanos ^ stack_ptr ^ std::process::id() as u64;
    format!("call_{:08x}", mix as u32)
}
