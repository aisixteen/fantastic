//! The verbs `web_ws` declares (issue #642): one struct per verb, then `verbs!`. The
//! four inbound verbs are the ones a wire leg shares (`web_rest` declares
//! `inbound_call` from here): how an admitted frame becomes the leg's own call.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value};

/// Declare this surface's route mounts for the parent `web` agent: the catch-all `/{host_id}/ws`, or the record's `route` literal.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetRoutes {}

/// One call a wire leg sends onward: where to, what, and the name of its execution.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OnwardCall {
    /// The agent the call goes to.
    pub target: String,
    /// The call: a verb frame with its own `type`.
    pub payload: Value,
    /// The name of the execution; the same name twice is one call.
    pub id: Option<String>,
    /// Paths into `payload` whose values are shown as `[secret]`.
    pub secret: Option<Vec<String>>,
}

/// Send `item` onward AS THIS LEG and return the onward call's reply: how an admitted inbound call frame becomes the leg's own call, gated as the leg.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboundCall {
    /// The call to send onward.
    pub item: OnwardCall,
}

/// Emit `payload` on `target` AS THIS LEG (gated as the leg, verb `emit`): how an admitted inbound emit frame becomes the leg's own emit.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboundEmit {
    /// The agent whose inbox gets the event.
    #[schemars(length(min = 1))]
    pub target: String,
    /// The event.
    pub payload: Map<String, Value>,
}

/// Mirror `src`'s traffic into `reader` AS THIS LEG (gated as the leg, verb `watch`). Returns {watching: src} or the refusal.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboundWatch {
    /// The agent to watch.
    #[schemars(length(min = 1))]
    pub src: String,
    /// The registered inbox that receives its traffic.
    #[schemars(length(min = 1))]
    pub reader: String,
}

/// May THIS LEG tap the kernel-wide state stream (gated on the root, verb `state_subscribe`)? Returns {subscribed: true} or the refusal; the transport then wires the tap.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InboundStateSubscribe {}

fantastic_kernel::verbs! {
    "get_routes" => GetRoutes,
    "inbound_call" => InboundCall,
    "inbound_emit" => InboundEmit,
    "inbound_watch" => InboundWatch,
    "inbound_state_subscribe" => InboundStateSubscribe,
}
