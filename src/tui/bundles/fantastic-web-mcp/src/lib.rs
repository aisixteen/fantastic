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
//!   dispatch, so it never sends onward itself — the shell (the operator) hands
//!   each batch to the leg's own `inbound_send`, whose sends are the leg's,
//!   never the operator's.
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
//! EVERYTHING ASYNC (#795, owner 2026-11-27). A tool call is a CALL: the batch is
//! handed to the leg's own `inbound_send` as one kernel call, and the tool answers
//! its ACK at once — `{pid, status: accepted}`, or the launch's refusal. The leg's
//! settle — `{outcomes}`, one row per item, once every item settled — is PUSHED
//! later as a server notification on the client's session ([`SETTLE_METHOD`],
//! params `{type: settle, re, status, data}`, `re` the ack's pid), the payload every
//! wire leg tells its client. Until then the call is in flight: it shows in `ps`,
//! and `cancel` on its pid (or the client's `notifications/cancelled` naming the
//! request) kills its subtree; an item `timeout` cancels that item.

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

/// The server notification that carries a tool call's settle to the client's session.
pub const SETTLE_METHOD: &str = "notifications/settle";

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
            // the config is read and written by calls (#795): a boot that is a call
            // defers until they settled; a boot the kernel runs directly answers at
            // once, and the outcome is on reflect when they have
            verbs::Call::Boot(_) => {
                let d = kernel.defer().ok();
                let k = Arc::clone(kernel);
                sync_config(agent_id, kernel, Box::new(move |r| {
                    Box::pin(async move {
                        let Some(d) = d else { return };
                        let reply = match r {
                            Ok(()) => Value::Null,
                            Err(e) => fantastic_kernel::refusal(fantastic_kernel::refusal::INVALID_META, e),
                        };
                        k.settle_deferred(d.pid(), Outcome::of_reply(Some(d.pid()), reply));
                    })
                }))
                .await;
                Value::Null
            }
            verbs::Call::SendBatch(a) => inbound_send(a, kernel).await,
            // THE PERSON'S INQUIRIES (#307): held on the record, listed, answered.
            verbs::Call::Inquiry(a) => fantastic_kernel::inquiry::ask(agent_id, a, kernel).await,
            verbs::Call::Inquiries(a) => fantastic_kernel::inquiry::list(agent_id, &a, kernel),
            verbs::Call::AnswerInquiry(a) => fantastic_kernel::inquiry::answer(agent_id, a, kernel).await,
            verbs::Call::Reflect(_) => {
                let mut r = json!({
                    "id": agent_id.as_str(),
                    "protocol": PROTOCOL_VERSION,
                    "inquiries_open": fantastic_kernel::inquiry::open_count(kernel, agent_id),
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
// THROUGH the bridge, AS ITSELF (the bridge is its child: it reaches it by
// position), makes sure
// `mcpServers.fantastic` is `{command: <this process's binary, absolute>, args:
// ["mcp"]}`, and writes it back only when that changed — every other server and
// key kept as they were. So a client opened in the project finds this kernel, and
// a moved or rebuilt binary is corrected on the next start. No `file_bridge_id` →
// nothing happens. A missing file is `{}` (the leg creates it). Set and anything
// goes wrong — no `mcp_config_file`, the bridge refuses, the file is not a JSON
// object, `mcpServers` is not an object, the write is refused — is LOUD: the boot
// answers `{error, reason: "invalid_meta"}` and the leg's reflect carries it as
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

/// What a step of the config sync hands on when it is done.
type Then<T> = Box<dyn FnOnce(T) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send>;

/// Make the config right, as the leg, by calls (#795): `done` gets the outcome once
/// the last of them settled, and reflect carries it from then on.
async fn sync_config(agent_id: &AgentId, kernel: &Arc<Kernel>, done: Then<Result<(), String>>) {
    let Some(meta) = kernel.resolve_target(agent_id.as_str()).map(|e| e.record().meta.clone()) else {
        return done(Err(format!("{agent_id}: no such agent"))).await;
    };
    let name = |k: &str| meta.get(k).and_then(Value::as_str).map(str::to_string);
    let key = error_key(kernel, agent_id);
    let recorded: Then<Result<(), String>> = Box::new(move |result| {
        Box::pin(async move {
            {
                let mut errors = config_errors().lock().expect("config errors");
                match &result {
                    Ok(()) => errors.remove(&key),
                    Err(e) => errors.insert(key, e.clone()),
                };
            }
            done(result).await;
        })
    });
    match (name(CONFIG_BRIDGE_FIELD), name(CONFIG_FILE_FIELD)) {
        (None, _) => recorded(Ok(())).await,
        (Some(bridge), None) => {
            recorded(Err(format!(
                "{agent_id}: file_bridge_id {bridge:?} is set but {CONFIG_FILE_FIELD} is not — name the \
                 config's path on that bridge (e.g. .mcp.json)"
            )))
            .await;
        }
        (Some(bridge), Some(path)) => write_config(Arc::clone(kernel), bridge, path, recorded).await,
    }
}

async fn write_config(kernel: Arc<Kernel>, bridge: String, path: String, done: Then<Result<(), String>>) {
    let entry = match server_entry() {
        Ok(e) => e,
        Err(e) => return done(Err(e)).await,
    };
    let k = Arc::clone(&kernel);
    read_from(kernel, bridge.clone(), path.clone(), 0, Vec::new(), Box::new(move |current| {
        Box::pin(async move {
            let current = match current {
                Ok(c) => c,
                Err(e) => return done(Err(e)).await,
            };
            let text = match with_entry(&current, &entry) {
                Err(e) => return done(Err(format!("{bridge} {path}: {e}"))).await,
                Ok(None) => return done(Ok(())).await,
                Ok(Some(t)) => t,
            };
            let item = Item::new(AgentId::from(bridge.as_str()), json!({"type": "write", "path": path, "bytes": text}));
            k.call_then(item, move |out| async move {
                if out.status != Status::Completed || out.result.get("error").is_some_and(|e| !e.is_null()) {
                    return done(Err(format!("{bridge} write {path}: {}", why(&out)))).await;
                }
                done(Ok(())).await;
            })
            .await;
        })
    }))
    .await;
}

/// The whole file through `read_stream`, one chunk call after the other from
/// `offset`; a missing file is empty.
fn read_from(
    kernel: Arc<Kernel>,
    bridge: String,
    path: String,
    offset: u64,
    mut bytes: Vec<u8>,
    done: Then<Result<String, String>>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let item = Item::new(
            AgentId::from(bridge.as_str()),
            json!({"type": "read_stream", "path": path, "offset": offset}),
        );
        let k = Arc::clone(&kernel);
        kernel
            .call_then(item, move |out| async move {
                if out.result.get("reason").and_then(Value::as_str) == Some("not_found") && offset == 0 {
                    return done(Ok(String::new())).await;
                }
                if out.status != Status::Completed || out.result.get("error").is_some_and(|e| !e.is_null()) {
                    return done(Err(format!("{bridge} read_stream {path}: {}", why(&out)))).await;
                }
                if let Some(b) = out.body.as_ref().and_then(|b| b.at("bytes")) {
                    bytes.extend_from_slice(b);
                }
                if out.result.get("eof").and_then(Value::as_bool) == Some(true) {
                    let text = String::from_utf8(bytes).map_err(|e| format!("{bridge} {path}: not utf-8 ({e})"));
                    return done(text).await;
                }
                let Some(next) = out.result.get("next_offset").and_then(Value::as_u64) else {
                    return done(Err(format!("{bridge} read_stream {path}: the reply names no next_offset"))).await;
                };
                read_from(k, bridge, path, next, bytes, done).await;
            })
            .await;
    })
}

fn why(o: &Outcome) -> String {
    match o.result.get("error") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => o.status.as_str().to_string(),
    }
}

/// `inbound_send {items}`: the batch, called from inside the leg's own dispatch — so
/// every item is the leg's call. The verb DEFERS: its settle is `{outcomes}`, one row
/// per item in item order, once every item settled (a launch refusal is its row at
/// once). Nothing waits: each item's settle fills its row.
async fn inbound_send(a: verbs::SendBatch, kernel: &Arc<Kernel>) -> Value {
    let d = match kernel.defer() {
        Ok(d) => d,
        Err(refused) => return refused,
    };
    let items = a.into_items();
    let rows: Arc<Mutex<Vec<Option<Value>>>> = Arc::new(Mutex::new(vec![None; items.len()]));
    if items.is_empty() {
        kernel.settle_deferred(d.pid(), Outcome::of_reply(Some(d.pid()), json!({"outcomes": []})));
        return Value::Null;
    }
    for (i, item) in items.into_iter().enumerate() {
        let (rows, k) = (Arc::clone(&rows), Arc::clone(kernel));
        kernel
            .call_then(item, move |out| async move {
                let all = {
                    let mut g = rows.lock().expect("rows poisoned");
                    g[i] = Some(row(&out));
                    g.iter().all(Option::is_some).then(|| g.iter().flatten().cloned().collect::<Vec<_>>())
                };
                if let Some(all) = all {
                    k.settle_deferred(d.pid(), Outcome::of_reply(Some(d.pid()), json!({"outcomes": all})));
                }
            })
            .await;
    }
    Value::Null
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

/// What `fantastic mcp` prints to stderr on start, for a coding CLI that ran it
/// in a shell to learn what it is: what the server is, and how to register it
/// as an MCP server in each client's own configuration.
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

    /// One message in. A tool call answers its ACK; `on_task` receives the call's pid
    /// the moment it exists (so the carrier can map a request to it for
    /// cancellation), and `on_settle` the server notification that carries its
    /// settle ([`SETTLE_METHOD`]) when it comes — the carrier writes it on the
    /// client's session.
    pub async fn handle<F, S, Fut>(&self, msg: &Value, on_task: F, on_settle: S) -> Answer
    where
        F: FnOnce(TaskId) + Send,
        S: FnOnce(Value) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str);
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        // A response from the client, or garbage. Neither is answered.
        let method = method?;
        // Notifications get no reply, ever. Cancellation is the carrier's.
        let id = id?;

        let result = match method {
            "initialize" => Ok(self.initialize()),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => Ok(self.call(&params, on_task, on_settle).await),
            other => Err((-32601, format!("Method not found: {other}"))),
        };
        Some(match result {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err((code, message)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
            }
        })
    }

    /// Cancel a tool call by its pid — subtree-wide, as the operator would, because
    /// the client that asked for it has gone.
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

    async fn call<F, S, Fut>(&self, params: &Value, on_task: F, on_settle: S) -> Value
    where
        F: FnOnce(TaskId) + Send,
        S: FnOnce(Value) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
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
        self.run(items, on_task, on_settle).await
    }

    /// Hand the batch to the leg's own `inbound_send` as ONE call — the carrier is the
    /// in-process shell (the operator) handing it in; the leg sends each item as
    /// itself — and answer its ACK. Its settle goes to `on_settle` as the
    /// notification. Nothing waits for it.
    async fn run<F, S, Fut>(&self, items: Vec<Item>, on_task: F, on_settle: S) -> Value
    where
        F: FnOnce(TaskId) + Send,
        S: FnOnce(Value) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let batch: Vec<Value> = items
            .iter()
            .map(|i| json!({"target": i.target.as_str(), "payload": i.payload, "timeout": i.timeout}))
            .collect();
        let hand_in = Item::new(self.leg.clone(), json!({"type": "inbound_send", "items": batch}));
        let ack = fantastic_kernel::send::as_operator(self.kernel.call(hand_in, move |out| async move {
            on_settle(settle_notification(&out)).await;
        }))
        .await;
        match (ack.status, ack.pid) {
            (Status::Accepted, Some(pid)) => {
                on_task(pid);
                text_result(&json!({"pid": pid.get(), "status": Status::Accepted.as_str()}), false)
            }
            _ => tool_error(&format!("the leg did not take the batch: {}", ack.result)),
        }
    }
}

/// The server notification carrying a tool call's settle: `{type: settle, re, status,
/// data}` — `re` the call's pid (its ack's), `data` the leg's `{outcomes}` (a row per
/// item, in item order: `{pid, status, result}`).
#[must_use]
pub fn settle_notification(out: &Outcome) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": SETTLE_METHOD,
        "params": {
            "type": "settle",
            "re": out.pid.map(|p| p.get()),
            "status": out.status.as_str(),
            "data": out.result,
        },
    })
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
    let a: verbs::SendBatch = tool_args(&verbs::SEND_TOOL, args)?;
    Ok(a.into_items())
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
        assert_eq!(err, "send: missing argument 'items[0].payload.type'");
    }

    #[test]
    fn send_carries_the_item_timeout_through() {
        let items =
            send_items(&json!({"items": [{"target": "exec", "payload": {"type": "start"}, "timeout": 5}]}))
                .unwrap();
        assert_eq!(items[0].timeout, Some(5.0));
    }

}
