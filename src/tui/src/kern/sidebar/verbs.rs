//! The verbs the sidebar agent declares (issue #642): `boot`, which the kernel sends when it
//! creates or loads the agent, and the `reflect` every agent answers, which returns its html
//! view.

use schemars::JsonSchema;
use serde::Deserialize;

/// The kernel starts the sidebar: from here on it follows the kernel (agents created, deleted and updated, kernels connected and gone, each chat's call phase, the person's open questions, the project's files added and removed) and publishes each change on its own inbox. Idempotent. Answers {following: true}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

fantastic_kernel::verbs! {
    "boot" => Boot,
}
