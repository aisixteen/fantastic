//! Bootstrap — the canonical "wake a kernel" entry.
//!
//! Every caller (CLI daemon, one-shot RPC, embedded library,
//! integration tests) goes through here so the boot sequence stays
//! identical:
//!
//! - **Disk mode** ([`StorageMode::Disk`]):
//!   1. Acquire `.fantastic/lock.json` (PID-keyed; stale → overwritten).
//!   2. Read `.fantastic/agent.json` (the root record); auto-create
//!      on a virgin workdir.
//!   3. Hydrate `<workdir>/.fantastic/agents/**` via
//!      [`crate::persistence::load_children`] (weak-load: unknown
//!      `handler_module` is logged + skipped).
//! - **InMemory mode** ([`StorageMode::InMemory`]):
//!   1. No lock, no fs I/O.
//!   2. Create the root agent in process memory only.
//!
//! Both modes leave the kernel in the same logical state: root is
//! registered, [`Kernel::root`] points at it, [`Kernel::agents`]
//! contains every loaded agent.
//!
//! The caller supplies the [`BundleRegistry`] BEFORE calling
//! [`bootstrap`] — that's where the runtime decides which bundles
//! ship in this binary.
//!
//! **Dirty-binding contract**: on Disk mode the on-disk tree
//! (`.fantastic/agents/<id>/agent.json` + sidecars) is loosely
//! coupled to the in-RAM [`crate::state::KernelState`]. Persistence
//! merges kernel-managed fields into existing `agent.json` files
//! (see [`crate::persistence::persist`]) — never wholesale
//! overwrites, never touches sidecar files. The in-RAM state can
//! drift from disk; bundles reconcile their own slices on next
//! touch.

use crate::agent::{Agent, AgentId, AgentRecord};
use crate::bundle::BundleRegistry;
use crate::errors::{KernelError, KernelResult};
use crate::kernel::Kernel;
use crate::storage::StorageMode;
use crate::{lock, persistence};
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Default id for the root agent — python's `kernel_state`, which IS the tree
/// root and carries the loader's own `handler_module`.
pub const DEFAULT_ROOT_ID: &str = "kernel_state";

/// The root's `handler_module`: the durable-state loader bundle. A root record
/// is seeded with it on a virgin dir, exactly as python's `_bootstrap` does.
pub const ROOT_HANDLER_MODULE: &str = "kernel_state.tools";

/// Options for [`bootstrap`].
#[derive(Debug, Clone)]
pub struct BootstrapOptions {
    /// State medium. See [`StorageMode`].
    pub storage: StorageMode,
    /// Whether to acquire the PID lock. Ignored in
    /// [`StorageMode::InMemory`] (no lock without a workdir).
    pub acquire_lock: bool,
    /// Override the root agent's id. Default `"kernel_state"`.
    pub root_id: String,
}

impl BootstrapOptions {
    /// Disk-backed daemon bootstrap (acquires lock; root id `"kernel_state"`).
    pub fn daemon(workdir: impl Into<PathBuf>) -> Self {
        Self {
            storage: StorageMode::Disk(workdir.into()),
            acquire_lock: true,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }

    /// Disk-backed one-shot read (no lock).
    pub fn one_shot(workdir: impl Into<PathBuf>) -> Self {
        Self {
            storage: StorageMode::Disk(workdir.into()),
            acquire_lock: false,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }

    /// In-memory bootstrap — no filesystem I/O, no lock. Used by the
    /// embedding app's "brain" kernel and by tests that don't want to
    /// touch disk. The kernel still answers [`Kernel::save`] /
    /// [`Kernel::load`]; the consumer drives any persistence
    /// externally.
    pub fn in_memory() -> Self {
        Self {
            storage: StorageMode::InMemory,
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
    /// Useful for boot-time `boot` verb dispatch.
    pub loaded: Vec<AgentId>,
    /// The highest message id the log held at hydration — where a persister
    /// should subscribe. Everything minted after this point (the roster's own
    /// boot traffic included) is REPLAYED to it, so the pump can be installed
    /// later without losing what happened in between.
    pub messages_after: i64,
}

/// Drives the boot sequence. Caller fills `bundles` first.
pub fn bootstrap(bundles: BundleRegistry, opts: BootstrapOptions) -> KernelResult<BootedKernel> {
    let mut kernel = Kernel::with_storage(opts.storage.clone());
    kernel.bundles = bundles;
    let kernel = Arc::new(kernel);

    match &opts.storage {
        StorageMode::Disk(workdir) => boot_disk(&kernel, workdir, &opts),
        StorageMode::InMemory => boot_in_memory(&kernel, &opts.root_id),
    }
}

fn boot_disk(
    kernel: &Arc<Kernel>,
    workdir: &Path,
    opts: &BootstrapOptions,
) -> KernelResult<BootedKernel> {
    let fantastic_dir = workdir.join(".fantastic");
    std::fs::create_dir_all(&fantastic_dir).map_err(|e| KernelError::Persistence {
        path: fantastic_dir.clone(),
        source: e,
    })?;
    if opts.acquire_lock {
        lock::acquire(workdir)?;
    }

    // THE ROOT RECORD, or a fresh seed. A missing or unparseable `agent.json`
    // reseeds the root like a virgin dir; the children under `agents/` are
    // walked either way (python `read_tree`, 2026-09-20).
    let root_record_path = fantastic_dir.join("agent.json");
    let (root_rec, seeded) = match read_record_at(&root_record_path) {
        Some(rec) => (rec, false),
        None => (
            AgentRecord {
                id: opts.root_id.clone(),
                handler_module: Some(ROOT_HANDLER_MODULE.to_string()),
                parent_id: None,
                meta: Map::new(),
            },
            true,
        ),
    };

    // Construct the root agent — its on-disk dir is `.fantastic/`
    // (children live under `.fantastic/agents/<id>/`).
    let root = Agent::new(
        AgentId(root_rec.id.clone()),
        root_rec.handler_module.clone(),
        None,
        root_rec.meta.clone(),
        fantastic_dir.clone(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(Arc::clone(&root));

    // THE VIRGIN SEED IS THE CONSTRUCTED ROOT, not the bare literal: the record
    // on disk then carries the structural grant baseline `Agent::new` stamps, in
    // its order (`can_call, callable_by`), and the loader's `boot` appends
    // `guid` after them. COLD primitive — this runs before any agent (and thus
    // any `file_bridge` store provider) exists, so it cannot route through one.
    if seeded && opts.storage.is_disk() {
        persistence::write_record_at(&root_record_path, &root.record())?;
    }

    // THE MESSAGE LOG, COLD — before any child exists, so a hydrated id can
    // never collide with one a booting agent mints. A cut or foreign log is
    // LOUD: half a conversation is worse than none, and the operator truncates
    // at the last complete row rather than the kernel guessing. Success is
    // SILENT (python prints nothing).
    let messages_after = match crate::messages_log::hydrate_messages(kernel, &fantastic_dir) {
        Ok(0) => 0,
        Ok(_) => kernel.rows.next_id() - 1,
        Err(e) => {
            return Err(KernelError::Persistence {
                path: fantastic_dir.join(crate::messages_log::MESSAGES_FILE),
                source: std::io::Error::other(e),
            })
        }
    };

    // A RESEEDED ROOT STILL HYDRATES ITS CHILDREN: python's `read_tree` walks
    // `agents/` under the default layout when the root record is missing or
    // corrupt (2026-09-20); the reseed only replaces the root's own record.
    let loaded = persistence::load_children(kernel, &kernel.bundles, Arc::clone(&root))?;
    Ok(BootedKernel {
        kernel: Arc::clone(kernel),
        loaded,
        messages_after,
    })
}

fn boot_in_memory(kernel: &Arc<Kernel>, root_id: &str) -> KernelResult<BootedKernel> {
    // InMemory mode never touches the filesystem. The root agent's
    // `root_path` is a sentinel empty PathBuf — bundles that try to
    // write sidecars in InMemory mode will get an fs error from
    // their own code, which is correct (they shouldn't be writing
    // sidecars in a brain-kernel context).
    // NO GUID HERE. The instance guid is minted ONCE by the root loader's `boot`
    // (python `kernel_state._boot`), so an in-memory kernel gets one the same
    // way a disk one does — and never two different ones.
    let root = Agent::new(
        AgentId(root_id.to_string()),
        Some(ROOT_HANDLER_MODULE.to_string()),
        None,
        Map::new(),
        PathBuf::new(),
        false,
    );
    kernel.register(Arc::clone(&root));
    kernel.set_root(root);
    Ok(BootedKernel {
        kernel: Arc::clone(kernel),
        loaded: Vec::new(),
        // In memory: nothing was read, so a persister starts at the beginning.
        messages_after: 0,
    })
}

/// Read the ROOT [`AgentRecord`] from `path`, or `None` when there is nothing
/// usable there — absent, unreadable, or not JSON. python's `read_tree`
/// swallows all three the same way (`json.JSONDecodeError, OSError,
/// ValueError` → `[]`) and the caller reseeds; a corrupt root is NOT a fatal
/// error, and nothing is migrated or repaired.
fn read_record_at(path: &Path) -> Option<AgentRecord> {
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Release the lock the bootstrap call acquired. Idempotent.
/// No-op on a workdir that was never used in Disk mode.
pub fn shutdown(workdir: &Path) -> KernelResult<()> {
    lock::release(workdir)
}

#[cfg(test)]
mod tests;
