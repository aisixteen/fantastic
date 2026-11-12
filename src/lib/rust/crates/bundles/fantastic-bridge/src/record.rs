//! The fields the two bridge derivations' records carry, beside the ones every record
//! has (issue #642).

use fantastic_ai_core::record::Credentials;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

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
    /// The leg's ingress, egress and token rules.
    #[serde(flatten)]
    pub rules: fantastic_io_bridge::authorizer::Rules,
}

/// Seconds a forward or a directory read waits for its reply, when the call names none.
pub const DEFAULT_TIMEOUT_SECS: f64 = 30.0;
/// Seconds between relay heartbeats, when the record names none.
pub const DEFAULT_HEARTBEAT_SECS: f64 = 30.0;
/// Seconds before a dropped relay link redials, when the record names none.
pub const DEFAULT_RECONNECT_SECS: f64 = 10.0;

pub(crate) fn default_timeout() -> f64 {
    DEFAULT_TIMEOUT_SECS
}
fn default_heartbeat() -> f64 {
    DEFAULT_HEARTBEAT_SECS
}
fn default_reconnect() -> f64 {
    DEFAULT_RECONNECT_SECS
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
    #[serde(default = "default_heartbeat")]
    pub heartbeat: f64,
    /// Seconds before a dropped link redials.
    #[serde(default = "default_reconnect")]
    pub reconnect: f64,
    /// What this peer is in the relay's directory; absent, none is advertised.
    pub role: Option<Role>,
    /// The guid of the manager this kernel belongs to; absent, standalone.
    pub owner_guid: Option<String>,
    /// What this peer exposes, as the directory lists it.
    pub exposes: Option<Vec<String>>,
    /// The leg's ingress, egress and token rules.
    #[serde(flatten)]
    pub rules: fantastic_io_bridge::authorizer::Rules,
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
