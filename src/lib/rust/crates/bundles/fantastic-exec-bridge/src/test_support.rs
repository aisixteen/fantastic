//! FOR TESTS (this crate's and its dependents'): driving `start`, whose call SETTLES
//! WHEN ITS JOB EXITS (#795).

use fantastic_kernel::{AgentId, Item, Kernel};
use serde_json::{json, Value};
use std::sync::Arc;

/// The job a `start` ran, as `status` shows it once [`crate::start_announced`] named it
/// (`job_id`, `status`, `pty`, …, and `announced`: the `job_started` payload). A start
/// that settles first — refused, or its job already ended — is that settle's result.
///
/// # Panics
/// No announcement and no settle within 20 seconds.
pub async fn started(kernel: &Arc<Kernel>, a: &AgentId, payload: Value) -> Value {
    let ann = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        crate::start_announced(kernel, a, payload),
    )
    .await
    .unwrap_or_else(|_| panic!("{a}: no job_started within 20 s"));
    let ann = match ann {
        Ok(ann) => ann,
        Err(settled) => return settled,
    };
    let st = kernel
        .operator_send(vec![Item::new(a.clone(), json!({"type": "status", "job_id": ann["job_id"]}))])
        .await
        .remove(0)
        .result;
    let mut job = st["jobs"][0].clone();
    job["announced"] = ann;
    job
}
