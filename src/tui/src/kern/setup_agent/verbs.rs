//! The verbs the canvas's set-up agent declares (issue #642): one struct per verb, then
//! `verbs!` — the list the agent's own entry checks every call against, and that
//! `reflect` and `docs` render. `reflect` is declared once, in the kernel.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// The kernel starts the agent: nothing to open. Answers nothing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// The AI backends this build links and the fields each one's record declares. Returns {backends:[{module, key, fields:[{name, type, required, enum, default, hint}]}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Backends {}

/// The keys stored so far (a ref and its host, never a value) and where they are kept. Returns {keys:[{ref, host}], mode, where}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Keys {}

/// An AI agent's settings, as the wizard shows them: its handler and every declared field's current value. Returns {id, handler, fields, saved}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AiSettings {
    /// The AI agent.
    #[schemars(length(min = 1))]
    pub id: String,
}

/// Change a live AI agent's fields (same backend): live on its next turn. The key stored for its endpoint is named and granted. Returns {configured, note}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigureAi {
    /// The AI agent.
    #[schemars(length(min = 1))]
    pub id: String,
    /// The fields to change: `{name: value}`, each of the type the backend's record declares.
    pub fields: Option<Map<String, Value>>,
}

/// Write an AI agent: a provider and the chat naming it, created, granted and booted as the human. Returns {written, id}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteAi {
    /// The chat's id.
    #[schemars(length(min = 1))]
    pub id: String,
    /// The backend: one of `backends`' modules.
    #[schemars(length(min = 1))]
    pub handler: String,
    /// The backend's own fields: `{name: value}`, each of the type its record declares.
    pub fields: Option<Map<String, Value>>,
    /// The agents the chat may call.
    pub may_call: Option<Vec<String>>,
}

/// Where keys are kept, and whether one was chosen. Returns {mode, where}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyMode {}

/// Where keys are kept: plain or secure. Returns {mode, where}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetKeyMode {
    /// `plain` or `secure`.
    pub mode: Mode,
}

/// The two places keys are kept.
#[derive(Debug, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// One private file.
    Plain,
    /// The platform keychain.
    Secure,
}

/// Store the key of a backend at an endpoint, bound to that endpoint's host. Returns {stored, note}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoreKey {
    /// The backend's module.
    #[schemars(length(min = 1))]
    pub module: String,
    /// The endpoint whose host the key is bound to.
    #[schemars(length(min = 1))]
    pub endpoint: String,
    /// The key itself.
    #[schemars(length(min = 1))]
    pub secret: String,
}

/// The saved set-ups. Returns {saved:[{name, record}], preferred, dir}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Saved {}

/// Save a set-up: the backend and every field set, never an id, a key or grants; the key stored for its endpoint is named. Returns {saved, path}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Save {
    /// The set-up's name.
    #[schemars(length(min = 1))]
    pub name: String,
    /// The backend: one of `backends`' modules.
    #[schemars(length(min = 1))]
    pub handler: String,
    /// The backend's own fields: `{name: value}`, each of the type its record declares.
    pub fields: Option<Map<String, Value>>,
}

/// Forget a saved set-up. Returns {forgotten}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Forget {
    /// The set-up's name.
    #[schemars(length(min = 1))]
    pub name: String,
}

/// The owner's default set-up for new AI agents; no name clears it. Returns {preferred}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Prefer {
    /// The saved set-up to prefer; absent, none.
    #[schemars(length(min = 1))]
    pub name: Option<String>,
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "backends" => Backends,
    "keys" => Keys,
    "ai_settings" => AiSettings,
    "configure_ai" => ConfigureAi,
    "write_ai" => WriteAi,
    "key_mode" => KeyMode,
    "set_key_mode" => SetKeyMode,
    "store_key" => StoreKey,
    "saved" => Saved,
    "save" => Save,
    "forget" => Forget,
    "prefer" => Prefer,
}
