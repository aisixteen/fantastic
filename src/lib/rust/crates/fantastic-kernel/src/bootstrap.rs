//! Bootstrap — the canonical "wake a kernel" entry.
//!
//! Every caller (CLI daemon, one-shot RPC, embedded library,
//! integration tests) goes through here so the boot sequence stays
//! identical:
//!
//! - **Disk mode** ([`StorageMode::Disk`]):
//!   1. Acquire `.fantastic/lock.yaml` (PID-keyed; stale → overwritten).
//!   2. Read the tree — `.fantastic/agent.yaml` (the root record) and every
//!      record under its children dir — into a flat record list; seed a root
//!      on a virgin workdir (or over a missing/corrupt root record).
//!   3. [`Kernel::load_records`] it, python's `kernel.load(read_tree(…))`:
//!      VALIDATED first (a bad id, a reserved key, a duplicate, an orphan
//!      refuses the boot), then built (weak-load: an unknown `handler_module`
//!      is logged + skipped with its subtree).
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
//! (`.fantastic/agents/<id>/agent.yaml` + sidecars) is loosely
//! coupled to the in-RAM [`crate::state::KernelState`]. Persistence
//! merges kernel-managed fields into existing `agent.yaml` files
//! (see [`crate::persistence::persist`]) — never wholesale
//! overwrites, never touches sidecar files. The in-RAM state can
//! drift from disk; bundles reconcile their own slices on next
//! touch.

use crate::agent::{Agent, AgentId};
use crate::bundle::BundleRegistry;
use crate::errors::{KernelError, KernelResult};
use crate::kernel::Kernel;
use crate::storage::StorageMode;
use crate::{lock, persistence};
use serde_json::{Map, Value};
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
            storage: StorageMode::disk(workdir.into()),
            acquire_lock: true,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }

    /// Disk-backed daemon bootstrap whose state lives in `state_dir`, not in
    /// `<workdir>/.fantastic` (acquires lock; root id `"kernel_state"`). The
    /// on-disk format is the same; only its place is the host's choice.
    pub fn daemon_at(workdir: impl Into<PathBuf>, state_dir: impl Into<PathBuf>) -> Self {
        Self {
            storage: StorageMode::Disk {
                workdir: workdir.into(),
                state_dir: state_dir.into(),
            },
            acquire_lock: true,
            root_id: DEFAULT_ROOT_ID.to_string(),
        }
    }

    /// Disk-backed one-shot read (no lock).
    pub fn one_shot(workdir: impl Into<PathBuf>) -> Self {
        Self {
            storage: StorageMode::disk(workdir.into()),
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
    /// A FRESH root's record, not yet on disk: a lockless boot defers it to the
    /// mode that takes the dir's lock ([`Self::write_seed`]).
    pub pending_seed: Option<(PathBuf, crate::agent::AgentRecord)>,
}

impl BootedKernel {
    /// Write a fresh root's record now that this process holds the dir's lock;
    /// once. Python `kernel/modes` `take_seed`.
    ///
    /// # Errors
    /// The record write's.
    pub fn write_seed(&mut self) -> KernelResult<()> {
        match self.pending_seed.take() {
            Some((path, rec)) => persistence::write_record_at(&path, &rec),
            None => Ok(()),
        }
    }
}

/// Drives the boot sequence. Caller fills `bundles` first.
pub fn bootstrap(bundles: BundleRegistry, opts: BootstrapOptions) -> KernelResult<BootedKernel> {
    let mut kernel = Kernel::with_storage(opts.storage.clone());
    kernel.bundles = bundles;
    let kernel = Arc::new(kernel);

    match &opts.storage {
        StorageMode::Disk { workdir, state_dir } => boot_disk(&kernel, workdir, state_dir, &opts),
        StorageMode::InMemory => boot_in_memory(&kernel, &opts.root_id),
    }
}

fn boot_disk(
    kernel: &Arc<Kernel>,
    workdir: &Path,
    state_dir: &Path,
    opts: &BootstrapOptions,
) -> KernelResult<BootedKernel> {
    refuse_home_as_state_dir(workdir, state_dir, fantastic_home().as_deref())?;
    let fantastic_dir = state_dir.to_path_buf();
    // A LOCKLESS boot (`--help`, `reflect`) creates nothing on disk; the lock
    // lives in `.fantastic`, so a locked one makes the dir first.
    if opts.acquire_lock {
        std::fs::create_dir_all(&fantastic_dir).map_err(|e| KernelError::Persistence {
            path: fantastic_dir.clone(),
            source: e,
        })?;
        lock::acquire_at(state_dir)?;
    }

    // THE TREE, python `_bootstrap`: `records = read_tree(.fantastic)`. A
    // missing or unparseable root `agent.yaml` does not hide the children under
    // `agents/` (python `read_tree`) — they are parented to the root
    // id the seed below mints.
    let mut records = if fantastic_dir.is_dir() {
        read_tree(&fantastic_dir, &opts.root_id)?
    } else {
        Vec::new()
    };
    let seeded = !records
        .iter()
        .any(|r| r.get("parent_id").is_none_or(Value::is_null));
    let root_file = fantastic_dir.join(persistence::RECORD_FILE);
    if seeded && root_file.exists() {
        // A root record that is THERE but did not parse is never written over —
        // the seed would lose its other keys (python `main._bootstrap`).
        return Err(KernelError::InvalidSnapshot(format!(
            "{} is unreadable — fix or remove it; the root is not re-seeded over it",
            root_file.display()
        )));
    }
    if seeded {
        let mut seed = Map::new();
        seed.insert("id".to_string(), Value::from(opts.root_id.clone()));
        seed.insert(
            "handler_module".to_string(),
            Value::from(ROOT_HANDLER_MODULE),
        );
        records.insert(0, seed);
    }
    // `kernel.load(records)`: VALIDATED before anything is built — a record with
    // a bad id or a reserved key, a duplicate, an orphan refuses the boot. The
    // kernel keeps no conversation and hydrates no rows: the load restores the
    // tree and the id counter (the root record's reserved bound, `next_id`).
    let root_hm = records
        .iter()
        .find(|r| r.get("parent_id").is_none_or(Value::is_null))
        .and_then(|r| r.get("handler_module"))
        .map(fantastic_bundle::py_str)
        .unwrap_or_default();
    // FOLDER BUNDLES before the tree loads, so an instance's handler resolves
    // (python main._bootstrap → kernel_state.load_bundles).
    if let Some(b) = kernel.bundles.get(&root_hm) {
        b.cold_boot(kernel, &fantastic_dir);
    }
    kernel.load_records(records)?;
    // The root weak-loads like any record: a runtime without its loader bundle
    // has no tree to run. Said, not papered over with a root it cannot serve.
    let Some(root) = kernel.root() else {
        return Err(KernelError::InvalidSnapshot(format!(
            "the root's bundle {root_hm} is not installed in this runtime"
        )));
    };

    // THE VIRGIN SEED IS THE CONSTRUCTED ROOT, not the bare literal: the record
    // on disk then carries the structural grant baseline `Agent::new` stamps, in
    // its order (`can_call, callable_by`), and the loader's `boot` appends
    // `guid` after them. COLD primitive — this runs before any agent (and thus
    // any `file_bridge` store provider) exists, so it cannot route through one.
    // Written only under the dir's lock: a lockless boot hands it back as
    // `pending_seed`, for the mode that takes the lock to write
    // ([`BootedKernel::write_seed`]; python `Kernel.pending_seed`).
    let mut pending_seed = (seeded && opts.storage.is_disk()).then(|| (root_file, root.record()));
    if opts.acquire_lock {
        if let Some((path, rec)) = pending_seed.take() {
            persistence::write_record_at(&path, &rec)?;
        }
    }
    let loaded: Vec<AgentId> = kernel
        .agent_ids()
        .into_iter()
        .filter(|id| *id != root.id)
        .collect();
    Ok(BootedKernel {
        kernel: Arc::clone(kernel),
        loaded,
        pending_seed,
    })
}

/// python `kernel_state.read_tree` for the bootstrap's own `.fantastic`: the root
/// record (absent / unreadable / not a mapping ⇒ none) and every record under its
/// children dir, flat, `parent_id` from DISK POSITION. Corrupt siblings skip.
///
/// Every path is clamped under `.fantastic` as python's `fs` clamps it
/// ([`persistence::clamp`]): a `children_dir` that resolves outside (`../x`), or
/// a record (or its dir) linked outside, refuses the boot — python raises
/// `ValueError: fs: path '<rel>' escapes root` there — instead of hydrating
/// records from another directory.
fn read_tree(fantastic_dir: &Path, root_id: &str) -> KernelResult<Vec<Map<String, Value>>> {
    let refused = |text: String| KernelError::Persistence {
        path: fantastic_dir.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, text),
    };
    let root_rec = persistence::read_record_clamped(fantastic_dir, persistence::RECORD_FILE)
        .map_err(refused)?;
    let mut out: Vec<Value> = Vec::new();
    match root_rec {
        None => persistence::walk_tree(
            fantastic_dir,
            crate::kernel::DEFAULT_CHILDREN_DIR,
            &Value::from(root_id),
            &mut out,
            crate::kernel::DEFAULT_CHILDREN_DIR,
        ),
        Some(rec) => {
            let cd = rec
                .get("children_dir")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or(crate::kernel::DEFAULT_CHILDREN_DIR)
                .to_string();
            let id = rec.get("id").cloned().unwrap_or(Value::Null);
            out.push(Value::Object(rec));
            persistence::walk_tree(fantastic_dir, &cd, &id, &mut out, &cd)
        }
    }
    .map_err(refused)?;
    Ok(out
        .into_iter()
        .filter_map(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .collect())
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
        pending_seed: None,
    })
}

/// Release the lock the bootstrap call acquired. Idempotent.
/// No-op on a workdir that was never used in Disk mode.
pub fn shutdown(workdir: &Path) -> KernelResult<()> {
    lock::release(workdir)
}

/// [`shutdown`] for a kernel booted with [`BootstrapOptions::daemon_at`]:
/// releases the lock in its `state_dir`.
///
/// # Errors
/// As [`shutdown`].
pub fn shutdown_at(state_dir: &Path) -> KernelResult<()> {
    lock::release_at(state_dir)
}

/// The variable naming the fantastic home.
pub const HOME_VAR: &str = "FANTASTIC_HOME";

/// The fantastic home: `FANTASTIC_HOME`, else `~/.fantastic` (`$HOME`, or
/// `%USERPROFILE%` on Windows). `None` when neither names a dir.
#[must_use]
pub fn fantastic_home() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os(HOME_VAR).filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(h));
    }
    let user = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(user)
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".fantastic"))
}

/// The home as a path is written — python `kernel._home.home_spelled`:
/// `FANTASTIC_HOME` made absolute (against the current dir, folded), else
/// `~/.fantastic` with the `~` kept, for a reader.
///
/// # Errors
/// The current dir cannot be read (a relative `FANTASTIC_HOME` needs it).
pub fn home_spelled() -> std::io::Result<String> {
    match std::env::var_os(HOME_VAR).filter(|v| !v.is_empty()) {
        Some(h) => Ok(fold(&std::env::current_dir()?, Path::new(&h))
            .to_string_lossy()
            .into_owned()),
        None => Ok("~/.fantastic".to_string()),
    }
}

/// THE GUARD: a kernel never keeps its own state in the fantastic home. With
/// the home at `~/.fantastic`, a kernel booted in `~` would otherwise take the
/// home for its `.fantastic`. Both sides are absolutized against the current
/// dir and folded lexically (`.`/`..`), so the rule is the same in every
/// runtime and needs no dir to exist.
///
/// # Errors
/// [`KernelError::HomeAsStateDir`] when `state_dir` is `home`.
pub fn refuse_home_as_state_dir(
    workdir: &Path,
    state_dir: &Path,
    home: Option<&Path>,
) -> KernelResult<()> {
    let Some(home) = home else {
        return Ok(());
    };
    let cwd = std::env::current_dir().map_err(|e| KernelError::Persistence {
        path: workdir.to_path_buf(),
        source: e,
    })?;
    if fold(&cwd, state_dir) == fold(&cwd, home) {
        return Err(KernelError::HomeAsStateDir {
            dir: workdir.to_path_buf(),
            home: home.to_path_buf(),
        });
    }
    Ok(())
}

/// `p` absolutized against `base`, `.`/`..` folded, with no filesystem access.
fn fold(base: &Path, p: &Path) -> PathBuf {
    use std::path::Component;
    let joined = base.join(p);
    let mut out: Vec<Component> = Vec::new();
    for c in joined.components() {
        match c {
            Component::ParentDir if matches!(out.last(), Some(Component::Normal(_))) => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

#[cfg(test)]
mod tests;
