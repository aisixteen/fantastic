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
pub mod record;
pub mod verbs;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use fantastic_kernel::{
    bundle::BundleError, AgentId, Handler, Item, Kernel, Outcome, Reply, Status, TaskId,
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

/// What the bundle is for: its one short man text; how to call it is rendered from its
/// declared verbs (`docs`, `reflect`).
const MAN: &str = "An MCP leg: an external client (Claude Code, any MCP host) drives this kernel through it, with exactly its grants, never as the operator. It also holds the questions agents ask the person who drives it.";

#[async_trait]
impl Handler for WebMcpBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        "web_mcp"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(match call {
            verbs::Call::Boot(_) => match sync_config(agent_id, kernel).await {
                Ok(()) => Value::Null,
                Err(e) => json!({"error": e, "reason": "mcp_config"}),
            },
            verbs::Call::InboundSend(a) => inbound_send(a, kernel).await,
            // THE PERSON'S INQUIRIES (#307): held on the record, listed, answered.
            verbs::Call::Inquiry(a) => inquiries::ask(agent_id, a, kernel),
            verbs::Call::Inquiries(a) => inquiries::list(agent_id, &a, kernel),
            verbs::Call::AnswerInquiry(a) => inquiries::answer(agent_id, a, kernel).await,
            verbs::Call::Reflect(_) => {
                let mut r = json!({
                    "id": agent_id.as_str(),
                    "protocol": PROTOCOL_VERSION,
                    "inquiries_open": inquiries::open_count(kernel, agent_id),
                });
                if let Some(e) = config_error(kernel, agent_id) {
                    r["mcp_config_error"] = json!(e);
                }
                r
            }
        }))
    }

    /// The leg stops: the config error it recorded goes.
    async fn on_shutdown(
        &self,
        agent_id: &AgentId,
        kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        config_errors().lock().expect("config errors").remove(&error_key(kernel, agent_id));
        Ok(())
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

/// `inbound_send {items}`: the batch, sent from inside the leg's own dispatch —
/// so every item is the leg's call.
async fn inbound_send(a: verbs::InboundSend, kernel: &Arc<Kernel>) -> Value {
    let items = a
        .items
        .into_iter()
        .map(|it| {
            let mut item = Item::new(AgentId::from(it.target.as_str()), it.payload.into_value());
            item.timeout = it.timeout;
            item
        })
        .collect::<Vec<_>>();
    let outs = Arc::clone(kernel).send(items).await;
    json!({"outcomes": outs.iter().map(row).collect::<Vec<_>>()})
}

/// One settled item, as the send contract states it.
fn row(o: &Outcome) -> Value {
    json!({"pid": o.pid.map(|p| p.get()), "status": o.status.as_str(), "result": shown(o)})
}

// ── the protocol ────────────────────────────────────────────────────────────

/// The one short generic text of what this bundle is for: told to a client in
/// `initialize`. Everything else a client
/// learns is rendered from the agents' own declarations (`reflect`).
pub const INSTRUCTIONS: &str = "A live Aisixteen Fantastic kernel: a tree of agents that all answer one verb, send. Start with `reflect` and no arguments for the agent tree and the bundles; reflect an agent before you send to it. A refusal is an ordinary result.";



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

/// The two tools, in a deterministic order; each one's input schema is its declared struct.
pub fn tools() -> Vec<Value> {
    [&verbs::REFLECT_TOOL, &verbs::SEND_TOOL]
        .into_iter()
        .map(|t| json!({"name": t.name, "description": t.doc(), "inputSchema": t.schema()}))
        .collect()
}

/// The tool's arguments, checked by the kernel's one engine against the declared struct,
/// then typed. The refusal's text is the tool error.
fn tool_args<T: serde::de::DeserializeOwned>(tool: &fantastic_kernel::Verb, args: &Value) -> Result<T, String> {
    let refused = |r: Value| r["error"].as_str().map_or_else(|| r.to_string(), str::to_string);
    let checked = fantastic_kernel::verbs::validate(tool.name, tool.schema(), args).map_err(refused)?;
    fantastic_kernel::verbs::decode(tool.name, &checked).map_err(refused)
}

/// `reflect` → one reflect item. No target = the root's primer: the same view
/// the kernel's own AI agents are given.
fn reflect_items(args: &Value) -> Result<Vec<Item>, String> {
    let a: verbs::ReflectTool = tool_args(&verbs::REFLECT_TOOL, args)?;
    let mut payload = a.args;
    payload.insert("type".into(), json!("reflect"));
    let target = match a.target {
        Some(t) => t,
        None => {
            for (k, v) in [("tree", json!("ids")), ("bundles", json!("ids"))] {
                payload.entry(k).or_insert(v);
            }
            "kernel".to_string()
        }
    };
    Ok(vec![Item::new(AgentId::from(target.as_str()), Value::Object(payload))])
}

/// `send` → the batch, item for item.
fn send_items(args: &Value) -> Result<Vec<Item>, String> {
    let a: verbs::SendTool = tool_args(&verbs::SEND_TOOL, args)?;
    Ok(a.items
        .into_iter()
        .map(|it| {
            let mut item = Item::new(AgentId::from(it.target.as_str()), it.payload.into_value());
            item.timeout = it.timeout;
            item
        })
        .collect())
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
    }

    #[test]
    fn a_targeted_reflect_passes_its_extra_args_through() {
        let items = reflect_items(&json!({"target": "files", "detail": "full"})).unwrap();
        assert_eq!(items[0].target.as_str(), "files");
        assert_eq!(items[0].payload["detail"], "full");
        assert!(items[0].payload.get("tree").is_none(), "the primer defaults are for the root only");
    }

    #[test]
    fn send_refuses_an_item_whose_verb_is_not_in_payload_type() {
        let err = send_items(&json!({"items": [{"target": "files", "payload": {"verb": "read"}}]}))
            .unwrap_err();
        assert_eq!(err, "send: the arguments do not parse (missing field `type`)");
    }

    #[test]
    fn send_carries_the_item_timeout_through() {
        let items =
            send_items(&json!({"items": [{"target": "exec", "payload": {"type": "start"}, "timeout": 5}]}))
                .unwrap();
        assert_eq!(items[0].timeout, Some(5.0));
    }

}
