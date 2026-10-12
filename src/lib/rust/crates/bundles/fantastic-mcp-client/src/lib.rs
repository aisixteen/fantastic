//! mcp_client — ONE remote MCP server as an agent (owner, 2026-10-08). Python
//! `mcp_client/tools.py`, text for text.
//!
//! `create_agent handler_module=mcp_client.tools url=<the server's MCP endpoint>
//! credentials={store, ref}` (optional — a keyless server takes none). Its `boot`
//! asks the server who it is (`server/discover`) and lists its tools
//! (`tools/list`, every page); from then on EACH TOOL IS A VERB of this agent:
//! `send {type: <tool>, …its arguments}` is a `tools/call`. `docs of=<this agent>`
//! lists them — the agent's own table ([`Bundle::instance_verb_docs`]), built from
//! each tool's `inputSchema` and `description`. A grant names tools like verbs.
//!
//! Streamable HTTP, DUAL-ERA ([`wire`]): protocol 2026-07-28 (stateless) first; a
//! server that speaks only an `initialize`-era version (2025-11-25, 2025-06-18,
//! 2025-03-26, 2024-11-05) is spoken to in that version, through its session.
//! `reflect` names the version agreed. AUTH = a static key: the ref's `api_key`, read from the credential store before EVERY
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
use std::sync::{Arc, Mutex, OnceLock};

use async_trait::async_trait;
use base64::Engine;
use fantastic_ai_core::credentials::key_reader;
use fantastic_ai_core::schema::{self, credentials_field, Field};
use fantastic_bundle::py_repr_value;
use fantastic_kernel::agent::Agent;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::outcome::Blob;
use fantastic_kernel::refusal::{
    refusal, refusal_hint, with, NOT_CONNECTED, TRANSPORT_ERROR, UNAUTHORIZED, UNHANDLED,
};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};

use crate::wire::{Era, McpError, Mirror, Opts, Session};

/// The handler module a record names.
pub const HANDLER_MODULE: &str = "mcp_client.tools";
/// The bundle's name.
pub const NAME: &str = "mcp_client";
/// What a binary item past the first carries instead of its bytes.
pub const OMITTED: &str = "one binary body per reply — this item's bytes were not sent";

const REFLECT_DOC: &str =
    "No args. → {id, sentence, url, protocol_version (the one agreed at boot; null before), server?, instructions?, tools, skipped?, last_error?}.";
const BOOT_DOC: &str = "No args. Ask the server who it is and list its tools — this agent's verbs. Returns {server, protocol_version, tools:[names], skipped?}.\n\nSent again it RE-LISTS (a server's tools changed). A failure keeps the tools\nalready listed and answers why; `reflect` shows it as `last_error`.";
const NOTES: &str = "A remote MCP server's tools are this agent's verbs (`docs of=<id>`). Record: `url` (the MCP endpoint), `credentials` ({store, ref} holding `api_key`, sent as a Bearer key to that host only; absent = keyless). Wiring: this agent's `can_call` names the store, the store's `callable_by` names this agent. A tool runs on the remote server with the key's rights — the grant on each tool is the consent.";

/// What one server said, per instance.
#[derive(Debug, Default, Clone)]
struct State {
    /// `None` until a listing succeeded.
    tools: Option<Vec<(String, Value)>>,
    docs: Vec<(String, String)>,
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
    /// What each instance agreed with its server (era, version, session id).
    sessions: Mutex<HashMap<String, Session>>,
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
    fn session(&self, id: &str) -> Session {
        self.sessions
            .lock()
            .expect("sessions")
            .get(id)
            .cloned()
            .unwrap_or_default()
    }
    fn set_session(&self, id: &str, s: Session) {
        self.sessions
            .lock()
            .expect("sessions")
            .insert(id.to_string(), s);
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
    matches!(name, "reflect" | "boot" | "shutdown")
        || fantastic_kernel::send::kernel_verb_docs()
            .as_object()
            .is_some_and(|m| m.contains_key(name))
}

/// The record schema the catalog publishes and the write enforces.
#[must_use]
pub fn record_schema() -> &'static Value {
    static SCHEMA: OnceLock<Value> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        json!({"fields": [
            Field::new("url", "str", "the MCP server's endpoint, e.g. https://mcp.example.com/mcp")
                .required()
                .json(),
            credentials_field(false).json(),
        ]})
    })
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

// ─── a tool's doc, from its schema ───────────────────────────────────

fn type_name(s: &Value) -> String {
    let one = |t: &str| match t {
        "string" => "str".to_string(),
        "integer" => "int".to_string(),
        "number" => "float".to_string(),
        "boolean" => "bool".to_string(),
        "array" => "list".to_string(),
        "object" => "dict".to_string(),
        "null" => "null".to_string(),
        other => other.to_string(),
    };
    match s.get("type") {
        Some(Value::Array(ts)) => ts
            .iter()
            .map(|t| t.as_str().map_or_else(|| fantastic_bundle::py_str(t), one))
            .collect::<Vec<_>>()
            .join("|"),
        Some(Value::String(t)) => match t.as_str() {
            "string" | "integer" | "number" | "boolean" | "array" | "object" | "null" => one(t),
            _ => "any".to_string(),
        },
        _ => "any".to_string(),
    }
}

/// `args: a:str (req), b:int?. <description>` — the signature first (what the
/// oneliner tier keeps), the tool's description after it.
#[must_use]
pub fn tool_doc(tool: &Value) -> String {
    let schema = tool.get("inputSchema").cloned().unwrap_or(Value::Null);
    let req: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let sig = match schema.get("properties").and_then(Value::as_object) {
        Some(props) if !props.is_empty() => {
            let args: Vec<String> = props
                .iter()
                .map(|(n, s)| {
                    format!(
                        "{n}:{}{}",
                        type_name(s),
                        if req.contains(&n.as_str()) {
                            " (req)"
                        } else {
                            "?"
                        }
                    )
                })
                .collect();
            format!("args: {}.", args.join(", "))
        }
        _ => "No args.".to_string(),
    };
    let desc = tool
        .get("description")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| tool.get("title").and_then(Value::as_str))
        .unwrap_or("")
        .trim();
    if desc.is_empty() {
        sig
    } else {
        format!("{sig} {desc}")
    }
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
        let shown = fantastic_bundle::py_str(&url);
        let mut out = Map::new();
        out.insert("id".into(), json!(id.as_str()));
        out.insert(
            "sentence".into(),
            json!(format!(
                "The MCP server at {} — its tools are this agent's verbs (docs of={}).",
                if url.is_null() {
                    "None".to_string()
                } else {
                    shown
                },
                id.as_str()
            )),
        );
        out.insert("url".into(), url);
        out.insert(
            "protocol_version".into(),
            json!(self.session(id.as_str()).version),
        );
        out.insert("tools".into(), json!(st.tools.as_ref().map_or(0, Vec::len)));
        for (k, v) in [("server", &st.server), ("instructions", &st.instructions)] {
            if fantastic_bundle::py_truthy(v) {
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
                let err = reply.get("error").map(fantastic_bundle::py_str);
                self.put(id.as_str(), |s| s.error = err);
                reply
            }
        }
    }

    async fn list(&self, id: &AgentId, kernel: &Arc<Kernel>, url: &str) -> Result<Value, McpError> {
        let key = read_key(id, kernel, url).await?;
        let mut sess = self.session(id.as_str());
        let connected = wire::connect(url, key.as_deref(), &mut sess).await;
        self.set_session(id.as_str(), sess.clone());
        let disc = connected?;
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
                    &mut sess,
                    no_note,
                )
                .await;
                self.set_session(id.as_str(), sess.clone());
                let page = page?;
                tools.extend(
                    page.get("tools")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                );
                cursor = page
                    .get("nextCursor")
                    .filter(|c| fantastic_bundle::py_truthy(c))
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
        let mut docs = Vec::new();
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
            // `x-mcp-header` is a 2026-07-28 rule: a legacy server's schema mirrors nothing
            let found = if sess.era == Some(Era::Modern) {
                wire::mirrors(&schema)
            } else {
                Ok(vec![])
            };
            match found {
                Err(why) => {
                    skipped.push(json!({"name": n, "why": why}));
                    continue;
                }
                Ok(found) => {
                    mirrors.insert(n.to_string(), found);
                }
            }
            docs.push((n.to_string(), tool_doc(&t)));
            table.push((n.to_string(), t));
        }
        let server = disc.get("server").cloned().unwrap_or(Value::Null);
        let names: Vec<String> = table.iter().map(|(n, _)| n.clone()).collect();
        let instructions = disc.get("instructions").cloned().unwrap_or(Value::Null);
        self.put(id.as_str(), |s| {
            *s = State {
                tools: Some(table),
                docs,
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
        out.insert("protocol_version".into(), json!(sess.version));
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
        payload: &Value,
        kernel: &Arc<Kernel>,
        mirrors: &[Mirror],
    ) -> Result<(Value, Option<Blob>), McpError> {
        let url = url_of(kernel, id);
        let arguments: Map<String, Value> = payload
            .as_object()
            .map(|m| {
                m.iter()
                    .filter(|(k, _)| *k != "type")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
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
        let mut sess = self.session(id.as_str());
        let result = wire::rpc(
            &url,
            "tools/call",
            params,
            Opts {
                key: key.as_deref(),
                headers,
            },
            &mut sess,
            note,
        )
        .await;
        self.set_session(id.as_str(), sess);
        let result = result?;
        answer(&url, tool, &result)
    }
}

#[async_trait]
impl Bundle for McpClientBundle {
    fn name(&self) -> &str {
        NAME
    }

    fn record_schema(&self) -> Option<Value> {
        Some(record_schema().clone())
    }

    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        record: &Map<String, Value>,
    ) -> Option<String> {
        if let Some(url) = meta.get("url").filter(|v| !v.is_null()) {
            let ok = url
                .as_str()
                .and_then(|s| reqwest::Url::parse(s).ok())
                .is_some_and(|u| {
                    matches!(u.scheme(), "http" | "https")
                        && u.host_str().is_some_and(|h| !h.is_empty())
                        && u.fragment().is_none()
                });
            if !ok {
                return Some(format!(
                    "url={} must be an absolute http(s) URL of the MCP endpoint, with no fragment (e.g. https://mcp.example.com/mcp)",
                    py_repr_value(url)
                ));
            }
        }
        schema::validate(record_schema(), NAME, meta, record)
    }

    fn verb_docs(&self) -> Value {
        json!({"reflect": REFLECT_DOC, "boot": BOOT_DOC})
    }

    fn instance_verb_docs(&self, agent: &Agent) -> Option<Value> {
        let mut out = Map::new();
        out.insert("reflect".into(), json!(REFLECT_DOC));
        out.insert("boot".into(), json!(BOOT_DOC));
        for (n, d) in self.get(agent.id.as_str()).docs {
            out.insert(n, json!(d));
        }
        Some(Value::Object(out))
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(json!({
            "tools_listed": "{tools:[names], skipped:[{name, why}]} — after every listing (boot)",
            "progress": "{tool, progressToken, progress, total?, message?} — a tool's progress",
            "log": "{tool, level, data, logger?} — a log line a tool sent",
        }))
    }

    fn notes_doc(&self) -> Option<Value> {
        Some(json!(NOTES))
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let (reply, _) = self
            .handle_binary(agent_id, payload.clone(), None, kernel)
            .await?;
        Ok(reply)
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        _body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        let verb = header
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        match verb.as_str() {
            "reflect" => return Ok((Some(self.reflect(agent_id, kernel)), None)),
            "boot" => return Ok((Some(self.boot(agent_id, kernel).await), None)),
            _ => {}
        }
        let st = self.get(agent_id.as_str());
        let id = agent_id.as_str();
        let Some(tools) = &st.tools else {
            let why = st
                .error
                .clone()
                .unwrap_or_else(|| "boot has not run".to_string());
            return Ok((
                Some(refusal_hint(
                    NOT_CONNECTED,
                    format!("{id}: the server's tools are not listed — {why}"),
                    format!("send boot to {id} once the server is up"),
                )),
                None,
            ));
        };
        if !tools.iter().any(|(n, _)| *n == verb) {
            let names: Vec<&str> = tools.iter().map(|(n, _)| n.as_str()).collect();
            let listed = if names.is_empty() {
                "none".to_string()
            } else {
                names.join(", ")
            };
            let mut r = refusal_hint(
                UNHANDLED,
                format!(
                    "{id}: the MCP server has no tool {}",
                    fantastic_bundle::py_repr(&verb)
                ),
                format!("its tools: {listed} (docs of={id}); boot re-lists them"),
            );
            with(
                &mut r,
                "verb",
                header.get("type").cloned().unwrap_or(Value::Null),
            );
            return Ok((Some(r), None));
        }
        let mirrors = st.mirrors.get(&verb).cloned().unwrap_or_default();
        match self.call(agent_id, &verb, &header, kernel, &mirrors).await {
            Ok((reply, body)) => Ok((Some(reply), body)),
            Err(McpError(reply)) => Ok((Some(reply), None)),
        }
    }

    async fn on_delete(
        &self,
        agent_id: &AgentId,
        _kernel: &Arc<Kernel>,
    ) -> Result<(), BundleError> {
        self.state.lock().expect("state").remove(agent_id.as_str());
        self.sessions
            .lock()
            .expect("sessions")
            .remove(agent_id.as_str());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
