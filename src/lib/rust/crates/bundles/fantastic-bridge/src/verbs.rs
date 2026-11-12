//! The verbs the two bridge derivations declare (issue #642): one struct per verb,
//! then one `verbs!` per derivation — `ws_bridge` (`ws`) and `relay_connector`
//! (`relay`, which adds the four relay-directory verbs). They share one engine.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

use crate::record::Role;
use crate::DEFAULT_FORWARD_TIMEOUT_SECS;

fn default_timeout() -> f64 {
    DEFAULT_FORWARD_TIMEOUT_SECS
}

/// Build the transport from the record (`transport` and its fields), spawn the read loop and emit `bridge_up`. Idempotent: booting a connected bridge changes nothing. Returns {booted, transport}, or {error} / {already}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Tear the transport down and boot again: the manual redial. A `ws` leg whose link dropped redials its known address by itself, with a backoff, and keeps its watches; `bridge_down` then `bridge_up` mark the drop and the return.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Reconnect {}

/// Run a call on the REMOTE kernel: ships a raw `{type: call, id, target, payload}` frame over the transport, awaits the matching reply and returns the unwrapped data. The far kernel judges the call as the leg that serves it there. Local-to-local calls are plain sends; this verb is only for crossing kernels. Loop detection across several hops is the caller's job.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    /// An agent id on the REMOTE kernel.
    #[schemars(length(min = 1))]
    pub target: String,
    /// The call to run there: a verb frame with its own `type`.
    pub payload: Map<String, Value>,
    /// Seconds to wait for the reply (not `timeout`, which is a call's own routing field).
    #[serde(default = "default_timeout")]
    pub forward_timeout: f64,
}

/// Send `{type: watch, src: target}` over the transport and subscribe this leg to `target`: the `{type: event, src, payload}` frames for it are re-emitted on THIS bridge agent's inbox as the payload plus `src`; an event for anything not subscribed is dropped. Local watchers subscribe to the bridge itself. Idempotent.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchRemote {
    /// An agent id on the REMOTE kernel to watch.
    #[schemars(length(min = 1))]
    pub target: String,
}

/// Unsubscribe this leg from `target` (its events are dropped from now on) and send `{type: unwatch, src: target}` so the remote stops emitting them.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnwatchRemote {
    /// The id previously passed to `watch_remote`.
    #[schemars(length(min = 1))]
    pub target: String,
}

/// One-shot snapshot of the relay directory: the peers the relay knows, with their typed attributes.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListPeers {
    /// Seconds to wait for the relay's answer (not `timeout`, which is a call's own routing field).
    #[serde(default = "default_timeout")]
    pub list_timeout: f64,
}

/// Subscribe to the relay directory: live `peer_joined`, `peer_left` and `peer_updated` events are re-emitted on this connector's inbox.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WatchDirectory {}

/// Stop the directory subscription `watch_directory` started.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnwatchDirectory {}

/// Advertise or update this peer's DIRECTORY identity, the typed attributes the relay keeps for it. Merged over the record and persisted (it re-announces on the next boot), then pushed to the relay live (`peer_updated`). An explicit null retracts a field.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetIdentity {
    /// `manager` or `kernel`.
    #[serde(default, deserialize_with = "present")]
    pub role: Option<Option<Role>>,
    /// The guid of the manager this kernel belongs to; null makes it standalone.
    #[serde(default, deserialize_with = "present")]
    pub owner_guid: Option<Option<String>>,
    /// What this peer exposes, as the relay's directory lists it.
    #[serde(default, deserialize_with = "present")]
    pub exposes: Option<Option<Vec<String>>>,
}

/// A field that is PRESENT (null included) is `Some`; an absent one stays `None`.
fn present<'de, T, D>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    T: Deserialize<'de>,
    D: Deserializer<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// The verbs of a `ws_bridge`.
pub mod ws {
    use super::{Boot, Forward, Reconnect, UnwatchRemote, WatchRemote};
    fantastic_kernel::verbs! {
        "boot" => Boot,
        "reconnect" => Reconnect,
        "forward" => Forward,
        "watch_remote" => WatchRemote,
        "unwatch_remote" => UnwatchRemote,
    }
}

/// The verbs of a `relay_connector`: the bridge's, and the relay directory's.
pub mod relay {
    use super::{
        Boot, Forward, ListPeers, Reconnect, SetIdentity, UnwatchDirectory, UnwatchRemote,
        WatchDirectory, WatchRemote,
    };
    fantastic_kernel::verbs! {
        "boot" => Boot,
        "reconnect" => Reconnect,
        "forward" => Forward,
        "watch_remote" => WatchRemote,
        "unwatch_remote" => UnwatchRemote,
        "list_peers" => ListPeers,
        "watch_directory" => WatchDirectory,
        "unwatch_directory" => UnwatchDirectory,
        "set_identity" => SetIdentity,
    }
}
