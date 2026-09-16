//! file_bridge — the fs-transport member of the io family.
//!
//! The disk is OUTSIDE the trust domain: crossing kernel↔disk is an IO edge,
//! gated exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule` ⇒
//! every verb except `reflect` denies with the teaching shape). Open it
//! consciously with `ingress_rule=allow_all_ingress`. Even open, the LAW is
//! running-dir only: the `root` AND every path are clamped inside the dir the
//! kernel runs in (`../`, absolute escapes, outward symlinks all refuse).
//!
//! Each file_bridge owns a `root` directory and a `readonly` flag. The disk
//! clamp + raw IO live in [`fs`] — the ONE disk surface; this bundle is the GATE
//! (ingress rule) + the verb shape over it, and never touches the disk directly.
//!
//! ## Verbs
//!
//! | verb     | args                       | reply                                 |
//! |----------|----------------------------|---------------------------------------|
//! | `reflect`| _none_                     | `{id, sentence, root, readonly, hidden_count, verbs, ingress_rule, egress_rule, sealed}` |
//! | `boot`   | _none_                     | `null`                                |
//! | `list`   | `path?`                    | `{path, files: [{name, path, type, size?}]}` |
//! | `read_stream` | `path`, `offset?`, `length?` | `{path, offset, bytes, next_offset, eof, size}` |
//! | `write_stream`| `path`, `bytes`, `offset?`, `truncate?` | `{path, written, offset, size}` |
//! | `pump`   | `source?`, `source_path`, `sink?`, `sink_path?` | server-side SOURCE→SINK copy |
//! | `delete` | `path`, `recursive?`       | `{path, deleted: true}`               |
//! | `rename` | `old_path`, `new_path`     | `{old_path, new_path}`                |
//! | `mkdir`  | `path`                     | `{path, created: true}`               |
//!
//! There is NO string `read`/`write`: bytes move only over the stream verbs,
//! raw, never base64 — the py-canonical shape. `readonly: true` on the record
//! refuses every mutating verb with the one refusal shape,
//! `reason: "unauthorized"`.

#![deny(missing_docs)]

pub mod fs;

use async_trait::async_trait;
use fantastic_bundle::{py_repr, py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal as rf;
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "file_bridge.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default hidden-file names hidden from `list`. Matches the Python
/// bundle so cross-runtime workdirs stay symmetric.
pub const DEFAULT_HIDDEN: &[&str] = &[".git", ".env", ".fantastic", "node_modules", "__pycache__"];

/// The verb table, in python's `VERBS` order — reflect's `verbs` map, the
/// unknown-verb refusal and the gate all read it.
const VERBS: [(&str, &str); 9] = [
    (
        "reflect",
        "Identity + filesystem root + readonly flag + auth posture (ingress/egress/sealed/see). No args.",
    ),
    ("boot", "No-op. Returns None."),
    (
        "list",
        "args: path:str (default ''). Returns {path, files:[{name, path, type, size?}, ...]}. Hides DEFAULT_HIDDEN entries (.git, .env, .fantastic, …).",
    ),
    (
        "read_stream",
        "args: path:str (req), offset:int=0, length:int=65536. The SOURCE half — returns",
    ),
    (
        "write_stream",
        "args: path:str (req), bytes:bytes (req — ONE raw chunk; a model writes it VERBATIM between `<bytes>` and `</bytes>`, nothing escaped), offset:int? (default =",
    ),
    (
        "pump",
        "The PUMP — a server-side stream copy SOURCE→SINK, chunk by chunk, in ONE",
    ),
    (
        "delete",
        "args: path:str (req), recursive:bool=False. Returns {path, deleted:true}.",
    ),
    (
        "rename",
        "args: old_path:str, new_path:str (both req). Returns {old_path, new_path}. Creates new parent dirs. Refused if readonly.",
    ),
    (
        "mkdir",
        "args: path:str (req). Returns {path, created:true}. Recursive (parents ok). Refused if readonly.",
    ),
];

/// Discovery must work on a sealed bridge — python's `UNGATED`.
const UNGATED: [&str; 1] = ["reflect"];

/// The filesystem bundle.
pub struct FileBundle;

/// A readonly leg refuses every mutating verb — in THE ONE REFUSAL SHAPE.
///
/// This answered a bare `{"error": "agent is readonly"}` until 2026-08-27, when
/// the shared agentic corpus caught it: no `reason`, so a caller could only
/// branch on the prose. `unauthorized` is the code the io SEAL answers with too
/// — both say "this edge is not open for that", and neither is about the
/// caller, which is why neither carries a `side`.
fn readonly_refusal() -> Value {
    rf::refusal_hint(
        rf::UNAUTHORIZED,
        "agent is readonly",
        "this leg accepts reads only; an operator may clear `readonly`",
    )
}

#[async_trait]
impl Bundle for FileBundle {
    /// Refuse an `ingress_rule` no resolver implements. Mirrors py
    /// `file_bridge.validate_meta`: an unknown rule now SEALS rather than erroring,
    /// so the edge is safe either way — but reporting `{"updated": true}` for a
    /// security control that does not exist is its own failure, and the complaint
    /// belongs at the write, next to the typo.
    ///
    /// Only a PRESENT `ingress_rule` is judged, as python judges it: the `auth`
    /// stem is a both-directions shorthand whose ingress half this registry may
    /// legitimately not name.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        if let Some(spec) = meta.get("ingress_rule") {
            if fantastic_io_bridge::authorizer::ingress::unknown(Some(spec)).is_some() {
                return Some(format!(
                    "ingress_rule={} is not a rule this bundle implements — \
                     legal values: allow_all_ingress, deny_ingress",
                    py_repr_value(spec)
                ));
            }
        }
        password_refusal(meta, "fs edge")
    }

    fn name(&self) -> &str {
        "file_bridge"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb_value = payload.get("type").cloned().unwrap_or(Value::Null);
        let verb = verb_value.as_str().unwrap_or("");
        // Snapshot meta so concurrent updates don't race.
        let agent = match kernel.agents.get(agent_id) {
            Some(e) => Arc::clone(&e),
            None => return Ok(Some(json!({"error": format!("no agent {agent_id}")}))),
        };
        let meta = agent.meta.read().expect("meta poisoned").clone();
        // UNKNOWN VERB FIRST — before the gate and before any clamp, as python's
        // `handler` looks the verb up before reaching the gate. `shutdown` is not
        // a verb of this bundle, so it is unhandled like any other.
        if !VERBS.iter().any(|(n, _)| *n == verb) {
            let mut v = rf::refusal(
                rf::UNHANDLED,
                format!("file_bridge: unknown type {}", py_repr_value(&verb_value)),
            );
            rf::with(&mut v, "verb", verb_value);
            return Ok(Some(v));
        }
        // GATE — the fs edge is an io_bridge leg: SEALED by default. Discovery
        // (`reflect`) is the only exemption; `boot` is gated like the rest.
        if !UNGATED.contains(&verb) {
            if let Some(denied) = gate(&meta, agent_id, verb) {
                return Ok(Some(denied));
            }
        }
        let base = workdir_base(kernel);
        let readonly = py_truthy(meta.get("readonly").unwrap_or(&Value::Null));
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, &meta, &base),
            "boot" => Value::Null,
            // The PUMP coordinates a SOURCE→SINK copy by id; it never touches
            // bytes/root itself, so it rides without a clamp.
            "pump" => pump_reply(agent_id, payload, kernel).await,
            other => {
                // THE RUNNING-DIR LAW. The clamp is resolved here but applied
                // AFTER each verb's argument checks — python reaches `fs` only
                // once the arguments are good, so a bad argument on a bridge with
                // an escaping root answers `bad_arg`, not `transport_error`.
                let root = fs::resolve_root(&root_str(&meta), &base);
                match other {
                    "list" => list_reply(&root, payload, &hidden_list(&meta)),
                    // THE STREAM VERBS ON THE TEXT CHANNEL, as python serves
                    // them: a model writes a document VERBATIM between `<bytes>`
                    // and `</bytes>` and python's parser delivers that as utf-8
                    // bytes to the same handler; a chunk read back renders utf-8
                    // in the reply. The binary channel stays for real blobs.
                    "read_stream" => read_stream_reply(&root, payload, true).0,
                    "write_stream" => match payload.get("bytes") {
                        // Absent ⇒ python's `payload.get("bytes", b"")`: an empty
                        // chunk, which is a legal write.
                        None => write_stream_reply(&root, payload, b"", readonly),
                        Some(Value::String(s)) => {
                            write_stream_reply(&root, payload, s.as_bytes(), readonly)
                        }
                        // The tag grammar types a bare number; a number is a document too.
                        Some(Value::Number(n)) => {
                            write_stream_reply(&root, payload, n.to_string().as_bytes(), readonly)
                        }
                        Some(_) if readonly => readonly_refusal(),
                        Some(_) => missing_bytes(),
                    },
                    "delete" => delete_reply(&root, payload, readonly),
                    "rename" => rename_reply(&root, payload, readonly),
                    "mkdir" => mkdir_reply(&root, payload, readonly),
                    _ => unreachable!("every VERBS entry is dispatched"),
                }
            }
        };
        Ok(Some(reply))
    }

    /// Binary channel: `read_stream` (empty request blob → reply BODY = the
    /// raw chunk) and `write_stream` (request blob = the raw chunk → status
    /// reply, no body). Symmetric with Python's `bytes`-in-the-dict; never
    /// base64. Any other verb falls back to the default (→`handle`).
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Vec<u8>), BundleError> {
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        if !matches!(verb, "read_stream" | "write_stream") {
            // file_bridge has no other binary verb — route the header through
            // the text path (which gates + dispatches); the blob is unused.
            let reply = self.handle(agent_id, &header, kernel).await?;
            return Ok((reply, Vec::new()));
        }
        let agent = match kernel.agents.get(agent_id) {
            Some(e) => Arc::clone(&e),
            None => {
                return Ok((
                    Some(json!({"error": format!("no agent {agent_id}")})),
                    Vec::new(),
                ))
            }
        };
        let meta = agent.meta.read().expect("meta poisoned").clone();
        // GATE — same sealed-by-default choke point as the text channel.
        if let Some(denied) = gate(&meta, agent_id, verb) {
            return Ok((Some(denied), Vec::new()));
        }
        let root = fs::resolve_root(&root_str(&meta), &workdir_base(kernel));
        let readonly = py_truthy(meta.get("readonly").unwrap_or(&Value::Null));
        match verb {
            "read_stream" => {
                let (reply, body) = read_stream_reply(&root, &header, false);
                Ok((Some(reply), body))
            }
            "write_stream" => {
                // THE BLOB IS THE CHUNK on this channel. python's binary leg
                // PLACES the body at `payload["bytes"]`, so a header that
                // already carries a `bytes` key is a JSON value where raw bytes
                // belong — `missing("write_stream", "bytes (one raw chunk)")`,
                // whatever it holds. An ABSENT key leaves the blob as the
                // chunk (empty blob ⇒ `written: 0`).
                if header.get("bytes").is_some() {
                    return Ok((
                        Some(rf::missing("write_stream", "bytes (one raw chunk)")),
                        Vec::new(),
                    ));
                }
                Ok((
                    Some(write_stream_reply(&root, &header, &blob, readonly)),
                    Vec::new(),
                ))
            }
            _ => unreachable!("only the two stream verbs reach here"),
        }
    }
}

/// The sealed-by-default GATE — the fs edge is an io_bridge leg. Mirrors py
/// file_bridge's `gate_inbound` choke point. Returns `Some(error)` on deny,
/// `None` to admit. Shared by the text (`handle`) and binary (`handle_binary`)
/// channels so a sealed bridge refuses streamed bytes exactly like a text call.
fn gate(meta: &Map<String, Value>, agent_id: &AgentId, verb: &str) -> Option<Value> {
    let spec = fantastic_io_bridge::authorizer::manifold_spec(
        meta.get("ingress_rule"),
        meta.get("auth"),
        "_ingress",
    );
    let rule = match fantastic_io_bridge::authorizer::ingress::resolve(spec.as_ref()) {
        Ok(r) => r,
        Err(e) => return Some(json!({"error": e})),
    };
    // NO token: the fs edge has no wire envelope, so nothing can present one.
    // It used to read `auth_token` off the RECORD — a persisted secret in the
    // portable workdir satisfied a `password_ingress` leg, the one thing the
    // password rule exists to prevent. `password_ingress` is now refused at
    // `validate_meta` (#116); the gate never reads the record for a credential.
    let action = fantastic_io_bridge::authorizer::Action {
        kind: "call",
        target: agent_id.0.as_str(),
        verb,
        token: None,
    };
    // FORWARDED, not re-worded. This hint was hardcoded to say "the fs edge" for
    // whichever leg reached this gate, and it was a second, shorter wording of a
    // repair the RULE already states — so the same seal read differently per edge
    // and differently from python.
    if let fantastic_io_bridge::authorizer::Decision::Deny(d) =
        fantastic_io_bridge::gate_inbound(&rule, &action)
    {
        return Some(d.to_refusal());
    }
    None
}

/// The clamped root, or the transport refusal python answers when the configured
/// root escapes the running dir. Every data verb reaches it only after its
/// argument checks.
fn clamped(root: &Result<PathBuf, String>) -> Result<&Path, Value> {
    match root {
        Ok(p) => Ok(p.as_path()),
        Err(e) => Err(rf::refusal(rf::TRANSPORT_ERROR, e.clone())),
    }
}

/// python's `_root_str`: the configured root string (`rec.get("root", "") or ""`).
fn root_str(meta: &Map<String, Value>) -> String {
    match meta.get("root") {
        Some(v) if py_truthy(v) => py_str(v),
        _ => String::new(),
    }
}

/// python's `_hidden`: `list(rec.get("hidden", DEFAULT_HIDDEN))` — the stored
/// list VERBATIM (duplicates and non-strings included; `hidden_count` counts
/// them, and only the strings filter a listing).
///
/// A non-iterable `hidden` (a number, a bool, `null`) raises `TypeError` out of
/// python's handler; this runtime has no exception to raise and reads it as no
/// entries (D3 residual).
fn hidden_list(meta: &Map<String, Value>) -> Vec<Value> {
    match meta.get("hidden") {
        None => DEFAULT_HIDDEN.iter().map(|s| json!(s)).collect(),
        Some(Value::Array(a)) => a.clone(),
        Some(Value::String(s)) => s.chars().map(|c| json!(c.to_string())).collect(),
        Some(Value::Object(o)) => o.keys().map(|k| json!(k)).collect(),
        Some(_) => Vec::new(),
    }
}

fn is_hidden(name: &str, hidden: &[Value]) -> bool {
    hidden.iter().any(|v| v.as_str() == Some(name))
}

/// The SOURCE half — read ONE raw chunk at `offset`. `text_channel` carries the
/// chunk in the reply's `bytes` key (python's shape); the binary channel carries
/// it in the reply BODY. Stateless cursor (no open handle): pull the next chunk
/// by calling again with `offset=next_offset`. Mirrors py `_read_stream`.
fn read_stream_reply(
    root: &Result<PathBuf, String>,
    payload: &Value,
    text_channel: bool,
) -> (Value, Vec<u8>) {
    let path_str = match path_arg("read_stream", payload, "path") {
        Ok(p) => p,
        Err(e) => return (e, Vec::new()),
    };
    let offset = match int_arg("read_stream", payload, "offset", 0, 0) {
        Ok(n) => n,
        Err(e) => return (e, Vec::new()),
    };
    let length = match int_arg("read_stream", payload, "length", 65536, 1) {
        Ok(n) => n,
        Err(e) => return (e, Vec::new()),
    };
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return (e, Vec::new()),
    };
    match fs::is_file(root, path_str) {
        Ok(true) => {}
        Ok(false) => {
            let mut v = rf::refusal(
                rf::NOT_FOUND,
                format!("file {} not found", py_repr(path_str)),
            );
            rf::with(&mut v, "path", json!(path_str));
            return (v, Vec::new());
        }
        Err(e) => return (rf::refusal(rf::TRANSPORT_ERROR, e), Vec::new()),
    }
    let (chunk, size) = match fs::read_chunk(root, path_str, offset, length) {
        Ok(c) => c,
        Err(e) => return (rf::refusal(rf::TRANSPORT_ERROR, e), Vec::new()),
    };
    let next_offset = offset + chunk.len() as u64;
    let mut out = Map::new();
    out.insert("path".to_string(), json!(path_str));
    out.insert("offset".to_string(), json!(offset));
    if text_channel {
        out.insert(
            "bytes".to_string(),
            json!(String::from_utf8_lossy(&chunk).into_owned()),
        );
    }
    out.insert("next_offset".to_string(), json!(next_offset));
    out.insert("eof".to_string(), json!(next_offset >= size));
    out.insert("size".to_string(), json!(size));
    let body = if text_channel { Vec::new() } else { chunk };
    (Value::Object(out), body)
}

/// python's `missing("write_stream", "bytes (one raw chunk)")`.
fn missing_bytes() -> Value {
    rf::missing("write_stream", "bytes (one raw chunk)")
}

/// A string argument that is a ROOT-RELATIVE path: absent/null ⇒ `""`; a
/// non-string or an absolute path is `bad_arg` (the absolute form is refused,
/// never silently re-rooted — python used to `lstrip("/")` against its own
/// docstring).
fn path_arg<'a>(verb: &str, payload: &'a Value, name: &str) -> Result<&'a str, Value> {
    let p = match payload.get(name) {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(rf::bad(verb, name, "a string")),
    };
    if p.starts_with('/') {
        return Err(rf::bad(verb, name, "a root-relative path (no leading '/')"));
    }
    Ok(p)
}

/// An integer argument with a floor: absent/null ⇒ `default`; anything that is
/// not a JSON integer (a bool, a float, a string) or is below `minimum` is
/// `bad_arg` naming the arg. Mirrors py `_int_arg` — a negative `offset` used
/// to trap swift and a huge `length` abort rust; now every runtime refuses.
fn int_arg(
    verb: &str,
    payload: &Value,
    name: &str,
    default: u64,
    minimum: u64,
) -> Result<u64, Value> {
    match payload.get(name) {
        None | Some(Value::Null) => Ok(default),
        Some(Value::Bool(_)) => Err(rf::bad(verb, name, &format!("an integer >= {minimum}"))),
        Some(v) => match v.as_u64() {
            Some(n) if n >= minimum => Ok(n),
            _ => Err(rf::bad(verb, name, &format!("an integer >= {minimum}"))),
        },
    }
}

/// `path_arg` for a MUTATING verb: the path must name something INSIDE the
/// root — empty/`.` is `missing_arg`, anything resolving to the root itself is
/// `bad_arg`. `delete {path:"", recursive:true}` used to remove the whole
/// served store (#97): the root is the boundary, not an operand.
fn target_arg<'a>(
    verb: &str,
    root: &Result<PathBuf, String>,
    payload: &'a Value,
    name: &str,
) -> Result<&'a str, Value> {
    let p = path_arg(verb, payload, name)?;
    if p.is_empty() || p == "." {
        return Err(rf::missing(verb, name));
    }
    let root = clamped(root)?;
    match fs::resolve(root, p) {
        Ok(target) if target == root => Err(rf::bad(
            verb,
            name,
            "a path inside the root, not the root itself",
        )),
        Ok(_) => Ok(p),
        Err(e) => Err(rf::refusal(rf::TRANSPORT_ERROR, e)),
    }
}

/// The SINK half — write ONE raw chunk at `offset` (default: append at end).
/// `truncate:true` on the first chunk starts fresh. Returns
/// `{path, written, offset, size}`. Mirrors py `_write_stream`.
fn write_stream_reply(
    root: &Result<PathBuf, String>,
    payload: &Value,
    blob: &[u8],
    readonly: bool,
) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let path_str = match path_arg("write_stream", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    // python's `_int_arg(..., "offset", -1, 0)`: absent ⇒ append at the end.
    let off_in = match payload.get("offset") {
        None | Some(Value::Null) => None,
        Some(_) => match int_arg("write_stream", payload, "offset", 0, 0) {
            Ok(n) => Some(n),
            Err(e) => return e,
        },
    };
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let truncate = py_truthy(payload.get("truncate").unwrap_or(&Value::Null));
    match fs::write_chunk(root, path_str, blob, off_in, truncate) {
        Ok((offset, size)) => json!({
            "path": path_str,
            "written": blob.len(),
            "offset": offset,
            "size": size,
        }),
        Err(e) => rf::refusal(rf::TRANSPORT_ERROR, e),
    }
}

/// The PUMP — a server-side SOURCE→SINK copy, chunk by chunk, in ONE call.
/// Storage-agnostic: both ends are bound BY ID + the duck-typed stream verbs, so
/// a `network_bridge` SOURCE pumps to a `file_bridge` SINK the same as fs→fs.
/// Each end SELF-gates + SELF-clamps — the pump only coordinates, never touching
/// bytes, so a sealed end refuses it. Mirrors py `_pump`.
async fn pump_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let self_id = json!(agent_id.0.as_str());
    let truthy_or = |key: &str, fallback: Value| -> Value {
        match payload.get(key) {
            Some(v) if py_truthy(v) => v.clone(),
            _ => fallback,
        }
    };
    let src = truthy_or("source", self_id.clone());
    let sink = truthy_or("sink", self_id);
    let spath = truthy_or("source_path", truthy_or("path", json!("")));
    let dpath = truthy_or("sink_path", spath.clone());
    let chunk = match int_arg("pump", payload, "chunk", 65536, 1) {
        Ok(n) => n,
        Err(e) => return e,
    };
    let src_id = AgentId::from(target_id(&src));
    let sink_id = AgentId::from(target_id(&sink));
    let (mut offset, mut first, mut chunks) = (0u64, true, 0u64);
    loop {
        let (rmeta, body) = kernel
            .send_with_binary(
                &src_id,
                json!({"type": "read_stream", "path": spath, "offset": offset, "length": chunk}),
                Vec::new(),
            )
            .await;
        // python tests the reply for a `bytes` chunk; on this channel the chunk
        // rides the body, so a reply that carries an `error` instead is the same
        // "no chunk came back".
        if !rmeta.is_object() || rmeta.get("error").is_some() {
            return rf::refusal(
                rf::TRANSPORT_ERROR,
                format!(
                    "pump: read from {} failed: {}",
                    py_repr_value(&src),
                    py_str(&rmeta)
                ),
            );
        }
        let (wmeta, _) = kernel
            .send_with_binary(
                &sink_id,
                json!({"type": "write_stream", "path": dpath, "offset": offset, "truncate": first}),
                body,
            )
            .await;
        if !wmeta.is_object() || py_truthy(wmeta.get("error").unwrap_or(&Value::Null)) {
            return rf::refusal(
                rf::TRANSPORT_ERROR,
                format!(
                    "pump: write to {} failed: {}",
                    py_repr_value(&sink),
                    py_str(&wmeta)
                ),
            );
        }
        first = false;
        chunks += 1;
        let next = rmeta.get("next_offset").and_then(Value::as_u64);
        if py_truthy(rmeta.get("eof").unwrap_or(&Value::Null)) {
            offset = next.unwrap_or(offset);
            break;
        }
        match next {
            Some(n) if n > offset => offset = n,
            // No forward progress and no eof: a SOURCE that would loop forever.
            _ => {
                return rf::refusal(
                    rf::TRANSPORT_ERROR,
                    format!(
                        "pump: source {} made no progress at offset {offset}",
                        py_repr_value(&src)
                    ),
                )
            }
        }
    }
    json!({"source": spath, "sink": dpath, "bytes": offset, "chunks": chunks})
}

/// The id a pump end addresses: a string as written, anything else as python
/// would print it into the `send` target.
fn target_id(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_str(other),
    }
}

/// The kernel's RUNNING DIRECTORY — the dir that holds its `.fantastic` (the
/// root loader's `root_path` parent). In production this IS the process cwd (you
/// run the kernel in your project dir), so the clamp matches Python's
/// `Path.cwd()`; deriving it from the root's location instead of reading the
/// process cwd keeps it robust under embedding/tests where they can differ.
fn workdir_base(kernel: &Kernel) -> PathBuf {
    kernel
        .root()
        .and_then(|r| r.root_path.parent().map(|p| p.to_path_buf()))
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `password_ingress` is meaningless on an in-process leg: the token is read
/// off a WIRE ENVELOPE, and the fs/process edges have none — no frame ever
/// presents one, so the rule could only ever refuse (python) or, worse, be
/// satisfied by a secret persisted ON THE RECORD (the ports, until
/// 2026-09-02). Refused at the write, next to the mistake (#116). Both
/// spellings: the scoped `ingress_rule` and the `auth` stem. Shared with
/// `fantastic-exec-bridge`.
pub fn password_refusal(meta: &Map<String, Value>, edge: &str) -> Option<String> {
    for (field, stem) in [("ingress_rule", "password_ingress"), ("auth", "password")] {
        let Some(spec) = meta.get(field) else {
            continue;
        };
        if fantastic_io_bridge::authorizer::rule_name(Some(spec), "") == stem {
            return Some(format!(
                "{field}={} is not applicable to the {edge}: a leg with no wire \
                 envelope can never present a token — legal values: \
                 allow_all_ingress, deny_ingress",
                py_repr_value(spec)
            ));
        }
    }
    None
}

/// python's `_reflect`: identity + the resolved root + the readonly flag + the
/// auth posture, in python's key order. It NEVER refuses — a root that escapes
/// the running dir is shown as configured, with the violation in `root_error`.
fn reflect_reply(agent_id: &AgentId, meta: &Map<String, Value>, base: &Path) -> Value {
    let raw_root = root_str(meta);
    let (root_display, root_error) = match fs::resolve_root(&raw_root, base) {
        Ok(p) => (p.to_string_lossy().into_owned(), None),
        Err(e) => (raw_root, Some(e)),
    };
    let mut verbs = Map::new();
    for (name, doc) in VERBS {
        verbs.insert(name.to_string(), json!(doc));
    }
    let mut out = Map::new();
    out.insert("id".to_string(), json!(agent_id.as_str()));
    out.insert(
        "sentence".to_string(),
        json!("Filesystem edge of the io family — sealed by default, running-dir only."),
    );
    out.insert("root".to_string(), json!(root_display));
    out.insert(
        "readonly".to_string(),
        json!(py_truthy(meta.get("readonly").unwrap_or(&Value::Null))),
    );
    out.insert("hidden_count".to_string(), json!(hidden_list(meta).len()));
    out.insert("verbs".to_string(), Value::Object(verbs));
    // The auth POSTURE, python's `describe(rec)`: rule NAMES (never config),
    // keyed by the field a caller WRITES (read-key == write-key), + `sealed`.
    if let Some(posture) = fantastic_io_bridge::authorizer::describe(meta).as_object() {
        for (k, v) in posture {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(e) = root_error {
        out.insert("root_error".to_string(), json!(e));
    }
    Value::Object(out)
}

fn list_reply(root: &Result<PathBuf, String>, payload: &Value, hidden: &[Value]) -> Value {
    let path_str = match path_arg("list", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return e,
    };
    // Python's branches, in python's order: absent ⇒ not_found (+path); present
    // but not a dir ⇒ bad_arg; the OS refusing the walk ⇒ transport_error.
    let entries = match list_entries(root, path_str) {
        Ok(Listing::Entries(e)) => e,
        Ok(Listing::Absent) => {
            let mut v = rf::refusal(
                rf::NOT_FOUND,
                format!("path {} does not exist", py_repr(path_str)),
            );
            rf::with(&mut v, "path", json!(path_str));
            return v;
        }
        Ok(Listing::NotADir) => {
            return rf::bad(
                "list",
                "path",
                &format!("a directory; {} is not one", py_repr(path_str)),
            )
        }
        Err(e) => return rf::refusal(rf::TRANSPORT_ERROR, e),
    };
    let mut files: Vec<Value> = Vec::new();
    for entry in entries {
        let name = fs::file_name(&entry);
        if is_hidden(&name, hidden) {
            continue;
        }
        // Relative to the RESOLVED root, as python's `entry.relative_to(rdir)`.
        let rel = entry
            .strip_prefix(root)
            .unwrap_or(&entry)
            .to_string_lossy()
            .into_owned();
        let mut obj = Map::new();
        obj.insert("name".to_string(), json!(name));
        obj.insert("path".to_string(), json!(rel));
        if entry.is_dir() {
            obj.insert("type".to_string(), json!("dir"));
        } else {
            obj.insert("type".to_string(), json!("file"));
            // python: a stat that fails (a dangling symlink) reports -1.
            let size = std::fs::metadata(&entry).map_or(-1, |m| m.len() as i64);
            obj.insert("size".to_string(), json!(size));
        }
        files.push(Value::Object(obj));
    }
    json!({"path": path_str, "files": files})
}

/// What `list` found at the path — python's three branches, told apart before
/// any of them is rendered.
enum Listing {
    Absent,
    NotADir,
    Entries(Vec<PathBuf>),
}

fn list_entries(root: &Path, path: &str) -> Result<Listing, String> {
    if !fs::exists(root, path)? {
        return Ok(Listing::Absent);
    }
    if !fs::is_dir(root, path)? {
        return Ok(Listing::NotADir);
    }
    Ok(Listing::Entries(fs::list_dir(root, path)?))
}

/// `delete path recursive?` — a file, an EMPTY dir, or (`recursive:true`) a
/// dir tree. A symlink is UNLINKED, never followed (so a dangling one is still
/// a deletable name). A bare delete on a populated dir refuses like python's
/// `rmdir` (it used to `remove_dir_all` regardless, #57).
fn delete_reply(root: &Result<PathBuf, String>, payload: &Value, readonly: bool) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let path_str = match target_arg("delete", root, payload, "path") {
        Ok(t) => t,
        Err(e) => return e,
    };
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return e,
    };
    // `exists` follows symlinks — a DANGLING link is still a deletable name.
    match (fs::exists(root, path_str), fs::is_symlink(root, path_str)) {
        (Ok(false), Ok(false)) => {
            let mut v = rf::refusal(
                rf::NOT_FOUND,
                format!("path {} does not exist", py_repr(path_str)),
            );
            rf::with(&mut v, "path", json!(path_str));
            return v;
        }
        (Err(e), _) | (_, Err(e)) => return rf::refusal(rf::TRANSPORT_ERROR, e),
        _ => {}
    }
    let recursive = py_truthy(payload.get("recursive").unwrap_or(&Value::Null));
    match fs::remove(root, path_str, recursive) {
        Ok(()) => json!({"path": path_str, "deleted": true}),
        Err(e) => rf::refusal(rf::TRANSPORT_ERROR, e),
    }
}

fn rename_reply(root: &Result<PathBuf, String>, payload: &Value, readonly: bool) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let old = match target_arg("rename", root, payload, "old_path") {
        Ok(t) => t,
        Err(e) => return e,
    };
    let new = match target_arg("rename", root, payload, "new_path") {
        Ok(t) => t,
        Err(e) => return e,
    };
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match fs::exists(root, old) {
        Ok(true) => {}
        Ok(false) => {
            let mut v = rf::refusal(
                rf::NOT_FOUND,
                format!("path {} does not exist", py_repr(old)),
            );
            rf::with(&mut v, "path", json!(old));
            return v;
        }
        Err(e) => return rf::refusal(rf::TRANSPORT_ERROR, e),
    }
    match fs::rename(root, old, new) {
        Ok(()) => json!({"old_path": old, "new_path": new}),
        Err(e) => rf::refusal(rf::TRANSPORT_ERROR, e),
    }
}

fn mkdir_reply(root: &Result<PathBuf, String>, payload: &Value, readonly: bool) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let path_str = match target_arg("mkdir", root, payload, "path") {
        Ok(t) => t,
        Err(e) => return e,
    };
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match fs::mkdir(root, path_str) {
        Ok(()) => json!({"path": path_str, "created": true}),
        Err(e) => rf::refusal(rf::TRANSPORT_ERROR, e),
    }
}

#[cfg(test)]
mod tests;
