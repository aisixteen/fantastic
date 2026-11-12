//! The fields the two bridge derivations' records carry, beside the ones every record
//! has (issue #642).

use fantastic_ai_core::record::Credentials;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a relay_connector is in the relay's directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A manager kernel: others belong to it.
    Manager,
    /// A plain kernel.
    Kernel,
}

/// Which transport a ws_bridge dials.
#[derive(Debug, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WsKind {
    /// A WebSocket to the far kernel's `web_ws` leg.
    Ws,
    /// An in-process pair, for tests.
    Memory,
}

/// Which transport a relay_connector dials.
#[derive(Debug, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RelayKind {
    /// A WebSocket to the relay kernel.
    Relay,
    /// An in-process pair, for tests.
    Memory,
}

/// The fields a ws_bridge's record carries.
#[derive(Debug, JsonSchema)]
pub struct WsRecord {
    /// The transport this bridge dials.
    pub transport: WsKind,
    /// The far kernel's host (the `ws` transport; nothing is guessed).
    #[schemars(length(min = 1))]
    pub host: Option<String>,
    /// The far kernel's port (the `ws` transport).
    #[schemars(range(min = 1))]
    pub port: Option<u16>,
    /// The id of the `web_ws` leg on the far kernel (the URL's first segment).
    #[schemars(length(min = 1))]
    pub peer_id: Option<String>,
    /// Who may reach this leg: a rule name (`allow_all_ingress`, `deny_ingress`, `password_ingress`) or `{type, env}`; absent, the leg is sealed.
    pub ingress_rule: Option<Value>,
    /// What this leg presents on a frame it sends: a rule name or `{type, env}`; absent, nothing.
    pub egress_rule: Option<Value>,
    /// The shorthand stem a rule name is built from per direction (`password`).
    pub auth: Option<Value>,
}

/// The fields a relay_connector's record carries.
#[derive(Debug, JsonSchema)]
pub struct RelayRecord {
    /// The transport this connector dials.
    pub transport: RelayKind,
    /// The relay's WebSocket URL.
    #[schemars(length(min = 1))]
    pub relay_url: Option<String>,
    /// The guid of the peer this connector talks to.
    #[schemars(length(min = 1))]
    pub partner_guid: Option<String>,
    /// This connector's own guid, minted on the first boot and kept.
    #[schemars(length(min = 1))]
    pub guid: Option<String>,
    /// The credential store and ref holding the relay group password (field `password`, bound to the relay's host).
    pub credentials: Option<Credentials>,
    /// Seconds between heartbeats to the relay; 0 or less turns them off.
    pub heartbeat: f64,
    /// Seconds before a dropped link redials.
    pub reconnect: f64,
    /// What this peer is in the relay's directory; absent, none is advertised.
    pub role: Option<Role>,
    /// The guid of the manager this kernel belongs to; absent, standalone.
    pub owner_guid: Option<String>,
    /// What this peer exposes, as the directory lists it.
    pub exposes: Option<Vec<String>>,
    /// Who may reach this leg: a rule name or `{type, env}`; absent, the leg is sealed.
    pub ingress_rule: Option<Value>,
    /// What this leg presents on a frame it sends: a rule name or `{type, env}`; absent, nothing.
    pub egress_rule: Option<Value>,
    /// The shorthand stem a rule name is built from per direction (`password`).
    pub auth: Option<Value>,
}

/// The ws_bridge's record declaration.
pub mod ws {
    fantastic_kernel::record!(WsRecord);
    use super::WsRecord;
}

/// The relay_connector's record declaration.
pub mod relay {
    fantastic_kernel::record!(RelayRecord);
    use super::RelayRecord;
}
