//! THE MCP LEG — a bundle written outside the kernel tree.
//!
//! Two things live here, and they are kept apart on purpose:
//!
//! - [`WebMcpBundle`] is the AGENT (`web_mcp.tools`): what answers when something
//!   sends a verb *to* the leg. It has almost nothing to say — the leg is driven
//!   by its carrier, not by verbs — but it is a real record, gated and walked
//!   like any other agent.
//! - [`McpCore`] is the PROTOCOL: one JSON-RPC message in, one reply out. Every
//!   carrier (stdio now, HTTP later) calls it, and no carrier knows what MCP
//!   means. IDENTITY COMES ONLY FROM DISPATCH: a carrier runs outside every
//!   dispatch, so it never sends onward itself — it hands each batch to the
//!   leg's own `inbound_send`, whose sends are the leg's, never the operator's.
//!
//! The surface is deliberately two tools, `reflect` and `send`. There is no
//! tool per agent or per verb: a client learns what exists by reflecting, the
//! way an agent inside the kernel does, so capability keeps emerging instead of
//! being frozen into a schema.
//!
//! PROTOCOL VERSION: 2025-11-25, stateful (`initialize` first). Measured, not
//! assumed: Claude Code 2.1.280's stdio client opens with `initialize` at
//! 2025-11-25 and sends nothing else first (2026-09-22). The 2026-07-28
//! stateless envelope is the HTTP carrier's, later; the tool semantics below do
//! not change between the two.
//!
//! NO WAIT BOUND (owner, 2026-10-01: "do not add wait"). Every tool call runs as
//! a kernel task owned by the leg (`Kernel::spawn_task`), so its id exists from
//! the first instant: it shows in `ps`, and `cancel` on it (or the client's
//! `notifications/cancelled`) kills the whole subtree. The reply is the call's
//! outcome, whenever the task ends; an item `timeout` cancels that item.

pub mod inquiries;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use fantastic_kernel::{
    bundle::BundleError, AgentId, Bundle, Item, Kernel, Outcome, Reply, Status, TaskId,
};
use serde_json::{json, Map, Value};

/// The handler module a record names to become this leg.
pub const HANDLER_MODULE: &str = "web_mcp.tools";

/// The one protocol version the stdio carrier speaks (see the module header).
pub const PROTOCOL_VERSION: &str = "2025-11-25";

// ── the agent ───────────────────────────────────────────────────────────────

/// The leg as an agent. Stateless: the carrier holds the connection, the kernel
/// holds everything else.
#[derive(Default)]
pub struct WebMcpBundle;

#[async_trait]
impl Bundle for WebMcpBundle {
    fn name(&self) -> &str {
        "web_mcp"
    }

    /// HOW TO CALL it — served by the kernel's `docs` verb, never on a reflect.
    fn verb_docs(&self) -> Value {
        json!({
            "inbound_send": INBOUND_SEND_DOC,
            "inquiry": inquiries::INQUIRY_DOC,
            "inquiries": inquiries::INQUIRIES_DOC,
            "answer": inquiries::ANSWER_DOC,
        })
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(Some(match verb {
            "boot" => match sync_config(agent_id, kernel).await {
                Ok(()) => Value::Null,
                Err(e) => json!({"error": e, "reason": "mcp_config"}),
            },
            "shutdown" => {
                config_errors().lock().expect("config errors").remove(&error_key(kernel, agent_id));
                Value::Null
            }
            "inbound_send" => inbound_send(payload, kernel).await,
            // THE PERSON'S INQUIRIES (#307): held on the record, listed, answered.
            "inquiry" => inquiries::ask(agent_id, payload, kernel),
            "inquiries" => inquiries::list(agent_id, payload, kernel),
            "answer" => inquiries::answer(agent_id, payload, kernel).await,
            "reflect" => {
                let mut r = json!({
                    "id": agent_id.as_str(),
                    "sentence": "An MCP leg: an external client (Claude Code, any MCP host) drives this kernel through me, with exactly my grants — never as the operator. My carrier speaks MCP; the verbs I dispatch are the kernel's own.",
                    "protocol": PROTOCOL_VERSION,
                    "tools": ["reflect", "send"],
                    "inquiries_open": inquiries::open_count(kernel, agent_id),
                    "human": "An agent may send me `inquiry` when a decision is the person's: I hold it; the person lists it with `inquiries` and answers with `answer {re, text}`.",
                });
                if let Some(e) = config_error(kernel, agent_id) {
                    r["mcp_config_error"] = json!(e);
                }
                r
            }
            other => json!({
                "error": format!("web_mcp: no verb `{other}` — this leg is driven by its carrier; reflect for what it is"),
                "reason": "unhandled",
                "verb": other,
            }),
        }))
    }
}

// ── the project's MCP config ────────────────────────────────────────────────
//
// THE LEG KEEPS THE CLIENT'S ENTRY CORRECT (owner, 2026-10-08): on boot, when the
// record names `file_bridge_id` (the file_bridge it writes through) AND
// `mcp_config_file` (a path on it, e.g. `.mcp.json`), the leg reads that file
// THROUGH the bridge, AS ITSELF (its `can_call` names the bridge with `read_stream`
// and `write`, the bridge's `callable_by` names it), makes sure
// `mcpServers.fantastic` is `{command: <this process's binary, absolute>, args:
// ["mcp"]}`, and writes it back only when that changed — every other server and
// key kept as they were. So a client opened in the project finds this kernel, and
// a moved or rebuilt binary is corrected on the next start. No `file_bridge_id` →
// nothing happens. A missing file is `{}` (the leg creates it). Set and anything
// goes wrong — no `mcp_config_file`, the bridge refuses, the file is not a JSON
// object, `mcpServers` is not an object, the write is refused — is LOUD: the boot
// answers `{error, reason: "mcp_config"}` and the leg's reflect carries it as
// `mcp_config_error` until a boot succeeds. Nothing is overwritten on an error.

/// The record field naming the file_bridge the leg reads and writes the config
/// through.
pub const CONFIG_BRIDGE_FIELD: &str = "file_bridge_id";
/// The record field naming the config's path on that bridge.
pub const CONFIG_FILE_FIELD: &str = "mcp_config_file";
/// The server name this kernel is registered under in the config.
pub const SERVER_NAME: &str = "fantastic";

/// Per kernel (its address in this process) and leg: why the last boot failed.
type ErrorKey = (usize, String);

fn config_errors() -> &'static Mutex<HashMap<ErrorKey, String>> {
    static ERRORS: OnceLock<Mutex<HashMap<ErrorKey, String>>> = OnceLock::new();
    ERRORS.get_or_init(Mutex::default)
}

fn error_key(kernel: &Arc<Kernel>, agent_id: &AgentId) -> ErrorKey {
    (Arc::as_ptr(kernel) as usize, agent_id.0.clone())
}

/// Why the last boot could not make the config right, or `None`.
#[must_use]
pub fn config_error(kernel: &Arc<Kernel>, agent_id: &AgentId) -> Option<String> {
    config_errors().lock().expect("config errors").get(&error_key(kernel, agent_id)).cloned()
}

/// The entry this process registers: its own binary, absolute, with `mcp`.
pub fn server_entry() -> Result<Value, String> {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map_err(|e| format!("the binary's own path is unknown: {e}"))?;
    Ok(json!({"command": exe.display().to_string(), "args": ["mcp"]}))
}

/// `doc` with `mcpServers.fantastic` set to `entry`, or `None` when it already
/// is. Every other key and server keeps its place and value.
pub fn with_entry(doc: &str, entry: &Value) -> Result<Option<String>, String> {
    let mut root = if doc.is_empty() {
        Map::new()
    } else {
        match serde_json::from_str::<Value>(doc) {
            Ok(Value::Object(m)) => m,
            Ok(_) => return Err("the file is not a JSON object".into()),
            Err(e) => return Err(format!("the file is not JSON: {e}")),
        }
    };
    let servers = root.entry("mcpServers").or_insert_with(|| json!({}));
    let Some(servers) = servers.as_object_mut() else {
        return Err("`mcpServers` is not an object".into());
    };
    if servers.get(SERVER_NAME) == Some(entry) {
        return Ok(None);
    }
    servers.insert(SERVER_NAME.into(), entry.clone());
    let text = serde_json::to_string_pretty(&Value::Object(root)).map_err(|e| e.to_string())?;
    Ok(Some(text + "\n"))
}

/// Make the config right, as the leg. Records the outcome for reflect.
async fn sync_config(agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), String> {
    let meta = kernel
        .agents
        .get(agent_id)
        .map(|e| e.meta.read().expect("meta poisoned").clone())
        .unwrap_or_default();
    let name = |k: &str| meta.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).map(str::to_string);
    let result = match (name(CONFIG_BRIDGE_FIELD), name(CONFIG_FILE_FIELD)) {
        (None, _) => Ok(()),
        (Some(bridge), None) => Err(format!(
            "{agent_id}: file_bridge_id {bridge:?} is set but {CONFIG_FILE_FIELD} is not — name the \
             config's path on that bridge (e.g. .mcp.json)"
        )),
        (Some(bridge), Some(path)) => write_config(kernel, &bridge, &path).await,
    };
    let mut errors = config_errors().lock().expect("config errors");
    match &result {
        Ok(()) => errors.remove(&error_key(kernel, agent_id)),
        Err(e) => errors.insert(error_key(kernel, agent_id), e.clone()),
    };
    result
}

async fn write_config(kernel: &Arc<Kernel>, bridge: &str, path: &str) -> Result<(), String> {
    let entry = server_entry()?;
    let current = read_all(kernel, bridge, path).await?;
    let Some(text) = with_entry(&current, &entry).map_err(|e| format!("{bridge} {path}: {e}"))? else {
        return Ok(());
    };
    let out = Arc::clone(kernel)
        .send(vec![Item::new(AgentId::from(bridge), json!({"type": "write", "path": path, "bytes": text}))])
        .await
        .remove(0);
    if out.status != Status::Completed || out.result.get("error").is_some_and(|e| !e.is_null()) {
        return Err(format!("{bridge} write {path}: {}", why(&out)));
    }
    Ok(())
}

/// The whole file through `read_stream`; a missing file is empty.
async fn read_all(kernel: &Arc<Kernel>, bridge: &str, path: &str) -> Result<String, String> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let out = Arc::clone(kernel)
            .send(vec![Item::new(
                AgentId::from(bridge),
                json!({"type": "read_stream", "path": path, "offset": offset}),
            )])
            .await
            .remove(0);
        if out.result.get("reason").and_then(Value::as_str) == Some("not_found") && offset == 0 {
            return Ok(String::new());
        }
        if out.status != Status::Completed || out.result.get("error").is_some_and(|e| !e.is_null()) {
            return Err(format!("{bridge} read_stream {path}: {}", why(&out)));
        }
        if let Some(b) = out.body.as_ref().and_then(|b| b.at("bytes")) {
            bytes.extend_from_slice(b);
        }
        if out.result.get("eof").and_then(Value::as_bool) == Some(true) {
            break;
        }
        offset = out.result.get("next_offset").and_then(Value::as_u64).unwrap_or(offset);
    }
    String::from_utf8(bytes).map_err(|e| format!("{bridge} {path}: not utf-8 ({e})"))
}

fn why(o: &Outcome) -> String {
    match o.result.get("error") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => o.status.as_str().to_string(),
    }
}

/// `inbound_send`'s doc line.
pub const INBOUND_SEND_DOC: &str = "args: items:[{target, payload, timeout?}] (req). Sends the batch onward AS THIS LEG and returns {outcomes:[{pid, status, result}]} — how the carrier's batch becomes the leg's own calls, gated as the leg.";

/// `inbound_send {items}`: the batch, sent from inside the leg's own dispatch —
/// so every item is the leg's call.
async fn inbound_send(payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let items = match payload.get("items").and_then(Value::as_array) {
        Some(list) => list
            .iter()
            .filter_map(|it| {
                let target = it.get("target").and_then(Value::as_str)?;
                let mut item = Item::new(AgentId::from(target), it.get("payload").cloned().unwrap_or(Value::Null));
                item.timeout = it.get("timeout").and_then(Value::as_f64);
                Some(item)
            })
            .collect::<Vec<_>>(),
        None => return json!({"error": "inbound_send: `items` must be a list of {target, payload}", "reason": "bad_arg", "arg": "items"}),
    };
    let outs = Arc::clone(kernel).send(items).await;
    json!({"outcomes": outs.iter().map(row).collect::<Vec<_>>()})
}

/// One settled item, as the send contract states it.
fn row(o: &Outcome) -> Value {
    json!({"pid": o.pid.map(|p| p.get()), "status": o.status.as_str(), "result": shown(o)})
}

// ── the protocol ────────────────────────────────────────────────────────────

/// What a cold client is told in `initialize`. It is the whole onboarding: the
/// client is never told what agents exist, only how to ask — and what the
/// product is, what is optional, and what it must never touch.
pub const INSTRUCTIONS: &str = "This server is a live Aisixteen Fantastic kernel running in this project directory: a tree of agents that all answer one verb, send. You are connected as the `mcp` agent, with god-mode grants on every agent (every verb); the human can narrow them later with update_agent.\n\
\n\
Start with `reflect` and no arguments — it returns the kernel's own primer: the agent tree, the bundles it can run, and a readme index you drill into with `reflect` + readme=<key>. Reflect an agent before you send to it: verbs are discovered, never guessed. Then act with `send`. A refusal is an ordinary result, not a failure of the tool.\n\
\n\
AN AI IS OPTIONAL. Nothing is configured by default and the kernel works without one: you can be the brain yourself, driving agents, terminals and the canvas through send. The human works from the terminal UI (`fantastic`) or the browser canvas (`fantastic canvas`); the canvas and the terminal UI both open shell terminals, where a coding CLI like you can run too.\n\
\n\
SETTING UP AN AI is the human's: never pick a backend, model, host or port yourself — ASK the human which they want.\n\
\n\
KEYS live in the kernel's credential store, the `credentials` agent (it exists once the human chose where keys are kept, plain or secure): a key is a ref `<provider>/<host>` bound to its endpoint's host, read by the AI agent's backend at use time. ASK the human how the key gets there: they type it in a set-up surface (the canvas's ⚙ AI set-up, the terminal UI's set-up page), or you store it from a shell with `fantastic send`, the value written as `$env:NAME` — resolved only by that shell:\n\
\x20\x20KEY=$(op read op://vault/openai/key) fantastic send '{\"target\":\"credentials\",\"payload\":{\"type\":\"set\",\"ref\":\"responses/api.openai.com\",\"fields\":{\"api_key\":\"$env:KEY\"}}}'\n\
`set` only stores a value for a DECLARED ref: a new one is declared first with update_agent on `credentials`, its `refs` map ({<ref>: {host}} — the host lives only there; written WHOLE — keep every ref already there). Only the store's parent, `user`, may write that record: send it through `user` — {target: user, payload: {type: forward, target: kernel, payload: {type: update_agent, id: credentials, refs: {…}}}}. Never print, echo or write a key value anywhere.";

/// What `fantastic mcp` prints to stderr on start, for a coding CLI that ran it
/// in a shell to learn what it is: the onboarding above, and how to register
/// it as an MCP server in each client's own configuration.
pub const SHAPE: &str = "fantastic mcp — the MCP face of the Aisixteen Fantastic kernel in this directory (stdio, protocol 2025-11-25; tools: reflect, send).\n\
\n\
Register it ONCE as a stdio MCP server — command `fantastic`, args [\"mcp\"] — and it serves the project directory it is started in: it attaches to the kernel already running there (the terminal UI or the canvas), or runs it itself.\n\
\x20\x20Claude Code: claude mcp add --scope user fantastic -- fantastic mcp\n\
\x20\x20Codex:       ~/.codex/config.toml   [mcp_servers.fantastic] command = \"fantastic\", args = [\"mcp\"]\n\
\x20\x20Cursor:      ~/.cursor/mcp.json     {\"mcpServers\": {\"fantastic\": {\"command\": \"fantastic\", \"args\": [\"mcp\"]}}}\n\
\x20\x20Gemini CLI:  ~/.gemini/settings.json  {\"mcpServers\": {\"fantastic\": {\"command\": \"fantastic\", \"args\": [\"mcp\"]}}}\n\
\x20\x20VS Code:     .vscode/mcp.json       {\"servers\": {\"fantastic\": {\"type\": \"stdio\", \"command\": \"fantastic\", \"args\": [\"mcp\"]}}}\n\
The server must start in the project directory.\n\
\n";

/// Said before [`INSTRUCTIONS`] wherever a human or a shell reads them: they
/// are addressed to a connected client.
pub const LEAD_IN: &str = "What a connected MCP client is told (its `initialize` instructions):\n\n";

const REFLECT_DESC: &str = "Ask the kernel what exists. With NO arguments: the kernel's own primer — `tree` (every agent id), `bundles` (what it can run) and `readme` (an index of docs). With `target`: that agent describes itself — its sentence, its verbs and their args. Extra arguments are passed through to the reflect, e.g. readme=\"agents\" to read one doc of the index. Reflect before you send: verbs are discovered here, never guessed.";

const SEND_DESC: &str = "Dispatch to agents — the kernel's one verb, batch form: send([{target, payload:{type:<verb>, ...args}, timeout?}]) -> [{pid, status: completed|failed|cancelled|timed_out, result}]. The verb goes in payload.type. Discover verbs with reflect on the target; `kernel` is the alias for the root. A `timeout` (seconds) on an item CANCELS it when it expires. The call answers when it ends; each call is a kernel task, listed by send [{target:\"kernel\", payload:{type:\"ps\"}}]. A refusal (no grant) comes back as a result with an `error`, and a ledger row, like any other call.";

/// The carrier-agnostic MCP handler for one leg.
pub struct McpCore {
    kernel: Arc<Kernel>,
    leg: AgentId,
    server_version: String,
}

/// A JSON-RPC reply, or nothing for a notification.
pub type Answer = Option<Value>;

impl McpCore {
    pub fn new(kernel: Arc<Kernel>, leg: AgentId, server_version: &str) -> Self {
        Self { kernel, leg, server_version: server_version.to_string() }
    }

    /// One message in. `on_task` receives the task id of a tool call the moment
    /// it exists, so the carrier can map a request to it for cancellation.
    pub async fn handle<F>(&self, msg: &Value, on_task: F) -> Answer
    where
        F: FnOnce(TaskId) + Send,
    {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str);
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        let Some(method) = method else {
            // A response from the client, or garbage. Neither is answered.
            return None;
        };
        // Notifications get no reply, ever. Cancellation is the carrier's.
        let Some(id) = id else {
            return None;
        };

        let result = match method {
            "initialize" => Ok(self.initialize()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => Ok(self.call(&params, on_task).await),
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    /// Cancel a tool call by its task id — subtree-wide, as the operator would,
    /// because the client that asked for it has gone.
    pub async fn cancel(&self, task: &TaskId) -> Value {
        Arc::clone(&self.kernel).cancel_task(task).await
    }

    fn initialize(&self) -> Value {
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": "fantastic", "version": self.server_version },
            "instructions": INSTRUCTIONS,
        })
    }

    async fn call<F>(&self, params: &Value, on_task: F) -> Value
    where
        F: FnOnce(TaskId) + Send,
    {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        let items = match name {
            "reflect" => reflect_items(&args),
            "send" => send_items(&args),
            other => Err(format!("unknown tool `{other}` — this server has `reflect` and `send`")),
        };
        let items = match items {
            Ok(i) => i,
            Err(e) => return tool_error(&e),
        };
        self.run(name, items, on_task).await
    }

    /// Run a batch as one kernel task owned by the leg, and answer its outcome
    /// when it ends. The batch is handed to the leg's own `inbound_send`.
    async fn run<F>(&self, tool: &str, items: Vec<Item>, on_task: F) -> Value
    where
        F: FnOnce(TaskId) + Send,
    {
        let (tx, rx) = tokio::sync::oneshot::channel::<Outcome>();
        let k = Arc::clone(&self.kernel);
        let batch: Vec<Value> = items
            .iter()
            .map(|i| json!({"target": i.target.as_str(), "payload": i.payload, "timeout": i.timeout}))
            .collect();
        let hand_in = Item::new(self.leg.clone(), json!({"type": "inbound_send", "items": batch}));
        let task = self.kernel.spawn_task(&self.leg, tool, async move {
            let _ = tx.send(k.send(vec![hand_in]).await.remove(0));
        });
        on_task(task);

        match rx.await {
            Ok(o) => match o.result.get("outcomes").and_then(Value::as_array) {
                Some(rows) => settled(tool, rows),
                None => tool_error(&format!("the leg did not take the batch: {}", o.result)),
            },
            Err(_) => tool_error(&format!("task {} ended without an answer (cancelled)", task.get())),
        }
    }
}

/// The two tools, in a deterministic order.
pub fn tools() -> Vec<Value> {
    vec![
        json!({
            "name": "reflect",
            "description": REFLECT_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "target": {"type": "string", "description": "Agent id to reflect. Omit for the kernel's own primer."},
                    "readme": {"description": "true, or one key of the readme index to read (e.g. \"agents\", \"daemon\", \"transports\", \"orchestration\")."}
                },
                "additionalProperties": true
            }
        }),
        json!({
            "name": "send",
            "description": SEND_DESC,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "The batch. One item is one dispatch.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "target": {"type": "string", "description": "Agent id, or `kernel` for the root."},
                                "payload": {
                                    "type": "object",
                                    "description": "{type:<verb>, ...args} — verbs and args come from reflect on the target.",
                                    "properties": {"type": {"type": "string"}},
                                    "required": ["type"],
                                    "additionalProperties": true
                                },
                                "timeout": {"type": "number", "description": "Seconds. CANCELS the item when it expires."}
                            },
                            "required": ["target", "payload"]
                        }
                    }
                },
                "required": ["items"]
            }
        }),
    ]
}

/// `reflect` → one reflect item. No target = the root's primer: the same view
/// the kernel's own AI agents are given.
fn reflect_items(args: &Value) -> Result<Vec<Item>, String> {
    let mut payload = match args {
        Value::Object(m) => m.clone(),
        Value::Null => Map::new(),
        _ => return Err("reflect: arguments must be an object".into()),
    };
    let target = match payload.remove("target") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if !s.is_empty() => Some(s),
        Some(_) => return Err("reflect: `target` must be an agent id".into()),
    };
    payload.insert("type".into(), json!("reflect"));
    let target = match target {
        Some(t) => t,
        None => {
            for (k, v) in [("tree", json!("ids")), ("bundles", json!("ids")), ("readme", json!(true))] {
                payload.entry(k).or_insert(v);
            }
            "kernel".to_string()
        }
    };
    Ok(vec![Item::new(AgentId::from(target.as_str()), Value::Object(payload))])
}

/// `send` → the batch, item for item. Shape errors are the CLIENT's to fix, so
/// they come back as a tool error naming the item.
fn send_items(args: &Value) -> Result<Vec<Item>, String> {
    let list = args
        .get("items")
        .and_then(Value::as_array)
        .ok_or("send: `items` must be a list of {target, payload}")?;
    if list.is_empty() {
        return Err("send: `items` is empty — nothing to dispatch".into());
    }
    let mut out = Vec::with_capacity(list.len());
    for (i, it) in list.iter().enumerate() {
        let target = it
            .get("target")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("send: item {i} has no `target`"))?;
        let payload = it
            .get("payload")
            .filter(|p| p.is_object())
            .ok_or_else(|| format!("send: item {i} has no `payload` object"))?;
        if payload.get("type").and_then(Value::as_str).is_none() {
            return Err(format!(
                "send: item {i}: the verb goes in payload.type — reflect on `{target}` for its verbs"
            ));
        }
        let mut item = Item::new(AgentId::from(target), payload.clone());
        item.timeout = it.get("timeout").and_then(Value::as_f64);
        out.push(item);
    }
    Ok(out)
}

/// The settled batch, as the send contract states it. `isError` only when
/// NOTHING in the batch completed: a mixed batch is data, not a tool failure.
fn settled(tool: &str, rows: &[Value]) -> Value {
    let done = Status::Completed.as_str();
    let all_failed = !rows.is_empty() && rows.iter().all(|r| r["status"].as_str() != Some(done));
    // reflect is one item: hand back its result bare, which is what a client reads.
    let body = if tool == "reflect" && rows.len() == 1 { rows[0]["result"].clone() } else { Value::Array(rows.to_vec()) };
    text_result(&body, all_failed)
}

/// The result a client reads: a raw reply body (any verb may answer one) decoded
/// utf-8 (replace) AT ITS SLOT — what ai_core shows a model (`reply_as_text`).
/// A `read_stream` chunk reads as its text in `bytes`.
fn shown(o: &Outcome) -> Value {
    let mut v = o.result.clone();
    if let Some(b) = &o.body {
        // a slot the result cannot hold (a non-object reply) leaves it as it is
        let _ = b.place(&mut v, Value::String(String::from_utf8_lossy(&b.bytes).into_owned()));
    }
    v
}

fn text_result(v: &Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string());
    let mut r = json!({"content": [{"type": "text", "text": text}], "isError": is_error});
    if v.is_object() {
        r["structuredContent"] = v.clone();
    }
    r
}

fn tool_error(msg: &str) -> Value {
    json!({"content": [{"type": "text", "text": msg}], "isError": true})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_surface_is_exactly_reflect_then_send() {
        let names: Vec<String> =
            tools().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["reflect", "send"]);
    }

    #[test]
    fn a_zero_arg_reflect_asks_the_root_for_its_primer() {
        let items = reflect_items(&json!({})).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].target.as_str(), "kernel");
        let p = &items[0].payload;
        assert_eq!(p["type"], "reflect");
        assert_eq!(p["tree"], "ids");
        assert_eq!(p["bundles"], "ids");
        assert_eq!(p["readme"], true);
    }

    #[test]
    fn a_targeted_reflect_passes_its_extra_args_through() {
        let items = reflect_items(&json!({"target": "files", "readme": "agents"})).unwrap();
        assert_eq!(items[0].target.as_str(), "files");
        assert_eq!(items[0].payload["readme"], "agents");
        assert!(items[0].payload.get("tree").is_none(), "the primer defaults are for the root only");
    }

    #[test]
    fn send_refuses_an_item_whose_verb_is_not_in_payload_type() {
        let err = send_items(&json!({"items": [{"target": "files", "payload": {"verb": "read"}}]}))
            .unwrap_err();
        assert!(err.contains("payload.type"), "{err}");
    }

    #[test]
    fn send_carries_the_item_timeout_through() {
        let items =
            send_items(&json!({"items": [{"target": "exec", "payload": {"type": "start"}, "timeout": 5}]}))
                .unwrap();
        assert_eq!(items[0].timeout, Some(5.0));
    }

}
