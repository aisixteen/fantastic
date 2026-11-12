//! The verbs a py_agent bundle declares (issue #642). `boot` and `shutdown` are the
//! two every folder bundle that extends the base answers; the program's own verbs
//! are the bundle's dynamic verbs, read from its `bundle.yaml`. `reflect` is declared
//! once, in the kernel.

use schemars::JsonSchema;
use serde::Deserialize;

/// Start the instance's program (the kernel sends it when the instance is created); idempotent while the program runs.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// Stop the instance's program and answer every call still in flight with the reason; a reload (`reload_bundle`) or the kernel's stop sends it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {
    /// Why the program stops; it is said to the calls in flight.
    #[serde(default = "stopped")]
    #[schemars(length(min = 1))]
    pub reason: String,
}

fn stopped() -> String {
    "the kernel stopped".to_string()
}

fantastic_kernel::verbs! {
    dynamic;
    "boot" => Boot,
    "shutdown" => Shutdown,
}
