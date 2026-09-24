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
//!   persist_record {record}   -> {ok}                 write one agent.json
//!   forget_record {id}        -> {ok}                 remove one agent's dir
//!   persist_blob / load_blob                          the sidecar byte channel
//! ```
//!
//! The ROOT loader also owns the kernel-wide bookkeeping python's `_boot`
//! installs: the execution-ledger rehydrate + flusher, the message pump and the
//! record `persist_writer`.

#![deny(missing_docs)]

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{persistence, AgentId, Kernel};

/// The `handler_module` this bundle answers to — python's package path.
pub const HANDLER_MODULE: &str = "kernel_state.tools";

/// The SHORT orientation readme auto-seeded into `.fantastic/readme.md` on boot.
/// The full manual is the keyed [`DOCS`] tree — `reflect readme=index` / `=<key>`.
pub const README: &str = include_str!("readme.md");

/// The manual, split into keyed topic docs (nested dirs = tiers via a `/` in the
/// key). Seeded into `.fantastic/docs/<key>.md`; `reflect readme=index` lists them
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
        "frontend/panel-recipe",
        include_str!("docs/frontend/panel-recipe.md"),
    ),
    ("frontend/routing", include_str!("docs/frontend/routing.md")),
    ("terminal", include_str!("docs/terminal.md")),
    ("daemon", include_str!("docs/daemon.md")),
];

/// Seed `<workdir>/.fantastic/readme.md` from [`README`] AND the [`DOCS`] tree
/// into `<workdir>/.fantastic/docs/` if missing. Idempotent per file: preserves
/// any user-edited content on subsequent calls.
///
/// The COLD half of python's `_seed_readme` (the bootstrap writes the root
/// record before any provider can exist); the warm half rides the store in
/// [`persistence::install_persist_writer`].
pub fn seed_root_readme(workdir: &Path) -> io::Result<()> {
    let fdir = workdir.join(".fantastic");
    let dest = fdir.join("readme.md");
    if !dest.exists() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, README)?;
    }
    let docs_dir = fdir.join("docs");
    for (key, body) in DOCS {
        let path = docs_dir.join(format!("{key}.md"));
        if path.exists() {
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, body)?;
    }
    Ok(())
}

/// The SHORT `self_extension` a DISTILLED root reflect carries — python
/// `reflect_root_extra`'s non-full branch, verbatim.
const SELF_EXTENSION_SHORT: &str = "A DURABLE PROGRAM is a FILE plus a command — no `code` field. (1) WRITE \
it to a file: <type>write_stream</type> <path>…</path> <truncate>true</truncate> <bytes>…</bytes> — `bytes` is \
VERBATIM (the program between `<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped). \
(2) RUN it on the COMMAND runner: <type>start</type> <command>…</command> <args><item>…</item></args>, gated by its \
`allowed` regexes (reflect it). (3) Re-run the same command to keep it \
current. A COMPUTE worker is an `exec_bridge.tools` agent + a program \
file — NOT a new handler_module. Create it as YOUR child; to wire it \
to a SIBLING (`totals`) grant `can_call:{totals}` AND `update_agent \
id='totals'` to add it to `totals.callable_by`. Read <type>reflect</type> \
<readme>self-extension</readme> — send it to 'kernel' — for the fd wire.";

/// The one-liner teaching keyed doc fetch — python `reflect_root_extra`'s `docs`.
const DOCS_LINE: &str = "This kernel's manual is a keyed doc tree. To read it, send 'kernel' \
a reflect with a string `readme` field: <type>reflect</type> \
<readme>index</readme> lists every topic with its ~token cost; \
<readme>key</readme> opens one (e.g. self-extension, streams); \
<readme>true</readme> is the short orientation. The field is named exactly \
`readme` (a string) - not 'readme_key'. Read only what a task needs.";

/// The LONG `self_extension` a `detail:"full"` root reflect carries — python's
/// full branch, verbatim (it has no fd-wire tail sentence).
const SELF_EXTENSION_FULL: &str = "A DURABLE PROGRAM is a FILE you write plus a command that runs it — \
not a `code`-carrying agent. There is no `code` field: a program used \
to be embedded as a JSON string, which is exactly how a call gets lost \
to escaping. Assemble these THREE pieces and you have built something \
that persists and re-runs, with no `code` field at all:\n\
1. WRITE THE PROGRAM TO A FILE: never carry file contents in a JSON \
string. Use the file leg's STREAM verbs — <type>write_stream</type> <path>…</path> \
<truncate>true</truncate> <bytes>…</bytes> to write, <type>read_stream</type> <path>…</path> <offset>0</offset> to \
read, one raw chunk at a time. `bytes` is VERBATIM: the program between \
`<bytes>` and `</bytes>` exactly as written, real newlines, nothing escaped. That is \
the only byte path; the string read/write verbs are gone.\n\
2. RUN IT: the executor is a general-purpose COMMAND runner. Reflect \
it, read its `allowed` regexes, then send \
<type>start</type> <command>path</command> <args><item>…</item></args>; poll <type>status</type>. \
It runs commands, not inline programs.\n\
3. RE-RUN: the file is durable on disk, so the SAME `start` command run \
again later re-executes the SAME program against whatever the inputs \
hold THEN — that is how a thing 'keeps itself up to date'.\n\
A WORKER THAT COMPUTES is an `exec_bridge.tools` agent (the EXISTING \
command runner) that owns a program file — you never author a new \
handler_module (you cannot add modules to the catalog and do not need \
to). 'A specialist with child workers' = a parent agent whose children \
are exec_bridge runners, each with its own program file.\n\
WIRING TO A SIBLING: you cannot raise your OWN authority, but if you \
hold `update_agent` (e.g. on the root) you may open ANY agent's ingress \
— including a sibling like `totals`/`readings` that is not your child. \
To let your worker call `totals`: grant the worker `can_call:{totals}` \
AND `update_agent id='totals'` to add the worker to `totals.callable_by` \
(send the WHOLE map — update replaces, not merges).\n\
A SPAWNED PROGRAM reaches agents over its control fd — a TWO-WAY socket \
(FANTASTIC_CTRL_FD): write {op:'send', rid, target, payload}; the reply \
comes back on the SAME fd as {op:'reply', rid, data}. It is not inline \
— read frames until one carries your `rid`, skipping event/inbox frames.\n\
COMPOSE the agents you were given for everything else (`list_agents`, \
reflect each, send). CREATE/DELETE plain agents (create_agent with a \
handler_module and grants, delete_agent) work as before.";

/// Per-loader process state — what `boot` installed, so a second `boot` is a
/// no-op and `on_shutdown` can tear it down. Python keeps these on the agent
/// object (`_fs_flush_loop`, `_message_pump`).
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
/// SESSION loader can serve a sub-namespace. Python `_root`.
fn loader_root(agent: &Arc<Agent>) -> std::path::PathBuf {
    agent
        .meta
        .read()
        .ok()
        .and_then(|m| {
            m.get("root")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
        })
        .unwrap_or_else(|| agent.root_path.clone())
}

impl KernelStateBundle {
    async fn boot(&self, agent: &Arc<Agent>, kernel: &Arc<Kernel>) -> Value {
        // IDEMPOTENT, as python's `_boot` is: a second boot installs nothing.
        {
            let mut st = self.state.lock().expect("kernel_state poisoned");
            let entry = st.entry(agent.id.0.clone()).or_default();
            if entry.booted {
                return Value::Null;
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
        Value::Null
    }

    async fn load_tree(&self, agent: &Arc<Agent>) -> Value {
        json!({
            "records": persistence::read_tree(&loader_root(agent)),
            "version": fantastic_kernel::CURRENT_VERSION,
        })
    }

    async fn persist_record(
        &self,
        agent: &Arc<Agent>,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Value {
        let Some(record) = payload
            .get("record")
            .and_then(Value::as_object)
            .filter(|r| r.get("id").is_some_and(fantastic_bundle::py_truthy))
        else {
            return fantastic_kernel::missing("persist_record", "record (dict with id)");
        };
        let id = record.get("id").and_then(Value::as_str).unwrap_or("");
        if let Some(bad) = fantastic_kernel::refusal::guard_agent_id("persist_record", id) {
            return bad;
        }
        let parent_id = record.get("parent_id").and_then(Value::as_str);
        if let Some(denied) = guard_record_owner("persist_record", id, parent_id, kernel) {
            return denied;
        }
        if let Some(pid) = parent_id {
            if let Some(bad) = fantastic_kernel::refusal::guard_agent_id("persist_record", pid) {
                return bad;
            }
        }
        let path = match kernel.agents.get(&AgentId::from(id)) {
            Some(a) => a.root_path.clone(),
            None => resolve_under(agent, kernel, id, parent_id),
        };
        write_record(&path, record);
        json!({ "ok": true })
    }

    async fn forget_record(
        &self,
        agent: &Arc<Agent>,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Value {
        let Some(target) = payload
            .get("id")
            .filter(|v| fantastic_bundle::py_truthy(v))
            .and_then(Value::as_str)
        else {
            return fantastic_kernel::missing("forget_record", "id");
        };
        if let Some(bad) = fantastic_kernel::refusal::guard_agent_id("forget_record", target) {
            return bad;
        }
        let parent_id = payload.get("parent_id").and_then(Value::as_str);
        if let Some(denied) = guard_record_owner("forget_record", target, parent_id, kernel) {
            return denied;
        }
        if let Some(pid) = parent_id {
            if let Some(bad) = fantastic_kernel::refusal::guard_agent_id("forget_record", pid) {
                return bad;
            }
        }
        let path = match kernel.agents.get(&AgentId::from(target)) {
            Some(a) => a.root_path.clone(),
            None => resolve_under(agent, kernel, target, parent_id),
        };
        let _ = fs::remove_dir_all(&path);
        json!({ "ok": true })
    }
}

/// Merge-write one record's `agent.json` at `root_path` (the agent's own dir) —
/// python `write_record`. Reads any existing file and overlays only the
/// kernel-managed keys, preserving unknown fields + sidecars.
fn write_record(root_path: &Path, record: &Map<String, Value>) {
    let file = root_path.join("agent.json");
    let mut on_disk: Map<String, Value> = fs::read_to_string(&file)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| match v {
            Value::Object(m) => Some(m),
            _ => None,
        })
        .unwrap_or_default();
    for (k, v) in record {
        on_disk.insert(k.clone(), v.clone());
    }
    let _ = fs::create_dir_all(root_path);
    let body = fantastic_bundle::py_json_dumps(&Value::Object(on_disk), false, Some(2));
    let _ = fs::write(&file, body);
}

/// An absolute dir for a record under THIS loader's root (a session loader's
/// `root` meta moves the whole namespace). Python `_resolve_path`.
fn resolve_under(
    agent: &Arc<Agent>,
    kernel: &Arc<Kernel>,
    id: &str,
    parent_id: Option<&str>,
) -> std::path::PathBuf {
    let base = loader_root(agent);
    let under_root = persistence::resolve_record_path(kernel, id, parent_id);
    match kernel.root() {
        Some(root) if base != root.root_path => {
            let rel = persistence::store_reldir(&root.root_path, &under_root);
            if rel.is_empty() {
                base
            } else {
                base.join(rel)
            }
        }
        _ => under_root,
    }
}

/// Refuse a RECORD verb that names an agent outside the caller's own subtree —
/// python `_guard_record_owner`. An OPERATOR (`current_origin()` is `None`) is
/// the bookkeeping these verbs exist for and stays ungated.
fn guard_record_owner(
    verb: &str,
    record_id: &str,
    parent_id: Option<&str>,
    kernel: &Arc<Kernel>,
) -> Option<Value> {
    let caller = fantastic_kernel::current_origin()?;
    if caller.0 == record_id {
        return None;
    }
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut node: Option<String> = parent_id
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .or_else(|| {
            kernel
                .agents
                .get(&AgentId::from(record_id))
                .and_then(|a| a.parent_id.as_ref().map(|p| p.0.clone()))
        });
    while let Some(n) = node {
        if n == caller.0 {
            return None;
        }
        if !seen.insert(n.clone()) {
            break;
        }
        node = kernel
            .agents
            .get(&AgentId::from(n.as_str()))
            .and_then(|a| a.parent_id.as_ref().map(|p| p.0.clone()));
    }
    Some(fantastic_kernel::refusal(
        fantastic_kernel::refusal::UNAUTHORIZED,
        format!(
            "{} may not {verb} {} — a record verb is scoped to the caller's own \
             subtree; only an operator may name one outside it",
            fantastic_bundle::py_repr(&caller.0),
            fantastic_bundle::py_repr(record_id)
        ),
    ))
}

#[async_trait]
impl Bundle for KernelStateBundle {
    // The catalog NAME is python's entry-point name, not the module path: the
    // prompt and the `bundles_next` cursor both carry it.
    fn name(&self) -> &str {
        "kernel_state"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    fn docs(&self) -> &'static [(&'static str, &'static str)] {
        DOCS
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
                "root": loader_root(&agent).to_string_lossy(),
                "verbs": verbs_view(),
            }),
            "boot" => self.boot(&agent, kernel).await,
            "load_tree" => self.load_tree(&agent).await,
            "persist_record" => self.persist_record(&agent, payload, kernel).await,
            "forget_record" => self.forget_record(&agent, payload, kernel).await,
            "persist_blob" => persistence::persist_blob(kernel, payload).await,
            "load_blob" => persistence::load_blob(kernel, payload).await,
            // CODED. A reason-less error on the ROOT — the most-addressed agent
            // in the kernel — is decorated by `apply_reflect_flags` into
            // something that reads like a healthy reflect (python 2026-09-02).
            other => {
                let t = payload.get("type").cloned().unwrap_or(Value::Null);
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!(
                        "kernel_state: unknown type {}",
                        fantastic_bundle::py_repr_value(&t)
                    ),
                );
                fantastic_kernel::refusal::with(&mut r, "verb", json!(other));
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
    ) -> Option<Map<String, Value>> {
        let mut out = Map::new();
        out.insert(
            "persistence".to_string(),
            json!({ "provider": persistence::find_store(kernel).map(|id| id.0) }),
        );
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
            return Some(out);
        }
        // A DOC THAT WILL NOT DECODE KILLS THE WHOLE EXTRA, not just `readmes`.
        // python's `_readme_index` lets the UnicodeDecodeError out of
        // `reflect_root_extra`, and the substrate's merge site swallows it
        // (`except Exception: pass`) — so the full root reflect carries no
        // `persistence`, `guid`, `readmes`, `docs` or `self_extension` at all.
        // Degrading to an empty index published a frame python does not.
        let Ok(readmes) =
            fantastic_kernel::reflect::readme_index(&agent.root_path.join("docs"), "")
        else {
            return None;
        };
        out.insert("readmes".to_string(), Value::Array(readmes));
        out.insert("docs".to_string(), json!(DOCS_LINE));
        out.insert("self_extension".to_string(), json!(SELF_EXTENSION_FULL));
        Some(out)
    }

    /// LAND EVERYTHING while the provider child is still alive. The kernel awaits
    /// this BEFORE the teardown walk (python `flush_pending`).
    ///
    /// THE RECORD FLUSH LIVES HERE, not in `on_shutdown`. The record loop
    /// debounces 150 ms; a `create_agent` + `update_agent` followed straight away
    /// by a shutdown used to reach its final flush mid-teardown-walk, with the
    /// store already gone, and the update never landed. Records first: their
    /// writes mint rows, and the pump's own window has to close after them. The
    /// kernel already wraps this hook in `shutdown_context` — a draining store
    /// would otherwise refuse the write.
    async fn flush_pending(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        let (writer, pump) = {
            let mut st = self.state.lock().expect("kernel_state poisoned");
            let installed = st.get_mut(&agent_id.0).and_then(|s| s.installed.as_mut());
            match installed {
                Some(i) => (i.writer.clone(), i.pump.take()),
                None => (None, None),
            }
        };
        if let Some(w) = writer {
            w.flush(kernel).await;
        }
        if let Some(p) = pump {
            p.stop(kernel).await;
        }
        Ok(())
    }

    /// Unsubscribe + stop the loops. `flush_pending` normally ran already (the
    /// kernel calls it before the walk, while the provider is alive) and is what
    /// WRITES; a path that skipped it (a bare shutdown) still lands the last
    /// message window here, through the pump's own catch-up.
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
fn verbs_view() -> Value {
    json!({
        "reflect": "Identity + the disk root it owns. No args.",
        "boot": "Start the auto-flush loop unless `watch=false` (a loader serving a",
        "load_tree": "args: none. Read this loader's subtree from disk → {records, version}.",
        "persist_record": "args: record:dict (req). Merge-write one agent.json. Location is the",
        "forget_record": "args: id:str (req), parent_id:str (opt). rmtree the agent's dir. For a",
        "persist_blob": "args: agent_id:str (req), name:str (req), content:str (req). Persist an",
        "load_blob": "args: agent_id:str (req), name:str (req). Read an agent's sidecar THROUGH the",
    })
}

#[cfg(test)]
mod tests;
