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
//! Everyone else is booted by this shell afterwards, in registration order. A recorded
//! `web` host is booted in every mode: it is the core listener whose routes every agent shares.
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
use std::time::Duration;

/// How long the pages get to stop writing before the kernel drains.
pub(crate) const PAGE_STOP_GRACE: Duration = Duration::from_secs(5);

/// The whole budget of a signal stop: `PAGE_STOP_GRACE` (5 s) + the kernel's
/// quiet window (`QUIET_TIMEOUT`, 10 s). A clean stop finishes inside it; only
/// a real hang is cut at it (#562).
pub(crate) const STOP_GRACE: Duration = Duration::from_secs(15);

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
/// widens what a stray record could become. `extras` (an embedding binary's
/// own bundles, #248; `main_with`'s for the `fantastic` binary) run AFTER the stock set.
fn registry_with(state: &std::path::Path, extras: &[crate::Extra]) -> BundleRegistry {
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
    // THE SCHEDULER (#507): `scheduler.tools` runs a call on an interval, as itself.
    reg.register(fantastic_scheduler::HANDLER_MODULE, fantastic_scheduler::SchedulerBundle);
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
    // THE BRIDGES: a `ws_bridge` dials another kernel's `web_ws` (an ssh tunnel is an
    // `exec_bridge` job it dials locally); `relay_connector` joins a relay kernel (#469).
    reg.register(fantastic_bridge::WS_HANDLER_MODULE, fantastic_bridge::WsBridgeBundle);
    reg.register(fantastic_bridge::RELAY_HANDLER_MODULE, fantastic_bridge::RelayConnectorBundle);
    // THE HUMAN, as an agent. This is the id the terminal acts as, so every act
    // carries a ledger row naming a person instead of an operator injection.
    reg.register(fantastic_user::HANDLER_MODULE, fantastic_user::UserBundle::default());
    // THE MCP LEG — a bundle from outside the kernel tree. Registered so a
    // persisted `mcp` record hydrates into a live agent on every run; the
    // record itself is only ever created by `fantastic mcp`, never by the
    // blueprint.
    reg.register(fantastic_web_mcp::HANDLER_MODULE, fantastic_web_mcp::WebMcpBundle);
    // THE AI AGENT'S SAVED SET-UPS. Its directory is fixed here, by the binary —
    // never by a record; who reaches it is its `callable_by` (`kern/saved`).
    reg.register(
        fantastic_user_home_store::HANDLER_MODULE,
        fantastic_user_home_store::UserHomeStore::new(fantastic_user_home_store::home_dir_from_env()),
    );
    // THE USER STORE: the keys, set-ups and stored fantastic locations in the
    // user's home; "here" is this fantastic's state folder, fixed by the binary (`kern/user_store`).
    reg.register(
        fantastic_user_home_store::user_store::HANDLER_MODULE,
        fantastic_user_home_store::user_store::UserStore::new(
            fantastic_user_home_store::home_dir_from_env(),
            std::path::absolute(state).unwrap_or_else(|_| state.to_path_buf()),
        ),
    );
    // THE WEB HOST (the one HTTP/WS listener: a recorded one listens in every mode, and
    // agents add routes to it), the canvas's page and its socket leg. `fantastic canvas`
    // creates them when missing; registered so they rehydrate.
    reg.register(fantastic_web::HANDLER_MODULE, fantastic_web::WebBundle);
    reg.register(fantastic_canvas_page::HANDLER_MODULE, fantastic_canvas_page::CanvasPageBundle);
    reg.register(fantastic_web_ws::HANDLER_MODULE, fantastic_web_ws::WebWsBundle);
    // THE BROWSER'S SET-UP PAGE, host side. Registered so the page can create
    // it; it exists only while that page is open (`kern/setup_agent`).
    reg.register(crate::kern::setup_agent::HANDLER_MODULE, crate::kern::setup_agent::SetupBundle);
    // THE SIDEBAR: the canvas's left column, one html view its reflect returns (`kern/sidebar`).
    reg.register(crate::kern::sidebar::HANDLER_MODULE, crate::kern::sidebar::SidebarBundle);
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
    // The AI agent apart from any provider (`ai_chat.tools`): it completes through
    // the provider agent its record names (`provider_id`). Always linked.
    reg.register(fantastic_ai_core::chat_bundle::HANDLER_MODULE, fantastic_ai_core::chat_bundle::AiChatBundle);
    // The vLLM backend: linked by default (feature `vllm`).
    #[cfg(feature = "vllm")]
    reg.register(fantastic_vllm_backend::HANDLER_MODULE, fantastic_vllm_backend::VllmBundle);
    // The Anthropic Messages backend: linked by default (feature `anthropic`).
    #[cfg(feature = "anthropic")]
    reg.register(fantastic_anthropic_backend::HANDLER_MODULE, fantastic_anthropic_backend::AnthropicBundle);
    // The Ollama backend: linked by default (feature `ollama`).
    #[cfg(feature = "ollama")]
    reg.register(fantastic_ollama_backend::HANDLER_MODULE, fantastic_ollama_backend::OllamaBackendBundle);
    // The OpenAI Responses backend: linked by default (feature `responses`).
    #[cfg(feature = "responses")]
    reg.register(
        fantastic_responses_backend::HANDLER_MODULE,
        fantastic_responses_backend::ResponsesBundle,
    );
    // THE EMBEDDER'S BUNDLES (#248), after the stock set: a record naming one
    // hydrates like any other.
    for extra in extras {
        extra(&mut reg);
    }
    reg
}

/// Set `web`'s record field `key` (named `flag` in a note) before its listener boots.
/// A value the record already holds is left alone; a refused one is a note, and
/// the boot goes on with the recorded value.
async fn set_web_field(kernel: &Arc<Kernel>, id: &AgentId, key: &str, flag: &str, value: Value, notes: &mut Vec<String>) {
    let had = kernel.agents.get(id).and_then(|a| a.value().record().meta.get(key).cloned());
    if had.as_ref() == Some(&value) {
        return;
    }
    let mut update = json!({"type": "update_agent", "id": id.0});
    update[key] = value.clone();
    let r = kernel.send(vec![Item::new(AgentId::from("kernel"), update)]).await.remove(0).result;
    if let Some(err) = r.get("error").and_then(Value::as_str) {
        notes.push(format!("{id}: {flag} {value} not set: {err}"));
    }
}

/// `--host ADDR` on `web`: the listener's `bind` is ADDR. The guard's identity (`host`) is
/// ADDR too, unless ADDR is a wildcard, which a browser cannot dial: the record keeps its
/// loopback identity, so `localhost:<port>` is the address the guard admits (see `web.rs`).
/// A wildcard over a recorded concrete `host` (an earlier LAN bind) resets it to loopback,
/// so the guard never judges by an address the listener no longer names.
async fn set_web_bind(kernel: &Arc<Kernel>, id: &AgentId, addr: &str, notes: &mut Vec<String>) {
    set_web_field(kernel, id, "bind", "--host", json!(addr), notes).await;
    if !crate::kern::web::is_wildcard(addr) {
        set_web_field(kernel, id, "host", "--host", json!(addr), notes).await;
    } else if kernel.agents.get(id).is_some_and(|a| a.value().record().meta.contains_key("host")) {
        set_web_field(kernel, id, "host", "--host", json!("127.0.0.1"), notes).await;
    }
}

/// Boot every agent the kernel's start did not (`done`), in registration order. A recorded
/// `web` host boots too, in every mode: it is the one HTTP/WS listener, and every agent with
/// a route reuses it. `web_port`/`web_bind` (`--port`/`--host`) are set on it before
/// it listens; a `web` with neither the flag nor a recorded port gets a free one. A tree with no `web` record binds no port. A failure is a note, and the boot
/// goes on.
async fn boot_the_rest(kernel: &Arc<Kernel>, done: &[String], web_port: Option<u16>, web_bind: Option<&str>) -> Vec<String> {
    let mut notes = Vec::new();
    // the `web` hosts, by handler: one reflect of the tree
    let mut web: Vec<String> = Vec::new();
    let found = kernel
        .send(vec![Item::new(
            AgentId::from("kernel"),
            json!({"type": "reflect", "tree": "ids", "handler_modules": [fantastic_web::HANDLER_MODULE]}),
        )])
        .await
        .remove(0)
        .result;
    match found.get("tree").and_then(Value::as_array) {
        Some(ids) if found.get("error").is_none() => web = ids.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => notes.push(format!("the web hosts could not be listed: {}", found.get("error").and_then(Value::as_str).unwrap_or("no tree"))),
    }
    for id in kernel.agent_ids() {
        if done.contains(&id.0) {
            continue;
        }
        if web.contains(&id.0) {
            // No `--port` and none recorded (a `web` seeded without one): a free port is
            // picked once and recorded, as the first `canvas` does; later runs reuse it.
            let recorded = kernel
                .agents
                .get(&id)
                .is_some_and(|a| a.value().record().meta.get("port").and_then(Value::as_u64).is_some_and(|p| p > 0));
            let picked = match (web_port, recorded) {
                (None, false) => match crate::kern::web::free_port() {
                    Ok(p) => Some(p),
                    Err(e) => {
                        notes.push(format!("{id}: {e}"));
                        None
                    }
                },
                _ => web_port,
            };
            if let Some(p) = picked {
                set_web_field(kernel, &id, "port", "--port", json!(p), &mut notes).await;
            }
            if let Some(addr) = web_bind {
                set_web_bind(kernel, &id, addr, &mut notes).await;
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
    notes
}

impl Host {
    /// Boot over `paths.store` (the state) and `paths.work`, with the stock bundles plus `extras`. Acquires the directory
    /// lock, so a second shell in the same directory fails here rather than corrupting the
    /// store.
    ///
    /// `web_port`: `--port` — a recorded `web` on another port is set to it
    /// BEFORE its listener boots (nothing runs here yet, so it is the user's to
    /// overwrite). `web_bind`: `--host`, set on the record the same way.
    pub async fn boot(
        paths: &Paths,
        extras: &[crate::Extra],
        web_port: Option<u16>,
        web_bind: Option<String>,
    ) -> Result<Self, KernelErr> {
        let booted = bootstrap::bootstrap(
            registry_with(&paths.store, extras),
            BootstrapOptions::daemon(&paths.store, &paths.work),
        )
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
            // The root keeps the work folder: set on the first start, and again when a
            // `--workdir` was given while no kernel runs (`Paths::resolve`).
            let work = paths.work.display().to_string();
            if meta.get("workdir").and_then(Value::as_str) != Some(work.as_str()) {
                patch.insert("workdir".into(), json!(work));
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

        // Grants a product declared for the root (its blueprint's `@root` parent
        // entry): unioned in once the kernel runs, because only then does the root
        // loader write the record to disk.
        crate::boot::declared::apply_to_root(&kernel)
            .await
            .map_err(|e| KernelErr::Boot(format!("could not set the root's grants: {e}")))?;

        // Everyone else, in registration order. A failure is logged and
        // skipped: one bundle refusing must not take the shell down with it.
        let mut notes = boot_the_rest(&kernel, &done, web_port, web_bind.as_deref()).await;

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
        // THE PROJECT'S SYSTEM BLOCK: `prompt.md`, in its bridge's folder, seeded, read by
        // every AI through its bridge (`kern/prompt`)
        match crate::kern::prompt::ensure(&host).await {
            Ok(()) => {
                if let Err(e) = crate::kern::reach::grant_all(&host).await {
                    notes.push(e);
                }
                notes.extend(crate::kern::prompt::stamp_all(&host).await);
            }
            Err(e) => notes.push(e),
        }
        // THE PROJECT'S `.mcp.json`: kept by the `mcp` leg, through its bridge
        // (`kern/mcp_config`) — right before any coding CLI connects
        if let Err(e) = crate::kern::mcp_config::ensure(&host).await {
            notes.push(e);
        }
        // the orientation a coding CLI opened here reads (`kern/orient`), and the
        // state folder's readme, rewritten from the live values
        match (host.kernel.state_dir(), host.kernel.work_dir()) {
            (Ok(state), Ok(work)) => {
                if let Err(e) = crate::kern::orient::seed(&state) {
                    notes.push(e);
                }
                if let Err(e) = crate::kern::orient::write_readme(&host, &state, &work) {
                    notes.push(e);
                }
            }
            (Err(e), _) | (_, Err(e)) => notes.push(e),
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

    /// The AI agents in the tree: the ids `reflect` lists for the handlers of the
    /// five backends and `ai_chat.tools`, and the agents of roles over them.
    pub async fn ai_agents(&self) -> Vec<String> {
        let r = self
            .operator(
                "kernel",
                json!({"type": "reflect", "tree": "ids", "handler_modules": [
                    "anthropic_backend.tools", "ollama_backend.tools", "responses_backend.tools",
                    "vllm_backend.tools", "foundation_models_backend.tools", "ai_chat.tools"]}),
            )
            .await
            .result;
        if let Some(e) = r.get("error").and_then(Value::as_str) {
            tracing::warn!("ai_agents: reflect refused: {e}");
            return Vec::new();
        }
        r.get("tree").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default()
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

    /// [`Host::act`] with SECRET MARKS (`secret`, paths into `payload`): the
    /// values reach the target's handler and show as `[secret]` in every row,
    /// event and mirror — on the forward and on `user`'s onward send.
    pub async fn act_secret(&self, target: &str, payload: Value, secret: &[&str]) -> Outcome {
        let k = Arc::clone(&self.kernel);
        let item = Item::new(
            AgentId::from(self.cfg.user.as_str()),
            json!({"type": "forward", "target": target, "payload": payload}),
        )
        .with_secret(secret.iter().map(|m| format!("payload.{m}")));
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
        // an AI agent: its record's handler, or the base of its role, is a backend or
        // `ai_chat.tools` (the root's is not: it keeps the project's rows through the
        // kernel store)
        let handler = self.kernel.agents.get(&AgentId::from(agent)).and_then(|a| a.value().record().handler_module.clone());
        let keeps_rows = handler.is_some_and(|h| {
            let base = self.kernel.bundles.base_of(&h);
            [h.as_str(), base.as_str()].iter().any(|n| {
                matches!(
                    *n,
                    "anthropic_backend.tools"
                        | "ollama_backend.tools"
                        | "responses_backend.tools"
                        | "vllm_backend.tools"
                        | "foundation_models_backend.tools"
                        | "ai_chat.tools"
                )
            })
        });
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

/// Shut down IN THE KERNEL'S ORDER, on the runtime that booted it: the
/// pages stop writing first ([`stop_pages`]), then `Kernel::shutdown` (drain, each agent's `shutdown` verb, cancel what
/// remains, the last execution rows flushed into the memory store, the
/// records written, the teardown walk), and only then the lock release.
/// Releasing the lock first used to lose the last execution. The one stop: every mode,
/// every refused boot and an embedder end in it.
pub async fn stop(kernel: &Arc<Kernel>) {
    // The kernel lands every record and every message itself
    // (`flush_pending`) before its teardown walk; the lock goes last.
    stop_pages(kernel).await;
    kernel.shutdown().await;
    // Every row has landed: the SQLite stores fold their WAL into the file
    // and close, so a stopped kernel leaves only `memory.db`.
    for e in fantastic_sqlite_store::close_all(kernel) {
        tracing::warn!("shutdown: {e}");
    }
    match kernel.state_dir() {
        Ok(state) => {
            if let Err(e) = bootstrap::shutdown(&state) {
                tracing::warn!("shutdown: {e}");
            }
        }
        Err(e) => tracing::warn!("shutdown: {e}"),
    }
}

/// THE PAGES STOP BEFORE THE DRAIN (owner, 2026-10-15): every connected
/// page kernel is called `<guid>|canvas` `host_stopping` — it flushes its
/// last records and writes no more — and the drain starts once each has
/// answered or [`PAGE_STOP_GRACE`] has passed. A write that would land
/// mid-drain is refused there; this makes none land.
async fn stop_pages(kernel: &Arc<Kernel>) {
    let guids: std::collections::BTreeSet<String> = kernel
        .peer_rows()
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r.get("guid").and_then(Value::as_str).map(str::to_string))
        .collect();
    let calls = guids.into_iter().map(|g| {
        let k = Arc::clone(kernel);
        async move {
            let item = Item::new(AgentId::from(format!("{g}|canvas").as_str()), json!({"type": "host_stopping"}));
            match tokio::time::timeout(PAGE_STOP_GRACE, k.send(vec![item])).await {
                Err(_) => tracing::warn!("shutdown: page {g} did not stop within {PAGE_STOP_GRACE:?}"),
                Ok(mut out) => {
                    if let Some(e) = out.remove(0).result.get("error").and_then(Value::as_str) {
                        tracing::warn!("shutdown: page {g}: {e}");
                    }
                }
            }
        }
    });
    futures_util::future::join_all(calls).await;
}

/// THE ONE BOOT, every mode's and an embedder's: [`Host::boot`] (the stock bundles plus
/// `extras`), then, when `canvas` asks for it, the canvas ([`crate::kern::web::ensure`], its
/// URLs returned), then the grant watch ([`crate::kern::reach::start`]), which must follow the
/// boot's own grant writes. With `web_port`/`web_bind` and no canvas, the folder must hold a
/// `web` record to set them on; none is refused. A refusal after the kernel is up stops it
/// ([`stop`]) and returns the reason. Opens no socket, never seeds.
pub async fn boot_all(
    paths: &Paths,
    extras: &[crate::Extra],
    web_port: Option<u16>,
    web_bind: Option<String>,
    canvas: bool,
) -> Result<(Arc<Host>, Option<crate::kern::web::Urls>), String> {
    let asked = web_port.is_some() || web_bind.is_some();
    let bind = web_bind.clone();
    let host = Arc::new(Host::boot(paths, extras, web_port, web_bind).await.map_err(|e| format!("{e}"))?);
    let web = host
        .operator("kernel", json!({"type": "reflect", "tree": "ids", "handler_modules": [fantastic_web::HANDLER_MODULE]}))
        .await
        .result;
    let has_web = web.get("tree").and_then(Value::as_array).is_some_and(|t| !t.is_empty());
    if asked && !canvas && !has_web {
        stop(&host.kernel).await;
        return Err("--port/--host: the folder has no `web` record to set them on".into());
    }
    let urls = if canvas {
        match crate::kern::web::ensure(&host, web_port, bind.as_deref()).await {
            Ok(u) => Some(u),
            Err(e) => {
                stop(&host.kernel).await;
                return Err(e);
            }
        }
    } else {
        None
    };
    if let Err(e) = crate::kern::reach::start(&host).await {
        stop(&host.kernel).await;
        return Err(e);
    }
    Ok((host, urls))
}

/// How a bounded stop ended. Clean: the kernel's clean stop finished. Cut: it
/// hung past the grace, or a second stop signal came.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stopped {
    Clean,
    Cut,
}

/// Run the clean stop, but never past `grace` and never past a second signal.
/// A cut releases the lock and removes the run file, so the caller only exits.
pub(crate) async fn stop_bounded(
    clean: impl std::future::Future<Output = ()>,
    second: impl std::future::Future<Output = ()>,
    grace: Duration,
    state: &std::path::Path,
) -> Stopped {
    tokio::select! {
        biased;
        _ = clean => return Stopped::Clean,
        _ = second => tracing::warn!("stop: a second signal — exiting now"),
        _ = tokio::time::sleep(grace) => tracing::warn!("stop: the clean stop did not finish within {grace:?} — cut"),
    }
    if let Err(e) = bootstrap::shutdown(state) {
        tracing::warn!("stop: {e}")
    }
    crate::kern::web::remove_run_file(state);
    Stopped::Cut
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_holds_exactly_the_declared_set() {
        let reg = registry_with(std::path::Path::new("."), &[]);
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
            "ws_bridge.tools",
            "relay_connector.tools",
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

    /// An embedder's bundle (#248): answers every call with a fixed reply.
    struct ExtraProbe;

    const PROBE: &str = "extra_probe.tools";

    fantastic_kernel::stand_in_verbs!(extra_probe_verbs { Ping "ping" {} });

    #[async_trait::async_trait]
    impl fantastic_kernel::bundle::Handler for ExtraProbe {
        type Call = extra_probe_verbs::Call;

        fn name(&self) -> &str {
            "extra_probe"
        }

        fn man(&self) -> &'static str {
            "A test stand-in an embedder registers."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            extra_probe_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            extra_probe_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(
            &self,
            _id: &AgentId,
            call: Self::Call,
            _kernel: &Arc<Kernel>,
        ) -> Result<Option<Value>, fantastic_kernel::bundle::BundleError> {
            Ok(match call {
                extra_probe_verbs::Call::Ping(_) => Some(json!({"probe": "pong"})),
                extra_probe_verbs::Call::Reflect(_) => None,
            })
        }
    }

    /// The `Extra` an embedding binary passes to `main_with`.
    fn probe_extra(reg: &mut BundleRegistry) {
        reg.register(PROBE, ExtraProbe);
    }

    /// THE STOCK SET, pinned: exactly these handler modules (plus the backends the
    /// build's features link). A bundle added to or dropped from the roster
    /// changes this list on purpose.
    #[test]
    fn the_stock_registry_is_exactly_this_list() {
        let reg = registry_with(std::path::Path::new("."), &[]);
        let mut got: Vec<&str> = reg.catalog().map(|(m, _)| m).collect();
        got.sort_unstable();
        #[allow(unused_mut)]
        let mut want: Vec<&str> = vec![
            "ai_chat.tools",
            "canvas.tools",
            "canvas_page.tools",
            "credential_store.tools",
            "exec_bridge.tools",
            "file_bridge.tools",
            "kernel_state.tools",
            "mcp_client.tools",
            "py_agent.tools",
            "ram_store.tools",
            "relay_connector.tools",
            "scheduler.tools",
            "setup.tools",
            "sidebar.tools",
            "sqlite_store.tools",
            "user.tools",
            "user_home_store.tools",
            "user_store.tools",
            "web.tools",
            "web_mcp.tools",
            "web_ws.tools",
            "ws_bridge.tools",
        ];
        #[cfg(feature = "foundation-models")]
        want.push(fantastic_foundation_models_backend::HANDLER_MODULE);
        #[cfg(feature = "vllm")]
        want.push(fantastic_vllm_backend::HANDLER_MODULE);
        #[cfg(feature = "anthropic")]
        want.push(fantastic_anthropic_backend::HANDLER_MODULE);
        #[cfg(feature = "ollama")]
        want.push(fantastic_ollama_backend::HANDLER_MODULE);
        #[cfg(feature = "responses")]
        want.push(fantastic_responses_backend::HANDLER_MODULE);
        want.sort_unstable();
        assert_eq!(got, want, "the stock handler-module list");
    }

    #[test]
    fn an_extra_bundle_is_registered_after_the_stock_set() {
        let here = std::path::Path::new(".");
        let stock = registry_with(here, &[]);
        let reg = registry_with(here, &[probe_extra]);
        assert!(stock.get(PROBE).is_none(), "not in the stock set");
        assert!(reg.get(PROBE).is_some(), "the extra registered it");
        assert!(reg.get(fantastic_core::HANDLER_MODULE).is_some(), "the stock set stays");
        assert_eq!(reg.catalog().count(), stock.catalog().count() + 1, "one bundle more, listed");
    }

    /// `main_with` sets the process's extras; the registry built from `crate::extras()` —
    /// what the modes pass `Host::boot` — runs them. (The only test that sets them: the first set wins.)
    #[test]
    fn the_extras_main_with_sets_reach_the_host_registry() {
        crate::set_extras(&[probe_extra]);
        assert_eq!(crate::extras().len(), 1);
        assert!(registry_with(std::path::Path::new("."), crate::extras()).get(PROBE).is_some(), "the registry built from the process extras has it");
    }

    /// A record on disk naming the extra's handler module HYDRATES with the extra,
    /// and is weak-skipped without it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_extra_bundle_hydrates_its_record() {
        let dir = std::env::temp_dir().join(format!("fc-extra-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rec = dir.join(".fantastic").join("agents").join("probe").join("agent.yaml");
        std::fs::create_dir_all(rec.parent().unwrap()).unwrap();
        std::fs::write(&rec, fantastic_kernel::yaml_io::dump(&json!({"id": "probe", "handler_module": PROBE}))).unwrap();

        // one_shot: no lock, nothing written
        let booted = bootstrap::bootstrap(
            registry_with(&dir, &[probe_extra]),
            BootstrapOptions::one_shot(dir.join(".fantastic"), &dir),
        )
        .expect("boots");
        assert!(
            booted.loaded.contains(&AgentId::from("probe")),
            "hydrated: {:?}",
            booted.loaded
        );

        let stock = bootstrap::bootstrap(
            registry_with(&dir, &[]),
            BootstrapOptions::one_shot(dir.join(".fantastic"), &dir),
        )
        .expect("boots");
        assert!(
            !stock.loaded.contains(&AgentId::from("probe")),
            "weak-skipped without the extra: {:?}",
            stock.loaded
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A booted in-memory kernel from the CLI host's own registry, with `guid`.
    fn host_kernel(guid: &str) -> Arc<Kernel> {
        let k = bootstrap::bootstrap(registry_with(std::path::Path::new("."), &[]), BootstrapOptions::in_memory())
            .expect("boots")
            .kernel;
        let root = k.root().expect("root").id.clone();
        let mut m = serde_json::Map::new();
        m.insert("guid".into(), json!(guid));
        k.update(&root, m);
        k
    }

    async fn send(k: &Arc<Kernel>, to: &str, payload: Value) -> Value {
        k.send(vec![Item::new(AgentId::from(to), payload)]).await.remove(0).result
    }

    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    }

    async fn accepts(port: u16) -> bool {
        for _ in 0..50 {
            if tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .is_ok()
            {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        false
    }

    /// The web host table is process-wide and keyed by agent id, so each test names its own.
    async fn make_web(k: &Arc<Kernel>, id: &str, root: &str, port: u16) {
        let made = send(
            k,
            "kernel",
            json!({"type": "create_agent", "parent_id": root, "id": id,
                   "handler_module": fantastic_web::HANDLER_MODULE, "port": port}),
        )
        .await;
        assert_eq!(made["id"], id, "{made}");
        // create_agent boots it; stop it, so a boot is what brings it back (as after a rehydrate)
        send(k, id, json!({"type": "stop"})).await;
    }

    /// #560: `--headless`, the terminal UI and `fantastic mcp` pass no port; a recorded `web` still listens.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_recorded_web_listens_with_no_port_given() {
        let k = host_kernel("g560a");
        let root = k.root().expect("root").id.0.clone();
        let p = free_port();
        make_web(&k, "web560a", &root, p).await;
        assert!(!accepts(p).await, "nothing listens before the boot");
        let notes = boot_the_rest(&k, &[root], None, None).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert!(accepts(p).await, "the recorded web must listen");
        send(&k, "web560a", json!({"type": "stop"})).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canvas_still_sets_the_port_before_the_web_listens() {
        let k = host_kernel("g560b");
        let root = k.root().expect("root").id.0.clone();
        let (a, b) = (free_port(), free_port());
        make_web(&k, "web560b", &root, a).await;
        assert!(!accepts(a).await, "stopped before the boot");
        let notes = boot_the_rest(&k, &[root], Some(b), None).await;
        assert!(notes.is_empty(), "{notes:?}");
        let got = k
            .agents
            .get(&AgentId::from("web560b"))
            .expect("web")
            .value()
            .record()
            .meta["port"]
            .clone();
        assert_eq!(got, json!(b));
        assert!(accepts(b).await, "the web listens on the given port");
        assert!(!accepts(a).await, "not on the recorded one");
        send(&k, "web560b", json!({"type": "stop"})).await;
    }

    /// #560: a folder with no `web` record binds no port.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tree_without_a_web_record_boots_no_web() {
        let k = host_kernel("g560c");
        let root = k.root().expect("root").id.0.clone();
        let made = send(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": root, "id": "st", "handler_module": "ram_store.tools"}),
        )
        .await;
        assert_eq!(made["id"], "st", "{made}");
        let notes = boot_the_rest(&k, &[root], None, None).await;
        assert!(notes.is_empty(), "{notes:?}");
        let web = send(&k, "kernel", json!({"type": "reflect", "tree": "ids", "handler_modules": ["web.tools"]})).await;
        assert_eq!(web["tree"], json!([]), "no web agent exists: {web}");
    }

    /// A bundle that answers only `ping`, registered under any handler name.
    struct Stub(&'static str);

    fantastic_kernel::stand_in_verbs!(stub_verbs { Ping "ping" {} });

    #[async_trait::async_trait]
    impl fantastic_kernel::bundle::Handler for Stub {
        type Call = stub_verbs::Call;

        fn name(&self) -> &str {
            self.0
        }

        fn man(&self) -> &'static str {
            "A test stand-in that answers only ping."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            stub_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            stub_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(
            &self,
            _id: &AgentId,
            call: Self::Call,
            _k: &Arc<Kernel>,
        ) -> Result<Option<Value>, fantastic_kernel::bundle::BundleError> {
            Ok(match call {
                stub_verbs::Call::Ping(_) => Some(json!({"pong": true})),
                stub_verbs::Call::Reflect(_) => None,
            })
        }
    }

    /// The row watch lists the agents of the five backends and `ai_chat.tools`, and of
    /// roles over them, by handler: not a record that merely names a store, not the root.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ai_agents_lists_the_agents_of_the_six_handlers_only() {
        let dir = std::env::temp_dir().join(format!("fc-host-{}-ai-agents", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
            state_dir: dir.join(".fantastic"),
            work_dir: dir.to_path_buf(),
        });
        kernel.bundles.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
        for m in ["ai_chat.tools", "vllm_backend.tools", "py_agent.tools"] {
            kernel.bundles.register(m, Stub(m));
        }
        kernel.bundles.register_meta_yaml("chat_role", "base: ai_chat.tools\nsentence: s\n").unwrap();
        kernel.bundles.register_meta_yaml("py_role", "base: py_agent.tools\nsentence: s\n").unwrap();
        let kernel = Arc::new(kernel);
        let root = fantastic_kernel::Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            serde_json::Map::new(),
            dir.join(".fantastic"),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        for (id, handler, with_store) in
            [
            ("chat", "ai_chat.tools", false),
            ("vllm", "vllm_backend.tools", true),
            ("py", "py_agent.tools", true),
            ("chat2", "chat_role", false),
            ("py2", "py_role", false),
        ]
        {
            let mut payload = json!({"type": "create_agent", "parent_id": "kernel", "id": id, "handler_module": handler});
            if with_store {
                payload["memory_store_id"] = json!("m");
            }
            let made = send(&kernel, "kernel", payload).await;
            assert_eq!(made["id"], id, "{made}");
        }
        let host = Host { kernel, cfg: crate::env::ClientCfg::here(), root: "kernel".to_string() };
        assert_eq!(host.ai_agents().await, vec!["chat".to_string(), "vllm".to_string(), "chat2".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bundle that answers `rows` with one row, registered under any handler name.
    struct RowsStub(&'static str);

    fantastic_kernel::stand_in_verbs!(rows_verbs { Rows "rows" { after: Option<i64> } });

    #[async_trait::async_trait]
    impl fantastic_kernel::bundle::Handler for RowsStub {
        type Call = rows_verbs::Call;

        fn name(&self) -> &str {
            self.0
        }

        fn man(&self) -> &'static str {
            "A test stand-in that holds one row."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            rows_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            rows_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(
            &self,
            _id: &AgentId,
            call: Self::Call,
            _k: &Arc<Kernel>,
        ) -> Result<Option<Value>, fantastic_kernel::bundle::BundleError> {
            Ok(match call {
                rows_verbs::Call::Rows(_) => Some(json!({"rows": [{"id": 900001, "role": "user", "content": "hi"}]})),
                rows_verbs::Call::Reflect(_) => None,
            })
        }
    }

    /// `rows` reads the agent's own rows when its handler, or the base of its role, is
    /// one of the six: a role over `ai_chat.tools` does, a role over any other bundle does not.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rows_holds_the_agent_rows_of_a_role_over_an_ai_handler_only() {
        let dir = std::env::temp_dir().join(format!("fc-host-{}-role-rows", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut kernel = Kernel::with_storage(fantastic_kernel::StorageMode::Disk {
            state_dir: dir.join(".fantastic"),
            work_dir: dir.to_path_buf(),
        });
        kernel.bundles.register(fantastic_core::HANDLER_MODULE, fantastic_core::KernelStateBundle::default());
        for m in ["ai_chat.tools", "plain_rows.tools"] {
            kernel.bundles.register(m, RowsStub(m));
        }
        kernel.bundles.register_meta_yaml("chat_role", "base: ai_chat.tools\nsentence: s\n").unwrap();
        kernel.bundles.register_meta_yaml("plain_role", "base: plain_rows.tools\nsentence: s\n").unwrap();
        let kernel = Arc::new(kernel);
        let mut meta = serde_json::Map::new();
        meta.insert("memory_store_id".into(), json!("no_store"));
        let root = fantastic_kernel::Agent::new(
            AgentId::from("kernel"),
            Some(fantastic_core::HANDLER_MODULE.to_string()),
            None,
            meta,
            dir.join(".fantastic"),
            false,
        );
        kernel.register(Arc::clone(&root));
        kernel.set_root(root);
        for (id, handler) in [("w", "chat_role"), ("p", "plain_role")] {
            let made = send(
                &kernel,
                "kernel",
                json!({"type": "create_agent", "parent_id": "kernel", "id": id, "handler_module": handler}),
            )
            .await;
            assert_eq!(made["id"], id, "{made}");
        }
        let host = Host { kernel, cfg: crate::env::ClientCfg::here(), root: "kernel".to_string() };
        let (w, _) = host.rows("w", None).await;
        assert!(w.iter().any(|r| r.id == 900001), "{w:?}");
        let (p, _) = host.rows("p", None).await;
        assert!(p.iter().all(|r| r.id != 900001), "{p:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #469: a CLI host kernel makes a `ws_bridge` and `forward` reaches an agent on the
    /// other kernel (memory transport). Before the registry held `ws_bridge.tools`,
    /// `boot` and `forward` were refused: "cannot be imported in this runtime".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cli_kernel_bridges_to_another_kernel_through_ws_bridge() {
        let (ka, kb) = (host_kernel("host-a"), host_kernel("host-b"));
        let root_a = ka.root().expect("root").id.0.clone();
        let root_b = kb.root().expect("root").id.0.clone();
        let tag = format!("brg469_{}", std::process::id());
        let (a_id, b_id) = (format!("{tag}_a"), format!("{tag}_b"));
        // the agent on the far side: its `callable_by` names the serving leg
        let made = send(
            &kb,
            "kernel",
            json!({"type": "create_agent", "parent_id": root_b, "id": "far",
                   "handler_module": "ram_store.tools", "callable_by": {&b_id: "*"}}),
        )
        .await;
        assert_eq!(made["id"], "far", "{made}");
        fantastic_bridge::inject_pair(&AgentId::from(a_id.as_str()), &AgentId::from(b_id.as_str()));
        let made = send(
            &ka,
            "kernel",
            json!({"type": "create_agent", "parent_id": root_a, "id": a_id,
                   "handler_module": "ws_bridge.tools", "transport": "memory", "peer_id": "B"}),
        )
        .await;
        assert_eq!(made["id"], json!(a_id), "{made}");
        let made = send(
            &kb,
            "kernel",
            json!({"type": "create_agent", "parent_id": root_b, "id": b_id,
                   "handler_module": "ws_bridge.tools", "transport": "memory", "peer_id": "A",
                   "ingress_rule": "allow_all_ingress", "can_call": {"far": "*"}}),
        )
        .await;
        assert_eq!(made["id"], json!(b_id), "{made}");
        for (k, id) in [(&ka, &a_id), (&kb, &b_id)] {
            let r = send(k, id, json!({"type": "boot"})).await;
            assert!(r["booted"] == json!(true) || r["already"] == json!(true), "{r}");
        }
        let r = send(&ka, &a_id, json!({"type": "forward", "target": "far", "payload": {"type": "reflect"}})).await;
        assert_eq!(r["id"], "far", "forward reached the far agent: {r}");
    }

    #[test]
    #[cfg(all(feature = "vllm", feature = "anthropic", feature = "ollama", feature = "responses"))]
    fn the_default_build_links_the_four_ai_backends() {
        let reg = registry_with(std::path::Path::new("."), &[]);
        for m in [
            "vllm_backend.tools",
            "anthropic_backend.tools",
            "ollama_backend.tools",
            "responses_backend.tools",
        ] {
            assert!(reg.get(m).is_some(), "{m} must be in the default build");
        }
    }

    /// #507: a `scheduler.tools` agent made through `create_agent` answers its verbs
    /// (before the registry held it: "No module named 'scheduler'").
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_scheduler_agent_answers_schedule_and_unschedule() {
        // a schedule persists through the root loader: a disk kernel whose store is
        // the file_bridge on `.fantastic`
        let dir = std::env::temp_dir().join(format!("fc-sched507-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let k = bootstrap::bootstrap(
            registry_with(&dir, &[]),
            BootstrapOptions::one_shot(dir.join(".fantastic"), &dir),
        )
        .expect("boots")
        .kernel;
        let root = k.root().expect("root").id.0.clone();
        let store = send(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": root, "id": "store",
                   "handler_module": "file_bridge.tools", "root": ".fantastic",
                   "allow": ["."], "ingress_rule": "allow_all_ingress"}),
        )
        .await;
        assert_eq!(store["id"], "store", "{store}");
        send(&k, "kernel", json!({"type": "boot"})).await;
        let made = send(
            &k,
            "kernel",
            json!({"type": "create_agent", "parent_id": root, "id": "sched507",
                   "handler_module": "scheduler.tools"}),
        )
        .await;
        assert_eq!(made["id"], "sched507", "{made}");
        let r = send(
            &k,
            "sched507",
            json!({"type": "schedule", "target": "kernel", "payload": {"type": "reflect"}, "interval_seconds": 3600}),
        )
        .await;
        let sid = r["schedule_id"].as_str().unwrap_or_else(|| panic!("schedule replied: {r}")).to_string();
        let r = send(&k, "sched507", json!({"type": "unschedule", "schedule_id": sid})).await;
        assert_eq!(r["removed"], json!(true), "{r}");
        let _ = std::fs::remove_dir_all(&dir);
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
    /// A bundle whose `hang` call never answers: an in-flight call at the stop.
    struct StopHang;

    const HANG: &str = "stop_hang.tools";

    static ENTERED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fantastic_kernel::stand_in_verbs!(hang_verbs { Hang "hang" {} });

    #[async_trait::async_trait]
    impl fantastic_kernel::bundle::Handler for StopHang {
        type Call = hang_verbs::Call;

        fn name(&self) -> &str {
            "stop_hang"
        }

        fn man(&self) -> &'static str {
            "A test stand-in whose `hang` never answers."
        }

        fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
            hang_verbs::VERBS
        }

        fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
            hang_verbs::dynamic_verbs(agent)
        }

        fn record(&self) -> &'static fantastic_kernel::RecordDecl {
            fantastic_kernel::fixture_record!()
        }

        async fn handle(
            &self,
            _id: &AgentId,
            call: Self::Call,
            _kernel: &Arc<Kernel>,
        ) -> Result<Option<Value>, fantastic_kernel::bundle::BundleError> {
            if let hang_verbs::Call::Hang(_) = call {
                ENTERED.store(true, std::sync::atomic::Ordering::SeqCst);
                std::future::pending::<()>().await;
            }
            Ok(None)
        }
    }

    fn hang_extra(reg: &mut BundleRegistry) {
        reg.register(HANG, StopHang);
    }

    /// A temp project with the lock held and a run file written.
    fn locked_dir(name: &str) -> std::path::PathBuf {
        let tmp = std::env::temp_dir().join(format!("fc-stop-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join(".fantastic/cli")).expect("dirs");
        fantastic_kernel::lock::acquire(&tmp.join(".fantastic")).expect("lock");
        std::fs::write(tmp.join(".fantastic").join(crate::kern::web::RUN_FILE), "pid: 1\n").expect("run file");
        tmp
    }

    fn assert_released(tmp: &std::path::Path) {
        assert!(
            !tmp.join(".fantastic/lock.yaml").exists(),
            "the lock file is gone"
        );
        assert!(
            fantastic_kernel::lock::held(&tmp.join(".fantastic"))
                .unwrap()
                .is_none(),
            "the lock is released"
        );
        assert!(
            !tmp.join(".fantastic").join(crate::kern::web::RUN_FILE).exists(),
            "the run file is gone"
        );
    }

    #[test]
    fn stop_grace_is_the_page_grace_plus_the_quiet_window() {
        assert_eq!(STOP_GRACE, Duration::from_secs(15));
        assert_eq!(STOP_GRACE, PAGE_STOP_GRACE + Duration::from_secs(10));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_with_a_call_in_flight_finishes_clean_inside_the_grace() {
        let tmp = std::env::temp_dir().join(format!("fc-stop-clean-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).expect("dir");
        let k = bootstrap::bootstrap(
            registry_with(std::path::Path::new("."), &[hang_extra]),
            BootstrapOptions::in_memory(),
        )
        .expect("boots")
        .kernel;
        let root = k.root().expect("root").id.clone();
        let made = send(&k, "kernel", json!({"type": "create_agent", "parent_id": root, "id": "hang562", "handler_module": HANG})).await;
        assert_eq!(made["id"], "hang562", "{made}");
        let k2 = k.clone();
        tokio::spawn(async move {
            k2.send(vec![Item::new("hang562", json!({"type": "hang"}))])
                .await;
        });
        for _ in 0..250 {
            if ENTERED.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            ENTERED.load(std::sync::atomic::Ordering::SeqCst),
            "the call is in flight"
        );
        let t0 = std::time::Instant::now();
        let out = stop_bounded(
            k.shutdown(),
            std::future::pending(),
            STOP_GRACE,
            &tmp.join(".fantastic"),
        )
        .await;
        let dt = t0.elapsed();
        assert_eq!(out, Stopped::Clean);
        assert!(
            dt >= Duration::from_secs(9),
            "the quiet window was waited: {dt:?}"
        );
        assert!(dt < STOP_GRACE, "inside the grace: {dt:?}");
        let led = send(&k, "kernel", json!({"type": "ledger"})).await;
        let rows = led["executions"]
            .as_array()
            .unwrap_or_else(|| panic!("ledger: {led}"));
        let row = rows
            .iter()
            .find(|r| r["target"] == "hang562" && r["status"] == "cancelled")
            .unwrap_or_else(|| panic!("a cancelled hang562 execution: {led}"));
        assert_ne!(
            row["acked"],
            json!(true),
            "the dead letter is unacked: {row}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn a_hang_past_the_grace_is_cut_with_the_lock_and_run_file_gone() {
        let tmp = locked_dir("cut");
        let t0 = std::time::Instant::now();
        let out = stop_bounded(
            std::future::pending(),
            std::future::pending(),
            Duration::from_millis(300),
            &tmp.join(".fantastic"),
        )
        .await;
        let dt = t0.elapsed();
        assert_eq!(out, Stopped::Cut);
        assert!(
            dt >= Duration::from_millis(300) && dt < Duration::from_secs(2),
            "{dt:?}"
        );
        assert_released(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn a_second_signal_cuts_at_once() {
        let tmp = locked_dir("second");
        let t0 = std::time::Instant::now();
        let out = stop_bounded(
            std::future::pending(),
            tokio::time::sleep(Duration::from_millis(100)),
            STOP_GRACE,
            &tmp.join(".fantastic"),
        )
        .await;
        assert_eq!(out, Stopped::Cut);
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert_released(&tmp);
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

/// `set_web_field`: a `--port` / `--host` lands on the `web` record before its listener
/// boots, once; a value already held is left alone; a refused update is a note.
#[cfg(test)]
mod set_web_field_tests {
    use super::*;
    use crate::kern::web::canvas_tree_tests::harness;

    fn field_of(h: &Host, id: &str, key: &str) -> Option<Value> {
        h.kernel.agents.get(&AgentId::from(id)).and_then(|a| a.value().record().meta.get(key).cloned())
    }

    #[tokio::test]
    async fn a_host_is_set_on_the_web_record_and_a_held_one_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("fc-set-web-field-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = harness(&dir);
        h.operator(
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel", "id": "web",
                "handler_module": fantastic_core::HANDLER_MODULE, "port": 8088}),
        )
        .await;
        let web = AgentId::from("web");
        let mut notes = Vec::new();
        // absent: the bind is written, and nothing is noted
        set_web_field(&h.kernel, &web, "host", "--host", json!("0.0.0.0"), &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "host"), Some(json!("0.0.0.0")));
        // the same bind again: already held, so nothing changes and nothing is noted
        set_web_field(&h.kernel, &web, "host", "--host", json!("0.0.0.0"), &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "host"), Some(json!("0.0.0.0")));
        // a different bind overwrites it (the record is the user's to change before boot)
        set_web_field(&h.kernel, &web, "host", "--host", json!("127.0.0.1"), &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "host"), Some(json!("127.0.0.1")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_refused_host_is_a_note_and_the_boot_goes_on() {
        let dir = std::env::temp_dir().join(format!("fc-set-web-refused-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = harness(&dir);
        let mut notes = Vec::new();
        // no such agent: the update is refused, and the refusal is said, with the value
        set_web_field(&h.kernel, &AgentId::from("nobody"), "host", "--host", json!("0.0.0.0"), &mut notes).await;
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("nobody: --host \"0.0.0.0\" not set: "), "{notes:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `--host` on `web` writes the listener's `bind`, and its identity `host` only for a
/// concrete address: a wildcard publish keeps the loopback identity the guard judges by.
#[cfg(test)]
mod set_web_bind_tests {
    use super::*;
    use crate::kern::web::canvas_tree_tests::harness;

    fn field_of(h: &Host, id: &str, key: &str) -> Option<Value> {
        h.kernel.agents.get(&AgentId::from(id)).and_then(|a| a.value().record().meta.get(key).cloned())
    }

    #[tokio::test]
    async fn a_wildcard_bind_leaves_the_guard_identity_unset_and_a_lan_bind_sets_both() {
        let dir = std::env::temp_dir().join(format!("fc-set-web-bind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = harness(&dir);
        h.operator(
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel", "id": "web",
                "handler_module": fantastic_core::HANDLER_MODULE, "port": 8088}),
        )
        .await;
        let web = AgentId::from("web");
        let mut notes = Vec::new();
        set_web_bind(&h.kernel, &web, "0.0.0.0", &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "bind"), Some(json!("0.0.0.0")));
        assert_eq!(field_of(&h, "web", "host"), None);
        // a LAN address is both the listener and the identity
        set_web_bind(&h.kernel, &web, "192.168.1.5", &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "bind"), Some(json!("192.168.1.5")));
        assert_eq!(field_of(&h, "web", "host"), Some(json!("192.168.1.5")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_wildcard_over_a_lan_bind_resets_the_guard_identity_to_loopback() {
        let dir = std::env::temp_dir().join(format!("fc-set-web-bind-reset-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let h = harness(&dir);
        h.operator(
            "kernel",
            json!({"type": "create_agent", "parent_id": "kernel", "id": "web",
                "handler_module": fantastic_core::HANDLER_MODULE, "port": 8088}),
        )
        .await;
        let web = AgentId::from("web");
        let mut notes = Vec::new();
        // `--host 192.168.1.5` records both the listener and the guard's identity
        set_web_bind(&h.kernel, &web, "192.168.1.5", &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "host"), Some(json!("192.168.1.5")));
        // `--host 0.0.0.0` then moves the listener only: the identity must not stay on the LAN
        // address (the guard would refuse the `localhost:<port>` the run file names)
        set_web_bind(&h.kernel, &web, "0.0.0.0", &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "bind"), Some(json!("0.0.0.0")));
        assert_eq!(field_of(&h, "web", "host"), Some(json!("127.0.0.1")));
        // and a wildcard again over the loopback identity changes nothing
        set_web_bind(&h.kernel, &web, "0.0.0.0", &mut notes).await;
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(field_of(&h, "web", "host"), Some(json!("127.0.0.1")));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
