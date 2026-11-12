//! mcp_client — ONE remote MCP server as an agent. Python
//! `mcp_client/tools.py`, text for text.
//!
//! `create_agent handler_module=mcp_client.tools url=<the server's MCP endpoint>
//! credentials={store, ref}` (optional — a keyless server takes none). Its `boot`
//! asks the server who it is (`server/discover`) and lists its tools
//! (`tools/list`, every page); from then on EACH TOOL IS A VERB of this agent:
//! `send {type: <tool>, …its arguments}` is a `tools/call`. `docs of=<this agent>`
//! lists them — its dynamic verbs ([`fantastic_kernel::verbs::DynVerb`]), built from
//! each tool's `inputSchema` and `description`. A grant names tools like verbs.
//!
//! Streamable HTTP, protocol 2026-07-28 ([`wire`]), stateless. AUTH = a static key:
//! the ref's `api_key`, read from the credential store before EVERY
//! request (as an AI backend reads its key) and sent as `Authorization: Bearer`
//! to the ref's host only.
//!
//! THE ANSWER: `{content, structured?}` — the tool's content items as the server
//! sent them, except `image`/`audio` `data` and an embedded resource's `blob`,
//! which arrive as RAW BYTES — ONE binary body per reply, at its top-level
//! `bytes` (the frame codec and every leg carry one, there): the first binary
//! item says `body: "bytes"`, a later one has `size` and `omitted` instead;
//! `structuredContent` as `structured`. A tool error (`isError`) adds `error`, so
//! the call settles `errored`. A tool that needs input mid-call answers
//! `{error, input_required}`.

pub mod wire;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use base64::Engine;
use fantastic_ai_core::credentials::key_reader;
use fantastic_ai_core::record::Credentials;
use fantastic_bundle::py_repr_value;
use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::DynVerb;
use schemars::JsonSchema;
use serde::Deserialize;
use fantastic_kernel::outcome::Blob;
use fantastic_kernel::refusal::{
    refusal, TRANSPORT_ERROR, UNAUTHORIZED,
};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::wire::{McpError, Mirror, Opts};

/// The handler module a record names.
pub const HANDLER_MODULE: &str = "mcp_client.tools";
/// The bundle's name.
pub const NAME: &str = "mcp_client";
/// What a binary item past the first carries instead of its bytes.
pub const OMITTED: &str = "one binary body per reply — this item's bytes were not sent";

/// What the bundle is for — its one short man text.
const MAN: &str = "A client of a remote MCP server: its tools are this agent's verbs, listed when it boots and run on the server with the key's rights; progress and log lines a tool sends come back as events.";

/// What one server said, per instance.
#[derive(Debug, Default, Clone)]
struct State {
    /// `None` until a listing succeeded.
    tools: Option<Vec<(String, Value)>>,
    mirrors: HashMap<String, Vec<Mirror>>,
    server: Value,
    instructions: Value,
    skipped: Vec<Value>,
    error: Option<String>,
}

/// The bundle.
#[derive(Default)]
pub struct McpClientBundle {
    state: Mutex<HashMap<String, State>>,
}

impl McpClientBundle {
    fn get(&self, id: &str) -> State {
        self.state
            .lock()
            .expect("state")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }
    fn put(&self, id: &str, f: impl FnOnce(&mut State)) {
        f(self
            .state
            .lock()
            .expect("state")
            .entry(id.to_string())
            .or_default());
    }
}

/// Verbs the kernel and this bundle answer first — a tool by one of these names
/// is skipped, never shadowed silently.
fn reserved(name: &str) -> bool {
    declared::VERBS
        .iter()
        .chain(fantastic_kernel::declared::common::VERBS.iter())
        .any(|v| v.name == name)
        || fantastic_kernel::declared::kernel_verbs()
            .iter()
            .any(|v| v.name == name)
}

fn meta_of(kernel: &Kernel, id: &AgentId) -> Map<String, Value> {
    kernel
        .agents
        .get(id)
        .map(|e| e.meta.read().expect("meta").clone())
        .unwrap_or_default()
}

fn url_of(kernel: &Kernel, id: &AgentId) -> String {
    meta_of(kernel, id)
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

async fn read_key(
    id: &AgentId,
    kernel: &Arc<Kernel>,
    url: &str,
) -> Result<Option<String>, McpError> {
    key_reader(id, kernel, NAME, false)
        .read(url)
        .await
        .map_err(|e| McpError(refusal(UNAUTHORIZED, e.0)))
}

async fn no_note(_: Value) {}

// ─── the record and the verbs ────────────────────────────────────────

/// The fields an mcp_client record carries.
#[derive(Debug, JsonSchema)]
pub struct McpRecord {
    /// The MCP server's endpoint: an absolute http(s) URL with a host and no fragment, e.g. https://mcp.example.com/mcp.
    #[schemars(length(min = 1), regex(pattern = r"^https?://[^/?#\s]+[^#\s]*$"))]
    pub url: String,
    /// The credential store and ref holding `api_key`, sent as a Bearer key to that host only; absent = keyless.
    pub credentials: Option<Credentials>,
}

fantastic_kernel::record!(McpRecord);

/// Ask the server who it is and list its tools — this agent's verbs. Returns {server, protocol_version, tools:[names], skipped?}. Sent again it RE-LISTS (a server's tools changed); a failure keeps the tools already listed and answers why (`reflect` shows it as `last_error`).
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

mod declared {
    use super::Boot;
    fantastic_kernel::verbs! { dynamic; "boot" => Boot }
}

// ─── the answer ──────────────────────────────────────────────────────

fn b64(url: &str, s: &Value) -> Result<Vec<u8>, McpError> {
    let text = s.as_str().unwrap_or("");
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|e| {
            McpError(refusal(
                TRANSPORT_ERROR,
                format!("mcp {url}: invalid base64 in a content item: {e}"),
            ))
        })
}

/// One binary value: the reply's ONE body, at its top-level `bytes`, when it is
/// the first (the item says `body: "bytes"`), else only its size and `omitted`.
fn body(url: &str, data: &Value, sent: &mut Option<Blob>) -> Result<Map<String, Value>, McpError> {
    let raw = b64(url, data)?;
    let mut out = Map::new();
    out.insert("size".into(), json!(raw.len()));
    if sent.is_some() {
        out.insert("omitted".into(), json!(OMITTED));
    } else {
        out.insert("body".into(), json!("bytes"));
        *sent = Some(Blob::key("bytes", raw));
    }
    Ok(out)
}

/// A `tools/call` result as the caller reads it — the JSON and its one body.
///
/// # Errors
/// Invalid base64 in a content item.
pub fn answer(
    url: &str,
    tool: &str,
    result: &Map<String, Value>,
) -> Result<(Value, Option<Blob>), McpError> {
    if result.get("resultType").and_then(Value::as_str) == Some("input_required") {
        let mut ir = Map::new();
        for k in ["inputRequests", "requestState"] {
            if let Some(v) = result.get(k) {
                ir.insert(k.into(), v.clone());
            }
        }
        return Ok((
            json!({
                "error": format!("{tool}: the server needs input this client cannot give yet"),
                "input_required": ir,
            }),
            None,
        ));
    }
    let mut sent: Option<Blob> = None;
    let mut content = Vec::new();
    let items = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for c in items {
        let Some(m) = c.as_object() else {
            content.push(c);
            continue;
        };
        let kind = m.get("type").and_then(Value::as_str).unwrap_or("");
        if matches!(kind, "image" | "audio") && m.get("data").is_some_and(Value::is_string) {
            let mut rest: Map<String, Value> = m
                .iter()
                .filter(|(k, _)| *k != "data")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            rest.extend(body(url, &m["data"], &mut sent)?);
            content.push(Value::Object(rest));
            continue;
        }
        if kind == "resource" {
            if let Some(res) = m
                .get("resource")
                .and_then(Value::as_object)
                .filter(|r| r.contains_key("blob"))
            {
                let mut inner: Map<String, Value> = res
                    .iter()
                    .filter(|(k, _)| *k != "blob")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                inner.extend(body(url, &res["blob"], &mut sent)?);
                let mut item = m.clone();
                item.insert("resource".into(), Value::Object(inner));
                content.push(Value::Object(item));
                continue;
            }
        }
        content.push(c);
    }
    let mut out = Map::new();
    if result.get("isError") == Some(&Value::Bool(true)) {
        let text: Vec<&str> = content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect();
        let text = text.join("\n");
        let err = if text.is_empty() {
            format!("{tool}: the tool reported an error")
        } else {
            text
        };
        out.insert("error".into(), json!(err));
    }
    out.insert("content".into(), Value::Array(content));
    if let Some(s) = result.get("structuredContent") {
        out.insert("structured".into(), s.clone());
    }
    Ok((Value::Object(out), sent))
}

// ─── the bundle ──────────────────────────────────────────────────────

impl McpClientBundle {
    fn reflect(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        let url = meta_of(kernel, id)
            .get("url")
            .cloned()
            .unwrap_or(Value::Null);
        let st = self.get(id.as_str());
        let mut out = Map::new();
        out.insert("id".into(), json!(id.as_str()));
        out.insert("sentence".into(), json!(MAN));
        out.insert("url".into(), url);
        out.insert("tools".into(), json!(st.tools.as_ref().map_or(0, Vec::len)));
        for (k, v) in [("server", &st.server), ("instructions", &st.instructions)] {
            if !v.is_null() {
                out.insert(k.into(), v.clone());
            }
        }
        if !st.skipped.is_empty() {
            out.insert("skipped".into(), Value::Array(st.skipped.clone()));
        }
        if let Some(e) = st.error.filter(|e| !e.is_empty()) {
            out.insert("last_error".into(), json!(e));
        }
        Value::Object(out)
    }

    async fn boot(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        let url = url_of(kernel, id);
        match self.list(id, kernel, &url).await {
            Ok(v) => v,
            Err(McpError(reply)) => {
                let err = fantastic_kernel::refusal::error_text(&reply);
                self.put(id.as_str(), |s| s.error = err);
                reply
            }
        }
    }

    async fn list(&self, id: &AgentId, kernel: &Arc<Kernel>, url: &str) -> Result<Value, McpError> {
        let key = read_key(id, kernel, url).await?;
        let disc = wire::connect(url, key.as_deref()).await?;
        let mut tools: Vec<Value> = Vec::new();
        let has_tools = disc
            .get("capabilities")
            .and_then(Value::as_object)
            .is_some_and(|c| c.contains_key("tools"));
        if has_tools {
            let mut cursor: Option<Value> = None;
            let mut seen: Vec<Value> = Vec::new();
            loop {
                let mut params = Map::new();
                if let Some(c) = &cursor {
                    params.insert("cursor".into(), c.clone());
                }
                let page = wire::rpc(
                    url,
                    "tools/list",
                    params,
                    Opts {
                        key: key.as_deref(),
                        headers: vec![],
                    },
                    no_note,
                )
                .await;
                let page = page?;
                tools.extend(
                    page.get("tools")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                );
                cursor = page
                    .get("nextCursor")
                    .filter(|c| c.as_str().is_some_and(|s| !s.is_empty()))
                    .cloned();
                let Some(c) = &cursor else { break };
                if seen.contains(c) {
                    return Err(McpError(refusal(
                        TRANSPORT_ERROR,
                        format!("mcp {url}: tools/list repeated cursor {}", py_repr_value(c)),
                    )));
                }
                seen.push(c.clone());
            }
        }
        let mut table: Vec<(String, Value)> = Vec::new();
        let mut mirrors = HashMap::new();
        let mut skipped = Vec::new();
        for t in tools {
            let name = t.get("name").cloned().unwrap_or(Value::Null);
            let Some(n) = name.as_str().filter(|n| !n.is_empty()) else {
                skipped.push(json!({"name": name, "why": "a tool with no name"}));
                continue;
            };
            if reserved(n) {
                skipped
                    .push(json!({"name": n, "why": "the kernel or this bundle answers that verb"}));
                continue;
            }
            if table.iter().any(|(k, _)| k == n) {
                skipped.push(json!({"name": n, "why": "listed twice"}));
                continue;
            }
            let schema = t.get("inputSchema").cloned().unwrap_or_else(|| json!({}));
            match wire::mirrors(&schema) {
                Err(why) => {
                    skipped.push(json!({"name": n, "why": why}));
                    continue;
                }
                Ok(found) => {
                    mirrors.insert(n.to_string(), found);
                }
            }
            table.push((n.to_string(), t));
        }
        let server = disc.get("server").cloned().unwrap_or(Value::Null);
        let names: Vec<String> = table.iter().map(|(n, _)| n.clone()).collect();
        let instructions = disc.get("instructions").cloned().unwrap_or(Value::Null);
        self.put(id.as_str(), |s| {
            *s = State {
                tools: Some(table),
                mirrors,
                server: server.clone(),
                instructions,
                skipped: skipped.clone(),
                error: None,
            };
        });
        kernel
            .emit(
                id,
                json!({"type": "tools_listed", "tools": names, "skipped": skipped}),
            )
            .await;
        let mut out = Map::new();
        out.insert("server".into(), server);
        out.insert("tools".into(), json!(names));
        if !skipped.is_empty() {
            out.insert("skipped".into(), Value::Array(skipped));
        }
        Ok(Value::Object(out))
    }

    async fn call(
        &self,
        id: &AgentId,
        tool: &str,
        args: Value,
        kernel: &Arc<Kernel>,
        mirrors: &[Mirror],
    ) -> Result<(Value, Option<Blob>), McpError> {
        let url = url_of(kernel, id);
        let arguments: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
        let key = read_key(id, kernel, &url).await?;
        let headers = wire::param_headers(mirrors, &Value::Object(arguments.clone()))?;
        let mut params = Map::new();
        params.insert("name".into(), json!(tool));
        params.insert("arguments".into(), Value::Object(arguments));
        let k = Arc::clone(kernel);
        let me = id.clone();
        let tool_name = tool.to_string();
        let note = move |msg: Value| {
            let k = Arc::clone(&k);
            let me = me.clone();
            let tool_name = tool_name.clone();
            async move {
                let mut out = Map::new();
                let kind = match msg.get("method").and_then(Value::as_str) {
                    Some("notifications/progress") => "progress",
                    Some("notifications/message") => "log",
                    _ => return,
                };
                out.insert("type".into(), json!(kind));
                out.insert("tool".into(), json!(tool_name));
                if let Some(p) = msg.get("params").and_then(Value::as_object) {
                    for (pk, pv) in p {
                        out.insert(pk.clone(), pv.clone());
                    }
                }
                k.emit(&me, Value::Object(out)).await;
            }
        };
        let result = wire::rpc(
            &url,
            "tools/call",
            params,
            Opts {
                key: key.as_deref(),
                headers,
            },
            note,
        )
        .await?;
        answer(&url, tool, &result)
    }
}

#[async_trait]
impl Handler for McpClientBundle {
    type Call = declared::Call;

    fn name(&self) -> &str {
        NAME
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        declared::VERBS
    }

    /// THE SERVER'S CURRENT TOOL LIST: each tool is a verb of this agent, its arguments
    /// the tool's own `inputSchema`, checked by the one engine like any declared verb.
    fn dynamic_verbs(&self, agent: &Agent) -> Vec<DynVerb> {
        self.get(agent.id.as_str())
            .tools
            .unwrap_or_default()
            .iter()
            .map(|(name, tool)| {
                let desc = tool
                    .get("description")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .or_else(|| tool.get("title").and_then(Value::as_str))
                    .unwrap_or("")
                    .trim();
                DynVerb::new(
                    name.clone(),
                    desc,
                    tool.get("inputSchema").unwrap_or(&Value::Null),
                )
            })
            .collect()
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let (reply, _) = Handler::handle_binary(self, agent_id, call, None, kernel).await?;
        Ok(reply)
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        _body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        use declared::Call;
        match call {
            Call::Reflect(_) => Ok((Some(self.reflect(agent_id, kernel)), None)),
            Call::Boot(_) => Ok((Some(self.boot(agent_id, kernel).await), None)),
            // A TOOL of the server, its arguments already checked against its schema
            Call::Dynamic(tool) => {
                let st = self.get(agent_id.as_str());
                let mirrors = st.mirrors.get(&tool.name).cloned().unwrap_or_default();
                match self
                    .call(agent_id, &tool.name, tool.args, kernel, &mirrors)
                    .await
                {
                    Ok((reply, body)) => Ok((Some(reply), body)),
                    Err(McpError(reply)) => Ok((Some(reply), None)),
                }
            }
        }
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.state.lock().expect("state").remove(agent_id.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
