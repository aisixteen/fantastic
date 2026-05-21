//! Workdir bootstrap — the canonical "wake a kernel on disk" entry.
//!
//! Every caller (CLI daemon, one-shot RPC, Swift-embedded library,
//! integration tests) goes through here so the boot sequence stays
//! identical:
//!
//! 1. Acquire `.fantastic/lock.json` (PID-keyed; stale → overwritten).
//! 2. Read or create `.fantastic/agent.json` (the root record).
//! 3. Register the root agent in the kernel index.
//! 4. Hydrate `<workdir>/.fantastic/agents/**` with weak-load semantics
//!    (agents whose `handler_module` isn't registered in `bundles`
//!    get logged + skipped; the on-disk record stays untouched).
//!
//! The caller supplies the [`BundleRegistry`] BEFORE calling
//! [`bootstrap`] — that's where the runtime decides which bundles
//! ship in this binary.

use crate::agent::{Agent, AgentId, AgentRecord};
use crate::bundle::BundleRegistry;
use crate::errors::KernelResult;
use crate::kernel::Kernel;
use crate::{lock, persistence};
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default id for the root agent. Matches the workdir convention.
pub const DEFAULT_ROOT_ID: &str = "core";

/// Options for [`bootstrap`].
#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// Workdir root. `.fantastic/` lives inside.
    pub workdir: PathBuf,
    /// Whether to acquire the PID lock. Set `false` for one-shot RPC
    /// modes that only want to read the tree (Python's
    /// `fantastic reflect` style — works even while a daemon owns
    /// the dir).
    pub acquire_lock: bool,
    /// Override the root agent's id. Default `"core"`.
    pub root_id: String,
}

impl BootstrapOptions {
    /// Default options for a daemon bootstrap (acquires lock; root
    /// id `"core"`).
    pub fn daemon(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            acquire_lock: true,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }

    /// Default options for a one-shot read (no lock).
    pub fn one_shot(workdir: impl Into<PathBuf>) -> Self {
        Self {
            workdir: workdir.into(),
            acquire_lock: false,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }
}

/// Outcome of a successful [`bootstrap`].
pub struct BootedKernel {
    /// The live kernel (root set, agents hydrated).
    pub kernel: Arc<Kernel>,
    /// Ids of every agent loaded during hydration (excluding root).
    /// Useful for boot-time `boot` verb dispatch in a later phase.
    pub loaded: Vec<AgentId>,
}

/// Drives the boot sequence. Caller fills `bundles` first.
pub fn bootstrap(bundles: BundleRegistry, opts: BootstrapOptions) -> KernelResult<BootedKernel> {
    let workdir = opts.workdir.clone();
    std::fs::create_dir_all(workdir.join(".fantastic")).map_err(|e| {
        crate::errors::KernelError::Persistence {
            path: workdir.join(".fantastic"),
            source: e,
        }
    })?;
    if opts.acquire_lock {
        lock::acquire(&workdir)?;
    }

    // Root record. Auto-create if absent — matches Python's behaviour
    // (fantastic on a virgin dir writes the bootstrap record).
    let root_record_path = workdir.join(persistence::ROOT_RECORD_PATH);
    let root_rec = match persistence::read_record_at(&root_record_path)? {
        Some(rec) => rec,
        None => {
            let rec = AgentRecord {
                id: opts.root_id.clone(),
                handler_module: None,
                parent_id: None,
                meta: Map::new(),
            };
            persistence::write_record_at(&root_record_path, &rec)?;
            rec
        }
    };

    // Construct the root agent — its on-disk dir is `.fantastic/`
    // (children live under `.fantastic/agents/<id>/`).
    let root_path = workdir.join(".fantastic");
    let root = Agent::new(
        AgentId(root_rec.id.clone()),
        root_rec.handler_module.clone(),
        None,
        root_rec.meta.clone(),
        root_path,
        false,
    );

    let mut kernel = Kernel::new();
    kernel.bundles = bundles;
    let kernel = Arc::new(kernel);
    let _rx = kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    let loaded = persistence::load_children(&kernel, &kernel.bundles, Arc::clone(&root))?;
    Ok(BootedKernel { kernel, loaded })
}

/// Release the lock the bootstrap call acquired. Idempotent.
pub fn shutdown(workdir: &Path) -> KernelResult<()> {
    lock::release(workdir)
}

#[cfg(test)]
mod tests;
