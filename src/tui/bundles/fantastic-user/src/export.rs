//! EXPORT A SUBTREE AS A BLUEPRINT (#268). The engine lives in
//! `fantastic-core` (`fantastic_core::export`), because the kernel's ROOT
//! answers `export_subtree` too; this is the same engine under `user`.

pub use fantastic_core::export::*;

#[cfg(test)]
use fantastic_kernel::AgentRecord;
#[cfg(test)]
use serde_json::{Map, Value};

use std::sync::Arc;

use fantastic_kernel::Kernel;

/// `user.export_subtree {agent_id, path}` — see the engine. The arguments are the
/// verb's declared ones (`fantastic_core::verbs::ExportSubtree`), typed.
pub async fn export_subtree(kernel: &Arc<Kernel>, p: &serde_json::Value) -> serde_json::Value {
    let args = match fantastic_kernel::verbs::decode::<fantastic_core::verbs::ExportSubtree>(
        "export_subtree",
        p,
    ) {
        Ok(a) => a,
        Err(refused) => return refused,
    };
    fantastic_core::export::export_subtree(
        kernel,
        &args.agent_id,
        &args.path,
        "user.export_subtree",
    )
    .await
}

#[cfg(test)]
mod tests;
