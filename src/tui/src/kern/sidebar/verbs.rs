//! The verbs the sidebar agent declares (issue #642): it answers `boot` (nothing to do)
//! and the `reflect` every agent answers, which returns its html view.

use schemars::JsonSchema;
use serde::Deserialize;

/// The kernel starts the sidebar: nothing to open. Answers nothing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

fantastic_kernel::verbs! {
    "boot" => Boot,
}
