//! `kernel_state.tools` — the durable-state ROOT agent (a weak-bound STREAM
//! CONSUMER).
//!
//! Port of python `PB/loader/kernel_state/src/kernel_state/tools.py`, which is
//! canonical. `kernel_state` IS the tree root (`id="kernel_state"`). It owns the
//! record⇄bytes serialization, the tree→path mapping and the auto-persist
//! lifecycle. The auto-flush persists ONLY through a stream PROVIDER it
//! DISCOVERS (`persistence::find_store`: a `file_bridge` in the loader's subtree
//! whose root resolves to `.fantastic`). NO PROVIDER ⇒ NO AUTO-PERSIST — state
//! lives in RAM until one is wired. There is no direct-write fallback.
//!
//! Contract (duck-typed; any agent answering these is a loader):
//! ```text
//!   load_tree                 -> {records, version}   read the subtree (direct)
//!   persist_record {record}   -> {ok}                 write one agent.yaml
//!   forget_record {id}        -> {ok}                 remove one agent's dir
//!   persist_blob / load_blob                          the sidecar byte channel
//! ```
//!
//! The ROOT loader also mints the kernel's instance `guid` at `boot` and installs
//! the record `persist_writer`. There is no message tape: the kernel keeps no
//! conversation, and flushes its own execution rows into the root's memory store.

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_file::fs as fantastic_fs;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{persistence, AgentId, Kernel};

pub mod export;
pub mod folders;

/// The `handler_module` this bundle answers to — python's package path.
pub const HANDLER_MODULE: &str = "kernel_state.tools";

/// The SHORT orientation readme — `reflect readme=true` serves it from here.
/// The full manual is the keyed [`DOCS`] tree — `reflect readme=index` / `=<key>`.
pub const README: &str = include_str!("readme.md");

/// The manual, split into keyed topic docs (nested dirs = tiers via a `/` in the
/// key), compiled in and never copied to disk; `reflect readme=index` lists them
/// token-annotated and `reflect readme=<key>` drills one. Trimmed + reordered so a
/// self-authoring model reaches `self-extension` first, not a system-manual dump.
pub const DOCS: &[(&str, &str)] = &[
    ("self-extension", include_str!("docs/self-extension.md")),
    ("reflect", include_str!("docs/reflect.md")),
    ("system-verbs", include_str!("docs/system-verbs.md")),
    ("agents", include_str!("docs/agents.md")),
    ("orchestration", include_str!("docs/orchestration.md")),
    ("memory", include_str!("docs/memory.md")),
    ("persistence", include_str!("docs/persistence.md")),
    ("transports", include_str!("docs/transports.md")),
    ("streams", include_str!("docs/streams.md")),
    ("frontend", include_str!("docs/frontend.md")),
    (
        "frontend/html-agent",
        include_str!("docs/frontend/html-agent.md"),
    ),
    ("frontend/routing", include_str!("docs/frontend/routing.md")),
    ("terminal", include_str!("docs/terminal.md")),
    ("daemon", include_str!("docs/daemon.md")),
];

/// The SHORT `self_extension` a DISTILLED root reflect carries — python
/// `reflect_root_extra`'s non-full branch, verbatim.
const SELF_EXTENSION_SHORT: &str = "A DURABLE PROGRAM is a FILE plus a command (no `code` field): write it with a file agent's <type>write_stream</type> (`bytes` verbatim), run it on an `exec_bridge.tools` agent — <type>start</type> <command>python3</command> <args><item>prog.py</item></args>, matched against its `allowed` — and re-run to update. Wiring an agent to another takes BOTH halves: its `can_call` and the other's `callable_by`. More: reflect kernel with <readme>self-extension</readme>.";

/// The one-liner teaching `docs` and keyed doc fetch — python
/// `reflect_root_extra`'s `docs`.
const DOCS_LINE: &str = "How to CALL an agent: send 'kernel' <type>docs</type> <of>agent id, bundle or kernel</of> — one signature line per verb; add <verb>name</verb> <tier>detailed</tier> for one verb's full text. The manual is a keyed doc tree: send 'kernel' <type>reflect</type> with <readme>index</readme> (every topic, ~token cost), <readme>key</readme> (one topic) or <readme>true</readme> (the short orientation). Read only what a task needs.";

/// The LONG `self_extension` a `detail:"full"` root reflect carries — python's
/// full branch, verbatim (it has no fd-wire tail sentence).
const SELF_EXTENSION_FULL: &str =
    "A DURABLE PROGRAM is a FILE you write plus a command that runs it — not a \
`code`-carrying agent (there is no `code` field). A program carried as a \
JSON string is exactly how a call gets lost to escaping. Assemble these \
THREE pieces and you have built something that persists and re-runs:\n1. \
WRITE THE PROGRAM TO A FILE: never carry file contents in a JSON string. \
Use the file leg's STREAM verbs — <type>write_stream</type> <path>…</path> \
<truncate>true</truncate> <bytes>…</bytes> to write, \
<type>read_stream</type> <path>…</path> <offset>0</offset> to read, one raw \
chunk at a time. `bytes` is VERBATIM: the program between `<bytes>` and \
`</bytes>` exactly as written, real newlines, nothing escaped. The text \
verbs (`write`, `edit`, `append`) take `bytes` the same way.\n2. RUN IT: \
the executor is a general-purpose COMMAND runner. Reflect it, read its \
`allowed` regexes, then send <type>start</type> <command>path</command> \
<args><item>…</item></args>; poll <type>status</type>. It runs commands, \
not inline programs.\n3. RE-RUN: the file is durable on disk, so the SAME \
`start` command run again later re-executes the SAME program against \
whatever the inputs hold THEN — that is how a thing 'keeps itself up to \
date'.\nA WORKER THAT COMPUTES is an `exec_bridge.tools` agent (the \
EXISTING command runner) that owns a program file — you never author a new \
handler_module (you cannot add modules to the catalog and do not need to). \
'A specialist with child workers' = a parent agent whose children are \
exec_bridge runners, each with its own program file.\nWIRING TO A SIBLING: \
a record is edited with `update_agent` on the root — holding that grant (both \
ends) you edit ANY record, `totals`/`readings` included; without it you cannot \
raise your OWN authority. To let your worker call `totals`: grant the worker \
`can_call:{totals}` AND `update_agent id='totals'` to add the worker to \
`totals.callable_by` (send the WHOLE map — update replaces, not merges).\nA SPAWNED PROGRAM reaches agents over its \
control fd — a TWO-WAY socket (FANTASTIC_CTRL_FD): write {op:'send', rid, \
target, payload}; the reply comes back on the SAME fd as {op:'reply', rid, \
data}. It is not inline — read frames until one carries your `rid`, \
skipping event/inbox frames.\nCOMPOSE the agents you were given for \
everything else (`list_agents`, reflect each, send). CREATE/DELETE plain \
agents (create_agent with a handler_module and grants, delete_agent) work \
as before.";

/// Per-loader process state — what `boot` installed, so a second `boot` is a
/// no-op and `on_shutdown` can tear it down. Python keeps it on the agent
/// object (`_fs_flush_loop`).
/// The bundle's per-loader handles. The `boot` SEQUENCE lives in the kernel
/// (`persistence::boot_root_loader`); what the bundle owns is the verb surface
/// and remembering what to tear down.
#[derive(Default)]
struct LoaderState {
    installed: Option<persistence::RootLoaderState>,
    booted: bool,
}

/// The `kernel_state.tools` bundle.
#[derive(Default)]
pub struct KernelStateBundle {
    state: Mutex<HashMap<String, LoaderState>>,
}

/// The loader's own disk dir: a `root` meta overrides the agent's address, so a
/// SESSION loader can serve a sub-namespace. Python `_root`. Unclamped — every
/// disk touch goes through [`clamp`] first.
fn loader_root(agent: &Arc<Agent>) -> PathBuf {
    persistence::loader_dir(agent)
}

/// python `_root`'s override: the loader's `root` meta when it is a non-empty
/// string (`isinstance(r, str) and r`).
fn root_meta(agent: &Agent) -> Option<String> {
    agent.meta.read().ok().and_then(|m| {
        m.get("root")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

/// `str(_root(agent))` as the loader's reflect reports it. A `root` meta is
/// reported AS GIVEN (`Path(r)`: an absolute meta stays absolute, a relative one
/// stays relative). With no meta python reports `_root_path`, the agent's
/// address RELATIVE to the running dir (`.fantastic/agents/…`); the port's
/// address is absolute, so it is shown relative to [`Kernel::workdir`] — and a
/// kernel with no running dir RAISES, as `clamp` does.
fn reported_root(agent: &Arc<Agent>, kernel: &Kernel) -> Result<String, BundleError> {
    let shown = match root_meta(agent) {
        Some(r) => PathBuf::from(r),
        None => {
            let base = kernel
                .workdir()
                .map_err(|e| -> BundleError { format!("RuntimeError: {e}").into() })?;
            // Resolved the way `Kernel::workdir` resolves the root's address.
            let at = if agent.root_path.is_absolute() {
                agent.root_path.clone()
            } else {
                std::env::current_dir()
                    .map_err(|e| -> BundleError { format!("RuntimeError: {e}").into() })?
                    .join(&agent.root_path)
            };
            at.strip_prefix(&base)
                .map_err(|_| -> BundleError {
                    format!(
                        "RuntimeError: loader address {} is outside the running dir {}",
                        at.display(),
                        base.display()
                    )
                    .into()
                })?
                .to_path_buf()
        }
    };
    Ok(py_path_str(&shown))
}

/// `Path(p).as_posix()`: no `.` segments, no trailing or doubled separator, `/`
/// between the parts on every OS; empty ⇒ `.`.
fn py_path_str(p: &Path) -> String {
    let parts: PathBuf = p
        .components()
        .filter(|c| !matches!(c, std::path::Component::CurDir))
        .collect();
    let text = fantastic_bundle::as_posix(&parts);
    if text.is_empty() {
        ".".to_string()
    } else {
        text
    }
}

/// A python exception out of a loader verb — `"<Class>: <message>"`, settled
/// `failed`. The `fs` surface hands back python's own text: an `OSError` leads
/// with `[Errno N]` (its class follows from the errno), a clamp refusal is the
/// `ValueError` `fs: root … escapes the running dir`.
fn raise_fs(msg: String) -> BundleError {
    let class = msg
        .strip_prefix("[Errno ")
        .and_then(|r| r.split(']').next())
        .and_then(|n| n.parse::<i32>().ok())
        .map_or("ValueError", fantastic_bundle::os_error_class);
    format!("{class}: {msg}").into()
}

/// python `fs.Root(agent.ctx.workdir, dir)`, resolved: `dir` clamped to the
/// running dir (relative ⇒ under it; absolute ⇒ only inside it; `..`/symlink
/// escapes refused with python's text, raised).
fn clamp(kernel: &Kernel, dir: &Path) -> Result<PathBuf, BundleError> {
    let base = kernel
        .workdir()
        .map_err(|e| -> BundleError { format!("RuntimeError: {e}").into() })?;
    fantastic_fs::resolve_root(&dir.to_string_lossy(), &base).map_err(raise_fs)
}

/// python `_read_record`: one `agent.yaml` → its mapping, or `None` when absent,
/// unreadable or not a mapping. The existence probe is OUTSIDE python's `try`,
/// so a path the clamp refuses raises.
fn read_record(root: &Path, rel: &str) -> Result<Option<Map<String, Value>>, BundleError> {
    if !fantastic_fs::exists(root, rel).map_err(raise_fs)? {
        return Ok(None);
    }
    Ok(fantastic_fs::read_text(root, rel)
        .ok()
        .and_then(|raw| persistence::parse_record(&raw)))
}

/// python `read_tree`: a `.fantastic` tree → a FLAT list of records (root +
/// descendants), `parent_id` from DISK POSITION. A MISSING OR CORRUPT ROOT RECORD
/// DOES NOT HIDE THE TREE: the children are walked under the default layout and
/// parented to [`ROOT_ID`](fantastic_kernel::bootstrap::DEFAULT_ROOT_ID).
///
/// `live` is the root's LIVE record when the caller has one (a session loader
/// serving its namespace): the namespace keeps no copy of it on disk.
fn read_tree(root: &Path, live: Option<Map<String, Value>>) -> Result<Vec<Value>, BundleError> {
    let root_rec = match live {
        Some(rec) => Some(rec),
        None => read_record(root, persistence::RECORD_FILE)?,
    };
    let Some(root_rec) = root_rec else {
        let mut records = Vec::new();
        walk(
            root,
            "agents",
            &Value::from(fantastic_kernel::bootstrap::DEFAULT_ROOT_ID),
            &mut records,
            "agents",
        )?;
        return Ok(records);
    };
    // The root record DECLARES the children-dir name (default "agents").
    let cd = root_rec
        .get("children_dir")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("agents")
        .to_string();
    let id = root_rec.get("id").cloned().unwrap_or(Value::Null);
    let mut records = vec![Value::Object(root_rec)];
    walk(root, &cd, &id, &mut records, &cd)?;
    Ok(records)
}

/// python `_walk`: absent or corrupt siblings are skipped; disk position is
/// authoritative for `parent_id`.
fn walk(
    root: &Path,
    rel_dir: &str,
    parent_id: &Value,
    out: &mut Vec<Value>,
    children_dir: &str,
) -> Result<(), BundleError> {
    if !(fantastic_fs::exists(root, rel_dir).map_err(raise_fs)?
        && fantastic_fs::is_dir(root, rel_dir).map_err(raise_fs)?)
    {
        return Ok(());
    }
    let Ok(entries) = fantastic_fs::list_dir(root, rel_dir) else {
        return Ok(());
    };
    for entry in entries {
        let name = fantastic_fs::file_name(&entry);
        let rel = format!("{rel_dir}/{name}")
            .trim_start_matches('/')
            .to_string();
        let Some(mut rec) = read_record(root, &format!("{rel}/{}", persistence::RECORD_FILE))?
        else {
            continue;
        };
        rec.insert("parent_id".to_string(), parent_id.clone());
        let id = rec.get("id").cloned().unwrap_or(Value::Null);
        out.push(Value::Object(rec));
        walk(
            root,
            &format!("{rel}/{children_dir}"),
            &id,
            out,
            children_dir,
        )?;
    }
    Ok(())
}

impl KernelStateBundle {
    async fn boot(&self, agent: &Arc<Agent>, kernel: &Arc<Kernel>) -> Result<Value, BundleError> {
        // A SESSION loader (a `root` meta pointing at a sub-namespace) writes no
        // record of its own there: its one record lives in its parent's tree and
        // `load_tree` walks the namespace from it (a copy at `<root>/agent.yaml`
        // would freeze and diverge).
        // IDEMPOTENT, as python's `_boot` is: a second boot installs nothing.
        {
            let mut st = self.state.lock().expect("kernel_state poisoned");
            let entry = st.entry(agent.id.0.clone()).or_default();
            if entry.booted {
                return Ok(Value::Null);
            }
            entry.booted = true;
        }
        let installed = persistence::boot_root_loader(kernel, agent).await;
        self.state
            .lock()
            .expect("kernel_state poisoned")
            .entry(agent.id.0.clone())
            .or_default()
            .installed = Some(installed);
        Ok(Value::Null)
    }

    /// python `_load_tree`: `read_tree(fs.Root(workdir, _root(agent)))`.
    async fn load_tree(
        &self,
        agent: &Arc<Agent>,
        kernel: &Arc<Kernel>,
    ) -> Result<Value, BundleError> {
        let root_dir = loader_root(agent);
        let live = if root_dir == agent.root_path {
            None
        } else {
            match persistence::record_map(&agent.record()) {
                Value::Object(rec) => Some(rec),
                _ => None,
            }
        };
        let root = clamp(kernel, &root_dir)?;
        Ok(json!({
            "records": read_tree(&root, live)?,
            "version": fantastic_kernel::CURRENT_VERSION,
        }))
    }

    async fn persist_record(
        &self,
        agent: &Arc<Agent>,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Value, BundleError> {
        if let Some(denied) = guard_own_tree("persist_record", agent) {
            return Ok(denied);
        }
        let Some(record) = payload
            .get("record")
            .and_then(Value::as_object)
            .filter(|r| r.get("id").is_some_and(fantastic_bundle::py_truthy))
        else {
            return Ok(fantastic_kernel::missing(
                "persist_record",
                "record (dict with id)",
            ));
        };
        if let Some(bad) = guard_id("persist_record", record.get("id")) {
            return Ok(bad);
        }
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        let parent_id = record.get("parent_id");
        let parent = parent_id.and_then(Value::as_str);
        let mut at = Places::new(agent, kernel);
        // python `_guard_parent_chain`: a parent_id that is not None must be a
        // valid id — a non-string included.
        if let Some(bad) = parent_id
            .filter(|p| !p.is_null())
            .and_then(|p| guard_id("persist_record", Some(p)))
        {
            return Ok(bad);
        }
        let path = match kernel.agents.get(&AgentId::from(id)) {
            Some(a) => a.root_path.clone(),
            None => at.resolve(id, parent)?,
        };
        write_record(&clamp(kernel, &path)?, record)?;
        Ok(json!({ "ok": true }))
    }

    async fn forget_record(
        &self,
        agent: &Arc<Agent>,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Value, BundleError> {
        if let Some(denied) = guard_own_tree("forget_record", agent) {
            return Ok(denied);
        }
        // `if not target: missing` — TRUTHINESS; a truthy non-string is then the
        // id guard's `bad_arg`, not `missing_arg`.
        let Some(raw) = payload.get("id").filter(|v| fantastic_bundle::py_truthy(v)) else {
            return Ok(fantastic_kernel::missing("forget_record", "id"));
        };
        if let Some(bad) = guard_id("forget_record", Some(raw)) {
            return Ok(bad);
        }
        let target = raw.as_str().unwrap_or("");
        let parent_id = payload.get("parent_id");
        let parent = parent_id.and_then(Value::as_str);
        let mut at = Places::new(agent, kernel);
        if let Some(bad) = parent_id
            .filter(|p| !p.is_null())
            .and_then(|p| guard_id("forget_record", Some(p)))
        {
            return Ok(bad);
        }
        let path = match kernel.agents.get(&AgentId::from(target)) {
            Some(a) => a.root_path.clone(),
            None => at.resolve(target, parent)?,
        };
        // python `rmtree`: the existence probe raises on a clamp escape; a
        // failed removal is swallowed.
        let rp = clamp(kernel, &path)?;
        if fantastic_fs::exists(&rp, "").map_err(raise_fs)? {
            let _ = fantastic_fs::remove(&rp, "", true);
        }
        Ok(json!({ "ok": true }))
    }
}

/// `guard_agent_id` on the RAW value: python refuses a non-string id with the
/// same `bad_arg` an empty one gets.
fn guard_id(verb: &str, v: Option<&Value>) -> Option<Value> {
    fantastic_kernel::refusal::guard_agent_id(verb, v.and_then(Value::as_str).unwrap_or(""))
}

/// Merge-write one record's `agent.yaml` in `root` (the agent's own dir, already
/// clamped) — python `write_record`: read any existing file, overlay the
/// record's keys (unknown fields + sidecars survive), write atomically through
/// `fs` — a failed write RAISES. A bundle's readme and docs are never copied.
fn write_record(root: &Path, record: &Map<String, Value>) -> Result<(), BundleError> {
    let mut on_disk = read_record(root, persistence::RECORD_FILE)?.unwrap_or_default();
    for (k, v) in record {
        on_disk.insert(k.clone(), v.clone());
    }
    let body = fantastic_kernel::yaml_io::dump(&Value::Object(on_disk));
    fantastic_fs::write_bytes(root, persistence::RECORD_FILE, body.as_bytes(), true, None)
        .map_err(raise_fs)?;
    Ok(())
}

/// The children-container dir name for this loader's namespace (declared config;
/// default "agents"). Python `_children_name`.
fn children_name(agent: &Arc<Agent>) -> String {
    persistence::loader_children(agent)
}

/// Where records REALLY are for this loader — python `_where`: the live tree
/// first, then its own persisted records under its root (disk position), read
/// once and only when a node is not live. That read goes through the clamp, so
/// a root outside the running dir — or a children dir or entry that resolves
/// outside the root — raises exactly where python's `_disk_parents` does.
struct Places<'a> {
    agent: &'a Arc<Agent>,
    kernel: &'a Arc<Kernel>,
    disk: Option<std::collections::HashMap<String, Option<String>>>,
}

impl<'a> Places<'a> {
    fn new(agent: &'a Arc<Agent>, kernel: &'a Arc<Kernel>) -> Self {
        Self {
            agent,
            kernel,
            disk: None,
        }
    }

    /// `(known, parent)` for one id.
    fn locate(&mut self, node: &str) -> Result<(bool, Option<String>), BundleError> {
        if let Some(live) = self.kernel.agents.get(&AgentId::from(node)) {
            return Ok((true, live.parent_id.as_ref().map(|p| p.0.clone())));
        }
        if self.disk.is_none() {
            let root = clamp(self.kernel, &loader_root(self.agent))?;
            self.disk = Some(
                persistence::disk_parents(&root, &children_name(self.agent), &self.agent.id.0)
                    .map_err(raise_fs)?,
            );
        }
        let disk = self.disk.as_ref().expect("filled above");
        Ok(match disk.get(node) {
            Some(parent) => (true, parent.clone()),
            None => (false, None),
        })
    }

    /// A record's dir under THIS loader's root — python `_resolve_path`. The
    /// chain is walked where it really is and STOPS AT THE LOADER, so a session
    /// loader's records nest under its own `root` — 2+ levels under a federated
    /// record included. A record that already exists keeps its own position; the
    /// payload's `parent_id` places only a new one.
    fn resolve(&mut self, id: &str, payload_parent: Option<&str>) -> Result<PathBuf, BundleError> {
        let mut parts: Vec<String> = vec![id.to_string()];
        let (known, mut pid) = self.locate(id)?;
        if !known {
            pid = payload_parent.map(str::to_string);
        }
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        while let Some(p) = pid {
            if p.is_empty() || p == self.agent.id.0 || !seen.insert(p.clone()) {
                break;
            }
            parts.push(p.clone());
            pid = self.locate(&p)?.1;
        }
        let children_dir = children_name(self.agent);
        let mut path = loader_root(self.agent);
        for part in parts.iter().rev() {
            path = path.join(&children_dir).join(part);
        }
        Ok(path)
    }
}

/// The ROOT loader's tree is the one this kernel HYDRATES at boot, so only the
/// operator (its own flush, the CLI, boot) writes it. An agent's record changes
/// through `update_agent`, gated like any verb — a record written here
/// by an agent would be hydrated with whatever grants it named. A session
/// loader (a remote's sub-namespace, never hydrated
/// here) keeps serving its remote. Python `_guard_own_tree`.
fn guard_own_tree(verb: &str, agent: &Arc<Agent>) -> Option<Value> {
    if agent.parent_id.is_some() || fantastic_kernel::current_origin().is_none() {
        return None;
    }
    let mut r = fantastic_kernel::refusal::refusal_hint(
        fantastic_kernel::refusal::UNAUTHORIZED,
        format!(
            "{verb}: the kernel's own tree is written only by the operator — \
             an agent's record changes through update_agent"
        ),
        "update_agent id=<id> …  (needs update_agent granted on the root)",
    );
    fantastic_kernel::refusal::with(&mut r, "side", json!("store"));
    Some(r)
}

#[async_trait]
impl Bundle for KernelStateBundle {
    // The catalog NAME is python's entry-point name, not the module path: the
    // prompt and the `bundles_next` cursor both carry it.
    fn name(&self) -> &str {
        "kernel_state"
    }

    fn verb_docs(&self) -> Value {
        verbs_view()
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    fn docs(&self) -> &'static [(&'static str, &'static str)] {
        DOCS
    }

    /// FOLDER BUNDLES before the tree loads, read through the clamp from the
    /// root's own `.fantastic` (python `main._bootstrap` → `load_bundles`).
    fn cold_boot(&self, kernel: &Kernel, fantastic_dir: &Path) {
        let Some(base) = fantastic_dir.parent() else {
            return;
        };
        if let Ok(root) = fantastic_fs::resolve_root(&fantastic_dir.to_string_lossy(), base) {
            folders::load_bundles(kernel, &root);
        }
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
            return Ok(Some(fantastic_kernel::unknown_agent(
                agent_id.as_str(),
                kernel.agents.iter().map(|e| e.key().0.clone()).collect(),
            )));
        };
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "reflect" => json!({
                "id": agent.id.0,
                "sentence": "Disk persistence/hydration root — owns .fantastic; \
                             answers load_tree / persist_record / forget_record.",
                "root": reported_root(&agent, kernel)?,
            }),
            "boot" => self.boot(&agent, kernel).await?,
            "load_tree" => self.load_tree(&agent, kernel).await?,
            "persist_record" => self.persist_record(&agent, payload, kernel).await?,
            "forget_record" => self.forget_record(&agent, payload, kernel).await?,
            "persist_blob" => persistence::persist_blob(kernel, &agent, payload).await?,
            "load_blob" => persistence::load_blob(kernel, &agent, payload).await?,
            "reload_bundle" => reload_bundle(&agent, payload, kernel).await?,
            "read_bundles" => read_bundles(&agent, payload, kernel)?,
            "export_subtree" => export_subtree_root(&agent, payload, kernel).await,
            // CODED. A reason-less error on the ROOT — the most-addressed agent
            // in the kernel — is decorated by `apply_reflect_flags` into
            // something that reads like a healthy reflect (python).
            // `verb` echoes the RAW `type` (python `verb=t`): null when absent, `5`
            // for a number — never the empty string the dispatch matched on.
            _ => {
                let t = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!(
                        "kernel_state: unknown type {}",
                        fantastic_bundle::py_repr_value(&t)
                    ),
                );
                fantastic_kernel::refusal::with(&mut r, "verb", t);
                r
            }
        };
        if reply.is_null() {
            return Ok(None);
        }
        Ok(Some(reply))
    }

    fn reflect_root_extra(
        &self,
        agent: &Arc<Agent>,
        full: bool,
        kernel: &Arc<Kernel>,
    ) -> Result<Option<Map<String, Value>>, Value> {
        let mut out = Map::new();
        // `provider` only while writes land; a failing store shows as `error`.
        let persistence = match persistence::persist_error(kernel) {
            Some(err) => json!({ "provider": null, "error": err }),
            None => json!({ "provider": persistence::find_store(kernel).map(|id| id.0) }),
        };
        out.insert("persistence".to_string(), persistence);
        out.insert(
            "guid".to_string(),
            agent
                .meta
                .read()
                .ok()
                .and_then(|m| m.get("guid").cloned())
                .unwrap_or(Value::Null),
        );
        if !full {
            out.insert("self_extension".to_string(), json!(SELF_EXTENSION_SHORT));
            return Ok(Some(out));
        }
        // A DOC THAT WILL NOT DECODE FAILS THE REFLECT. python's `_readme_index`
        // lets the UnicodeDecodeError out of `reflect_root_extra`, and the
        // substrate's merge site does not swallow it — the full root reflect
        // settles `failed` with that raise.
        let readmes = fantastic_kernel::reflect::readme_index(DOCS, "");
        out.insert("readmes".to_string(), Value::Array(readmes));
        out.insert("docs".to_string(), json!(DOCS_LINE));
        out.insert("self_extension".to_string(), json!(SELF_EXTENSION_FULL));
        Ok(Some(out))
    }

    /// LAND EVERYTHING while the provider child is still alive. The kernel awaits
    /// this BEFORE the teardown walk (python `flush_pending`).
    ///
    /// THE RECORD FLUSH LIVES HERE, not in `on_shutdown`. The record loop
    /// debounces 150 ms; a `create_agent` + `update_agent` followed straight away
    /// by a shutdown would otherwise reach its final flush mid-teardown-walk, with
    /// the store already gone, and the update would never land. The kernel already wraps
    /// this hook in `shutdown_context` — a draining store would otherwise refuse
    /// the write.
    async fn flush_pending(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let writer = {
            let st = self.state.lock().expect("kernel_state poisoned");
            st.get(&agent_id.0)
                .and_then(|s| s.installed.as_ref())
                .and_then(|i| i.writer.clone())
        };
        if let Some(w) = writer {
            w.flush(kernel).await;
        }
        Ok(())
    }

    /// Unsubscribe + stop the record loop. `flush_pending` normally ran already
    /// (the kernel calls it before the walk, while the provider is alive) and is
    /// what WRITES.
    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let installed = {
            let mut st = self.state.lock().expect("kernel_state poisoned");
            st.remove(&agent_id.0).and_then(|s| s.installed)
        };
        if let Some(i) = installed {
            persistence::shutdown_root_loader(i, kernel).await;
        }
        Ok(())
    }
}

/// `verbs` as the loader's own reflect renders it — one line per verb, python's
/// first-docstring-line rule.
/// `reload_bundle {name}` — python `kernel_state._reload_bundle`: re-read
/// `bundles/<name>/bundle.yaml` (a new folder loads) and restart every live
/// instance, each AS ITSELF (kernel bookkeeping): `shutdown` with the reason, then
/// `boot` under the fresh load. A folder that no longer loads is refused naming the
/// folder and field; its instances keep the previous load.
async fn reload_bundle(
    agent: &Arc<Agent>,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    use fantastic_kernel::refusal::{
        missing_hint, refusal, refusal_hint, INVALID_META, NOT_FOUND, NOT_HERE,
    };
    if agent.parent_id.is_some() {
        return Ok(refusal_hint(
            NOT_HERE,
            "reload_bundle: the kernel's bundles are the ROOT loader's",
            "send it to kernel",
        ));
    }
    let Some(name) = payload
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return Ok(missing_hint(
            "reload_bundle",
            "name",
            "a folder under .fantastic/bundles/",
        ));
    };
    let root = clamp(kernel, &loader_root(agent))?;
    let shown = fantastic_fs::file_name(&root);
    let rel = format!("{}/{name}", crate::folders::BUNDLES_DIR);
    if !(crate::folders::is_folder_name(name) && root.join(&rel).exists()) {
        return Ok(refusal_hint(
            NOT_FOUND,
            format!("reload_bundle: no folder {shown}/{rel}"),
            "a bundle is a folder under .fantastic/bundles/ with a bundle.yaml",
        ));
    }
    if let Some(err) = crate::folders::load_bundle(kernel, &root, name) {
        return Ok(refusal(INVALID_META, err));
    }
    let mut live: Vec<Arc<Agent>> = kernel
        .agents
        .iter()
        .filter(|e| e.value().handler_module.as_deref() == Some(name))
        .map(|e| Arc::clone(e.value()))
        .collect();
    live.sort_by(|a, b| a.id.0.cmp(&b.id.0));
    for a in &live {
        kernel
            .bookkeeping(a, json!({"type": "shutdown", "reason": "reloaded"}), None)
            .await;
        kernel.bookkeeping(a, json!({"type": "boot"}), None).await;
    }
    let ids: Vec<&str> = live.iter().map(|a| a.id.0.as_str()).collect();
    Ok(json!({"reloaded": name, "instances": ids}))
}

/// `read_bundles {name?}` — python `kernel_state._read_bundles`: the folder
/// bundles under THIS loader's root as TEXT, each `bundles/<name>/bundle.yaml`
/// raw (`error` when it has none), for a kernel that loads its own folders
/// through this loader (a page kernel's proxy_loader). Parses nothing.
fn read_bundles(
    agent: &Arc<Agent>,
    payload: &Value,
    kernel: &Arc<Kernel>,
) -> Result<Value, BundleError> {
    use crate::folders::{is_folder_name, BUNDLES_DIR, BUNDLE_FILE};
    use fantastic_kernel::refusal::{bad, refusal_hint, NOT_FOUND};
    let root = clamp(kernel, &loader_root(agent))?;
    let shown = fantastic_fs::file_name(&root);
    let names: Vec<String> = match payload.get("name") {
        None | Some(Value::Null) => {
            if fantastic_fs::exists(&root, BUNDLES_DIR).map_err(raise_fs)?
                && fantastic_fs::is_dir(&root, BUNDLES_DIR).map_err(raise_fs)?
            {
                let mut ns: Vec<String> = Vec::new();
                for p in fantastic_fs::list_dir(&root, BUNDLES_DIR).map_err(raise_fs)? {
                    let n = fantastic_fs::file_name(&p);
                    if fantastic_fs::is_dir(&root, &format!("{BUNDLES_DIR}/{n}"))
                        .map_err(raise_fs)?
                    {
                        ns.push(n);
                    }
                }
                ns.sort();
                ns
            } else {
                Vec::new()
            }
        }
        Some(Value::String(n)) if !n.is_empty() => {
            let rel = format!("{BUNDLES_DIR}/{n}");
            // a name the folder rule refuses is never joined into a path
            if !(is_folder_name(n)
                && fantastic_fs::exists(&root, &rel).map_err(raise_fs)?
                && fantastic_fs::is_dir(&root, &rel).map_err(raise_fs)?)
            {
                return Ok(refusal_hint(
                    NOT_FOUND,
                    format!("read_bundles: no folder {shown}/{rel}"),
                    format!("a bundle is a folder under {shown}/{BUNDLES_DIR}/ with a bundle.yaml"),
                ));
            }
            vec![n.clone()]
        }
        Some(_) => return Ok(bad("read_bundles", "name", "a folder name")),
    };
    let mut out = Vec::with_capacity(names.len());
    for n in names {
        let rel = format!("{BUNDLES_DIR}/{n}");
        let file = format!("{rel}/{BUNDLE_FILE}");
        let mut row = json!({"name": n, "folder": format!("{shown}/{rel}")});
        if fantastic_fs::exists(&root, &file).map_err(raise_fs)? {
            row["text"] = Value::String(fantastic_fs::read_text(&root, &file).map_err(raise_fs)?);
        } else {
            row["error"] = Value::String(format!("bundles/{n}: no bundle.yaml"));
        }
        out.push(row);
    }
    Ok(json!({"bundles": out}))
}

/// `export_subtree {agent_id, path}` — the ROOT writes an agent and everything
/// under it as a blueprint folder (`export.rs`; `user` answers the same verb).
async fn export_subtree_root(agent: &Arc<Agent>, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    if agent.parent_id.is_some() {
        return fantastic_kernel::refusal::refusal(
            fantastic_kernel::refusal::BAD_ARG,
            "kernel_state.export_subtree: the ROOT answers it",
        );
    }
    export::export_subtree(kernel, payload, "kernel_state.export_subtree").await
}

fn verbs_view() -> Value {
    json!({
        "reflect": "Identity + the disk root it owns. No args.",
        "boot": "Start the auto-flush loop unless `watch=false` (a loader serving a",
        "load_tree": "args: none. Read this loader's subtree from disk → {records, version}.",
        "persist_record": "args: record:dict (req). Merge-write one agent.yaml. Location is the",
        "forget_record": "args: id:str (req), parent_id:str (opt). rmtree the agent's dir. For a",
        "persist_blob": "args: agent_id:str (req), name:str (req), content:str (req). Persist an",
        "load_blob": "args: agent_id:str (req), name:str (req). Read an agent's sidecar THROUGH the",
        "reload_bundle": "args: name:str (req). Re-read `bundles/<name>/bundle.yaml` (a new folder loads) and restart every live instance of it, abruptly — an in-flight call is answered with an error naming the reload. Returns {reloaded, instances}. The root answers it.",
        "export_subtree": "args: agent_id:str (req), path:str (req) — writes the agent and everything under it as a blueprint folder; outside references become ports; no keys, no absolute paths, no rows. path: a new or empty folder (relative to the running dir; no `..`). Reply {exported, path, ports, files}.",
        "read_bundles": "args: name:str?. The folder bundles under this loader's root AS TEXT — {bundles:[{name, folder, text?, error?}]}, each `bundles/<name>/bundle.yaml` raw (`error` when it has none); `name` reads one. For a kernel that loads its own folders through this loader (a page kernel's proxy_loader); this loader parses nothing.",
    })
}

#[cfg(test)]
mod tests;
