//! BOOTING AND DRIVING THE KERNEL IN THIS PROCESS.
//!
//! `bootstrap` is synchronous and takes the bundle registry by value, so the
//! registry is built first and completely. What comes back is an `Arc<Kernel>`;
//! from there every operation is a direct call.
//!
//! THE START IS THE KERNEL'S: `Kernel::start` (the `kernel_start` verb) boots the
//! root — which mints the guid and installs the record writer — then the MEMORY
//! STORE the root names in `memory_store_id`, probes that it serves the operator,
//! and starts flushing execution rows into it. Without a store it refuses, loud.
//! Everyone else is booted by this shell afterwards, in registration order, so a
//! persisted `web` host is booted only by a mode that serves the browser.
//!
//! THE KERNEL KEEPS NO CONVERSATION. An AI agent's rows live in the memory store
//! under its own key; the kernel's execution rows (call / settle / task) live
//! there under the operator's key once flushed, and in RAM until then. Reading
//! an agent's rows is therefore a `read` on the store, made as the operator — the
//! store's admin.
//!
//! IDENTITY COMES ONLY FROM DISPATCH. A send this shell makes outside every
//! handler is the operator's (ungated); an ACT by the human goes through
//! `user.forward`, so it is `user`'s call, gated by `user`'s grants. A send made
//! inside a bundle's handler is that agent's.

use std::sync::Arc;

use fantastic_kernel::{
    bootstrap::{self, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel, Outcome,
};
use serde_json::{json, Value};

use crate::env::{ClientCfg, Paths};
use crate::kern::model::{AgentId as MyId, KernelErr, Record, Row, Task, TreeNode};

/// The ledger cap: calls kept before the kernel prunes its store (owner 2026-10-09).
pub const LEDGER_CAP: usize = 100_000;

/// The one shared persistent AI store's id (the blueprint's).
pub const AI_STORE: &str = "memory";

/// The kernel, plus who this terminal is when it speaks.
pub struct Host {
    pub kernel: Arc<Kernel>,
    pub cfg: ClientCfg,
    /// The root's id as the kernel reports it — `core` on rust, and DISCOVERED
    /// rather than assumed, because the same shell should not lie on a runtime
    /// whose root is named something else.
    pub root: MyId,
}

/// Every bundle this shell's roster can name. Deliberately short: the set of
/// agents is the user's to grow, and registering a bundle nobody declared only
/// widens what a stray record could become.
fn registry(cwd: &std::path::Path) -> BundleRegistry {
    let mut reg = BundleRegistry::new();
    // THE ROOT. Its handler is the durable-state loader; see the module header.
    reg.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
    // The one disk surface. Clamped to its declared root; a path outside is
    // refused rather than normalised.
    reg.register("file_bridge.tools", fantastic_file::FileBundle::default());
    // THE TWO MEMORY STORES (owner 2026-10-09): the KERNEL'S execution rows
    // (call / settle / task / ack / flush) in RAM — the root's `kernel_memory`,
    // a ram_store, so a keystroke never touches disk — and every AI agent's
    // conversation in ONE shared SQLite file (`memory`) that survives the process.
    reg.register(fantastic_ram_store::HANDLER_MODULE, fantastic_ram_store::RamStoreBundle::default());
    reg.register(
        fantastic_sqlite_store::HANDLER_MODULE,
        fantastic_sqlite_store::SqliteStoreBundle::default(),
    );
    // The shell. `allowed` is a list of regexes fullmatched against the joined
    // command line.
    reg.register(
        fantastic_exec_bridge::HANDLER_MODULE,
        fantastic_exec_bridge::ExecBridgeBundle,
    );
    // THE HUMAN, as an agent. This is the id the terminal acts as, so every act
    // carries a ledger row naming a person instead of an operator injection.
    reg.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());
    // THE MCP LEG — a bundle from outside the kernel tree. Registered so a
    // persisted `mcp` record hydrates into a live agent on every run; the
    // record itself is only ever created by `fantastic mcp`, never by the
    // blueprint.
    reg.register(fantastic_web_mcp::HANDLER_MODULE, fantastic_web_mcp::WebMcpBundle);
    // THE AI AGENT'S SAVED SET-UPS. Its directory and the ONE caller it serves (the
    // human) are fixed here, by the binary — never by a record (`kern/saved`).
    reg.register(
        fantastic_user_home_store::HANDLER_MODULE,
        fantastic_user_home_store::UserHomeStore::new(
            fantastic_user_home_store::home_dir_from_env(),
            vec![crate::env::HUMAN.to_string()],
        ),
    );
    // THE USER STORE: the keys, set-ups and stored fantastic locations in the
    // user's home; "here" is this project's dir, fixed by the binary (`kern/user_store`).
    reg.register(
        fantastic_user_home_store::user_store::HANDLER_MODULE,
        fantastic_user_home_store::user_store::UserStore::new(
            fantastic_user_home_store::home_dir_from_env(),
            std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf()),
        ),
    );
    // THE CANVAS: the HTTP host, its embedded page and its socket leg. Only ever
    // created by `fantastic canvas`; registered so they rehydrate.
    reg.register(fantastic_web::HANDLER_MODULE, fantastic_web::WebBundle);
    reg.register(fantastic_canvas_page::HANDLER_MODULE, fantastic_canvas_page::CanvasPageBundle);
    reg.register(fantastic_web_ws::HANDLER_MODULE, fantastic_web_ws::WebWsBundle);
    // THE BROWSER'S SET-UP PAGE, host side. Registered so the page can create
    // it; it exists only while that page is open (`kern/setup_agent`).
    reg.register(crate::kern::setup_agent::HANDLER_MODULE, crate::kern::setup_agent::SetupBundle);
    // THE CANVAS, host side: its node `canvas_root` — creates and deletes its
    // the cells' backends and spawns AI cells from the user's
    // saved set-ups (`kern/backend`).
    reg.register(crate::kern::backend::HANDLER_MODULE, crate::kern::backend::BackendBundle);
    // KEYS: the credential store (`kern/keys`) — every provider key, by ref.
    reg.register(fantastic_credential_store::HANDLER_MODULE, fantastic_credential_store::CredentialStoreBundle::default());
    // Apple's on-device model: linked by `--features foundation-models`.
    #[cfg(feature = "foundation-models")]
    reg.register(
        fantastic_foundation_models_backend::HANDLER_MODULE,
        fantastic_foundation_models_backend::FoundationModelsBackendBundle::system(),
    );
    // FOLDER BUNDLES' python base (`py_agent`): a `.fantastic/bundles/<name>/`
    // with `base: py_agent` runs its program as the agent's implementation
    // (`kern/bundle_watch` reloads it when its folder changes).
    reg.register(fantastic_py_agent::HANDLER_MODULE, fantastic_py_agent::PyAgentBundle);
    reg.register(fantastic_mcp_client::HANDLER_MODULE, fantastic_mcp_client::McpClientBundle::default());
    // The vLLM backend: linked by `--features vllm`.
    #[cfg(feature = "vllm")]
    reg.register(fantastic_vllm_backend::HANDLER_MODULE, fantastic_vllm_backend::VllmBundle);
    // The Anthropic Messages backend: linked by `--features anthropic`.
    #[cfg(feature = "anthropic")]
    reg.register(fantastic_anthropic_backend::HANDLER_MODULE, fantastic_anthropic_backend::AnthropicBundle);
    // The Ollama backend: linked by `--features ollama`.
    #[cfg(feature = "ollama")]
    reg.register(fantastic_ollama_backend::HANDLER_MODULE, fantastic_ollama_backend::OllamaBackendBundle);
    // The OpenAI Responses backend: linked by `--features responses`.
    #[cfg(feature = "responses")]
    reg.register(
        fantastic_responses_backend::HANDLER_MODULE,
        fantastic_responses_backend::ResponsesBundle,
    );
    reg
}

impl Host {
    /// Boot over `paths.cwd`. Acquires the directory lock, so a second shell in
    /// the same directory fails here rather than corrupting the store.
    ///
    /// `serve_web`: whether a persisted `web` host starts listening. Only a mode
    /// that serves the canvas opens the port; `fantastic` and `fantastic
    /// mcp` leave a `web` record on disk untouched (not booted).
    /// `web_port`: `--port` — a recorded `web` on another port is set to it
    /// BEFORE its listener boots (nothing runs here yet, so it is the user's to
    /// overwrite).
    pub async fn boot(paths: &Paths, serve_web: bool, web_port: Option<u16>) -> Result<Self, KernelErr> {
        let booted = bootstrap::bootstrap(registry(&paths.cwd), BootstrapOptions::daemon(&paths.cwd))
            .map_err(|e| KernelErr::Boot(format!("{e}")))?;
        let kernel = Arc::clone(&booted.kernel);
        // THE LEDGER CAP (owner 2026-10-09): 100 000 calls; past it the kernel
        // prunes its store to 90% — oldest settled first, unnamed before named,
        // never in-flight work or unacked dead letters. In-proc, the cap is
        // this shell's to set (the headless kernel binary parses `--ledger-cap`).
        kernel.set_ledger_cap(Some(LEDGER_CAP));
        let root = kernel
            .root()
            .map(|r| r.id.0.clone())
            .ok_or_else(|| KernelErr::Boot("the kernel booted with no root".into()))?;

        // The blueprint's root fields, each only while the root lacks it.
        let mut patch = serde_json::Map::new();
        {
            let r = kernel.root().expect("root checked above");
            let meta = r.meta.read().expect("root meta poisoned");
            for (k, v) in crate::boot::seed::root_fields().map_err(KernelErr::Boot)? {
                if !meta.contains_key(&k) {
                    patch.insert(k, v);
                }
            }
        }
        if !patch.is_empty() {
            let mut p = json!({"type": "update_agent", "id": root});
            for (k, v) in patch {
                p[k] = v;
            }
            let r = kernel
                .send(vec![Item::new(AgentId::from("kernel"), p)])
                .await
                .remove(0)
                .result;
            if let Some(e) = r.get("error").and_then(Value::as_str) {
                return Err(KernelErr::Boot(format!("could not set the root's fields: {e}")));
            }
        }

        // THE KERNEL'S START: root, then its memory store, then the flusher.
        let started = kernel.start(false).await;
        if started.get("started") != Some(&json!(true)) {
            let why = started.get("error").and_then(Value::as_str).unwrap_or("kernel_start refused");
            let hint = started.get("hint").and_then(Value::as_str).map(|h| format!(" — {h}")).unwrap_or_default();
            return Err(KernelErr::Boot(format!("{why}{hint}")));
        }
        let done: Vec<String> = serde_json::from_value(started["booted"].clone()).unwrap_or_default();

        // Everyone else, in registration order. A failure is logged and
        // skipped: one bundle refusing must not take the shell down with it.
        let mut notes = Vec::new();
        for id in kernel.agent_ids() {
            if done.contains(&id.0) {
                continue;
            }
            let is_web = kernel.agents.get(&id).is_some_and(|a| {
                a.value().record().handler_module.as_deref() == Some(fantastic_web::HANDLER_MODULE)
            });
            if is_web && !serve_web {
                continue;
            }
            if let (true, Some(p)) = (is_web, web_port) {
                let had = kernel.agents.get(&id).and_then(|a| a.value().record().meta.get("port").and_then(Value::as_u64));
                if had != Some(u64::from(p)) {
                    let r = kernel
                        .send(vec![Item::new(AgentId::from("kernel"), json!({"type": "update_agent", "id": id.0, "port": p}))])
                        .await
                        .remove(0)
                        .result;
                    if let Some(err) = r.get("error").and_then(Value::as_str) {
                        notes.push(format!("{id}: --port {p} not set: {err}"));
                    }
                }
            }
            let reply = kernel
                .send(vec![Item::new(id.clone(), json!({"type": "boot"}))])
                .await
                .remove(0)
                .result;
            if let Some(err) = reply.get("error").and_then(Value::as_str) {
                notes.push(format!("{id}: boot failed: {err}"));
            }
        }

        let host = Self {
            kernel,
            cfg: ClientCfg::here(),
            root,
        };
        // GOD MODE BY GRANTS, in every mode (a strictly single-user dev tool):
        // `user` and the other holders reach every agent, both edges, before the
        // shell acts through `user` (`kern/reach`). The WATCH that grants each
        // agent added later starts only once the boot's own grant writes are done
        // (the caller's `reach::start`): two writers of one record lose entries.
        if let Err(e) = crate::kern::reach::grant_all(&host).await {
            notes.push(e);
        }
        // The saved set-up store's agent, and the human's grant onto it — re-set on
        // every boot, whatever a record on disk says.
        if let Err(e) = crate::kern::saved::ensure(&host).await {
            notes.push(e);
        }
        if let Err(e) = crate::kern::user_store::ensure(&host).await {
            notes.push(e);
        }
        // THE PROJECT'S SYSTEM BLOCK: `.fantastic/prompt.md`, seeded, read by
        // every AI through its bridge (`kern/prompt`)
        match crate::kern::prompt::ensure(&host, &paths.cwd).await {
            Ok(()) => {
                if let Err(e) = crate::kern::reach::grant_all(&host).await {
                    notes.push(e);
                }
                notes.extend(crate::kern::prompt::stamp_all(&host).await);
            }
            Err(e) => notes.push(e),
        }
        // THE PROJECT'S `.fantastic/.mcp.json`: kept by the `mcp` leg, through its bridge
        // (`kern/mcp_config`) — right before any coding CLI connects
        if let Err(e) = crate::kern::mcp_config::ensure(&host).await {
            notes.push(e);
        }
        // the orientation a coding CLI opened here reads (`kern/orient`)
        if let Err(e) = crate::kern::orient::seed(&paths.cwd) {
            notes.push(e);
        }
        // A browser set-up agent lives only while its page is open: never hydrated.
        crate::kern::setup_agent::sweep(&host).await;
        for n in notes {
            tracing::warn!("{n}");
        }
        // The credential store, as the user's key mode says (none until chosen).
        if let Err(e) = crate::kern::keys::ensure_store(&host).await {
            tracing::warn!("{e}");
        }
        Ok(host)
    }

    /// The KERNEL'S store — the one the root names in `memory_store_id` (a
    /// ram_store): its execution rows live there, for this run only.
    pub fn kernel_store(&self) -> Result<String, String> {
        self.kernel
            .root()
            .and_then(|r| r.meta.read().ok()?.get("memory_store_id")?.as_str().map(str::to_string))
            .ok_or_else(|| "the root names no `memory_store_id`".to_string())
    }

    /// THE AI STORE — the one shared persistent store every AI agent this shell
    /// creates keeps its conversation in (and the kept dialogs): `memory`, the
    /// blueprint's sqlite store. Rows are keyed `<guid>|<agent>`, so agents
    /// never mix.
    pub fn ai_store(&self) -> Result<String, String> {
        if self.kernel.agents.contains_key(&AgentId::from(AI_STORE)) {
            Ok(AI_STORE.to_string())
        } else {
            Err(format!("the AI store `{AI_STORE}` is not in the tree"))
        }
    }

    /// The AI agents in the tree: every non-root agent whose record names a
    /// memory store (`memory_store_id`) — the field that makes one.
    pub fn ai_agents(&self) -> Vec<String> {
        let root = self.kernel.root().map(|r| r.id.clone());
        self.kernel
            .agents
            .iter()
            .filter(|e| Some(e.key()) != root.as_ref())
            .filter(|e| e.value().record().meta.get("memory_store_id").and_then(Value::as_str).is_some())
            .map(|e| e.key().0.clone())
            .collect()
    }

    /// Admit AI agent `ai` to the memory store — the store's side of the gate,
    /// merged into its `callable_by` — and return the store's id, for the AI's
    /// own record to name in `memory_store_id` and grant in `can_call`. Done
    /// BEFORE the AI is created: its first boot checks the store it names.
    pub async fn admit_to_memory(&self, ai: &str) -> Result<String, String> {
        let store = self.ai_store()?;
        let mut cb = self
            .kernel
            .agents
            .get(&AgentId::from(store.as_str()))
            .and_then(|a| a.value().record().meta.get("callable_by").and_then(Value::as_object).cloned())
            .ok_or_else(|| format!("the memory store `{store}` is not in the tree"))?;
        cb.insert(ai.into(), json!("*"));
        // as `user`, the store's parent: grants come from above
        let r = self
            .act("kernel", json!({"type": "update_agent", "id": store, "callable_by": cb}))
            .await
            .result;
        match r.get("error").and_then(Value::as_str) {
            Some(e) => Err(format!("could not admit `{ai}` to `{store}`: {e}")),
            None => Ok(store),
        }
    }

    /// Shut down IN THE KERNEL'S ORDER, on the runtime that booted it:
    /// `Kernel::shutdown` first (drain, each agent's `shutdown` verb, cancel what
    /// remains, the last execution rows flushed into the memory store, the
    /// records written, the teardown walk), and only then the lock release.
    /// Releasing the lock first used to lose the last execution.
    pub async fn shutdown(&self, paths: &Paths) {
        // The kernel lands every record and every message itself
        // (`flush_pending`) before its teardown walk; the lock goes last.
        self.kernel.shutdown().await;
        // Every row has landed: the SQLite stores fold their WAL into the file
        // and close, so a stopped kernel leaves only `memory.db`.
        for e in fantastic_sqlite_store::close_all(&self.kernel) {
            tracing::warn!("shutdown: {e}");
        }
        if let Err(e) = bootstrap::shutdown(&paths.cwd) {
            tracing::warn!("shutdown: {e}");
        }
    }

    // ── acting ───────────────────────────────────────────────────────────

    /// Dispatch AS THE HUMAN: `user.forward {target, payload}` — the send is
    /// made inside `user`'s own handler, so it is `user`'s call, gated by its
    /// grants. The outcome is the forwarded call's reply.
    pub async fn act(&self, target: &str, payload: Value) -> Outcome {
        let k = Arc::clone(&self.kernel);
        let item = Item::new(
            AgentId::from(self.cfg.user.as_str()),
            json!({"type": "forward", "target": target, "payload": payload}),
        );
        k.send(vec![item]).await.remove(0)
    }

    /// A plain send: outside every handler it is the OPERATOR's (ungated) — the
    /// shell's own housekeeping; inside a bundle's handler it is that agent's.
    pub async fn operator(&self, target: &str, payload: Value) -> Outcome {
        let k = Arc::clone(&self.kernel);
        let item = Item::new(AgentId::from(target), payload);
        k.send(vec![item]).await.remove(0)
    }

    /// An outcome as this shell reads it: the reply, or the refusal as data.
    pub fn read(out: Outcome) -> Result<Value, KernelErr> {
        if let Some(obj) = out.result.as_object() {
            if obj.get("error").and_then(Value::as_str).is_some() {
                let r = serde_json::from_value(out.result.clone())
                    .map_err(|e| KernelErr::Decode(e.to_string()))?;
                return Err(KernelErr::Refused(Box::new(r)));
            }
        }
        Ok(out.result)
    }

    // ── reading ──────────────────────────────────────────────────────────

    /// One agent's rows, oldest id first, and — for an AI agent — whether it
    /// holds its history (`rows` says `loaded`). Its OWN rows are what the agent
    /// holds in memory, its loaded head (its `rows` verb: a view of the agent,
    /// never the store, so the transcript cannot drift from it); then the
    /// kernel's execution rows ABOUT it — flushed ones read back from the store
    /// under the operator's own key, the rest still in RAM.
    /// `after`: only rows with a larger id (the caller's cursor); `None` = all.
    pub async fn rows(&self, agent: &str, after: Option<i64>) -> (Vec<Row>, Option<bool>) {
        let Ok(store) = self.kernel_store() else {
            return (Vec::new(), None);
        };
        let mut by: std::collections::BTreeMap<i64, Value> = std::collections::BTreeMap::new();
        // an AI agent: a non-root agent naming a memory store (the root names
        // the project's, for the kernel's own rows)
        let keeps_rows = agent != self.root
            && self
                .kernel
                .agents
                .get(&AgentId::from(agent))
                .is_some_and(|a| a.value().record().meta.get("memory_store_id").and_then(Value::as_str).is_some());
        let mut loaded = None;
        if keeps_rows {
            let mut cursor = after;
            loop {
                let mut q = json!({"type": "rows"});
                if let Some(a) = cursor {
                    q["after"] = json!(a);
                }
                let page = self.operator(agent, q).await.result;
                loaded = page.get("loaded").and_then(Value::as_bool).or(loaded);
                for mut r in page["rows"].as_array().cloned().unwrap_or_default() {
                    if r.get("agent").is_none() {
                        r["agent"] = json!(agent);
                    }
                    if let Some(id) = r.get("id").and_then(Value::as_i64) {
                        by.insert(id, r);
                    }
                }
                match page.get("next").and_then(Value::as_i64) {
                    Some(n) if Some(n) != cursor => cursor = Some(n),
                    _ => break,
                }
            }
        }
        let mut q = json!({"type": "read", "match": {"agent": agent}});
        if let Some(a) = after {
            q["after"] = json!(a);
        }
        let exec = self.operator(&store, q).await.result;
        let ram = self.kernel.rows.all().into_iter().map(Value::Object);
        for r in exec["rows"].as_array().cloned().unwrap_or_default().into_iter().chain(ram) {
            if r.get("agent").and_then(Value::as_str) != Some(agent) {
                continue;
            }
            if let Some(id) = r.get("id").and_then(Value::as_i64) {
                if after.is_some_and(|a| id <= a) {
                    continue;
                }
                by.insert(id, r);
            }
        }
        (by.into_values().filter_map(|v| serde_json::from_value(v).ok()).collect(), loaded)
    }

    /// What is running right now.
    pub fn tasks(&self) -> Vec<Task> {
        self.kernel
            .tasks
            .rows()
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect()
    }

    /// The whole tree, built from the kernel's own agent index. `handler_module`
    /// is what selects a panel, and it is right here on the record — there is no
    /// reflect round-trip to make.
    pub fn tree(&self) -> Option<TreeNode> {
        let root = self.kernel.root()?;
        Some(self.node(&root))
    }

    fn node(&self, agent: &Arc<fantastic_kernel::Agent>) -> TreeNode {
        let mut children: Vec<TreeNode> = agent
            .child_ids()
            .into_iter()
            .filter_map(|id| self.kernel.agents.get(&id).map(|e| self.node(e.value())))
            .collect();
        // Deterministic order, so the tree pane never reshuffles under the cursor.
        children.sort_by(|a, b| a.id.cmp(&b.id));
        TreeNode {
            id: agent.id.0.clone(),
            parent_id: agent.parent_id.as_ref().map(|p| p.0.clone()),
            handler_module: agent.handler_module.clone(),
            display_name: None,
            description: None,
            children,
        }
    }

    /// Every agent's record, for the grant maps a pane shows.
    pub fn records(&self) -> Vec<Record> {
        let mut out: Vec<Record> = self
            .kernel
            .agents
            .iter()
            .filter_map(|e| {
                let a = e.value();
                let mut m = a.record().meta.clone();
                m.insert("id".into(), json!(a.id.0));
                if let Some(h) = &a.handler_module {
                    m.insert("handler_module".into(), json!(h));
                }
                if let Some(p) = &a.parent_id {
                    m.insert("parent_id".into(), json!(p.0));
                }
                serde_json::from_value(Value::Object(m)).ok()
            })
            .collect();
        out.sort_by(|a: &Record, b: &Record| a.id.cmp(&b.id));
        out
    }

    /// TRUE when the records have nowhere durable to go. `find_store` looks for
    /// a file surface rooted at `.fantastic`; without one every record stays in
    /// RAM, and the statusline must say so rather than implying the tree survives
    /// a restart.
    pub fn volatile(&self) -> bool {
        fantastic_kernel::persistence::find_store(&self.kernel).is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_holds_exactly_the_declared_set() {
        let reg = registry(std::path::Path::new("."));
        for m in [
            "kernel_state.tools",
            "file_bridge.tools",
            "sqlite_store.tools",
            "ram_store.tools",
            "user_home_store.tools",
            "user_store.tools",
            "setup.tools",
            "canvas.tools",
            "exec_bridge.tools",
            "user.tools",
            "web_mcp.tools",
            "web.tools",
            "canvas_page.tools",
            "web_ws.tools",
        ] {
            assert!(reg.get(m).is_some(), "{m} must be registered");
        }
        // Nothing that was never asked for. A bundle registered here is a bundle a
        // stray record could become.
        for m in [
            "web_rest.tools",
            "web_ui.tools",
            "static_loader.tools",
            "ui.tools",
        ] {
            assert!(reg.get(m).is_none(), "{m} was never asked for");
        }
    }

    #[test]
    fn a_refusal_reads_back_as_a_refusal_not_a_reply() {
        let out = Outcome::refused(json!({
            "error": "user -> exec: not permitted",
            "reason": "unauthorized",
            "side": "egress",
            "hint": "name exec in user.can_call",
        }));
        match Host::read(out) {
            Err(KernelErr::Refused(r)) => {
                assert_eq!(r.side.as_deref(), Some("egress"));
                assert!(r.line().contains("name exec in user.can_call"));
            }
            other => panic!("a refusal must not read as a reply: {other:?}"),
        }
    }

    #[test]
    fn an_ordinary_reply_reads_through_verbatim() {
        let mut out = Outcome::refused(json!({"lines": [], "total": 0}));
        out.result = json!({"lines": [], "total": 0});
        assert_eq!(Host::read(out).unwrap(), json!({"lines": [], "total": 0}));
    }
}
