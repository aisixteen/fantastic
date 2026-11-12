//! The verbs `web_rest` declares (issue #642). `inbound_call` is the one inbound
//! verb every wire leg shares: it is declared once, in `web_ws`, and answered by
//! the same code here — a REST leg carries calls only.

use schemars::JsonSchema;
use serde::Deserialize;

pub use fantastic_web_ws::verbs::InboundCall;

/// Declare this surface's HTTP route mounts for the parent `web` agent. Returns {routes:[{kind, method, path}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetRoutes {}

fantastic_kernel::verbs! {
    "get_routes" => GetRoutes,
    "inbound_call" => InboundCall,
}
