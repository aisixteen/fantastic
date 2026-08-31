//! Filesystem-as-agent.
//!
//! Each agent owns a `root` directory and a `readonly` flag. Path
//! safety: every verb resolves `root/path` and refuses if the result
//! escapes `root`.
//!
//! ## Verbs
//!
//! | verb     | args                       | reply                                 |
//! |----------|----------------------------|---------------------------------------|
//! | `reflect`| _none_                     | `{id, sentence, root, readonly, hidden_count, verbs}` |
//! | `boot`   | _none_                     | `null`                                |
//! | `list`   | `path?`                    | `{path, files: [{name, path, type, size?}]}` |
//! | `read_stream` | `path`, `offset?`, `length?` | binary channel — reply body = one raw chunk |
//! | `write_stream`| `path`, `offset?`, `truncate?` | binary channel — request body = one raw chunk |
//! | `pump`   | `source?`, `source_path`, `sink?`, `sink_path?` | server-side SOURCE→SINK copy |
//! | `delete` | `path`                     | `{path, deleted: true}`               |
//! | `rename` | `old_path`, `new_path`     | `{old_path, new_path}`                |
//! | `mkdir`  | `path`                     | `{path, created: true}`               |
//!
//! There is NO string `read`/`write`: bytes move only over the stream verbs
//! (`read_stream`/`write_stream`), raw on the binary channel, never base64 — the
//! py-canonical shape. `readonly: true` on the record refuses every mutating verb
//! with the one refusal shape, `reason: "unauthorized"` (#27).

#![deny(missing_docs)]

use async_trait::async_trait;
use fantastic_bundle as _; // dep keeps the bundle ↔ kernel link explicit
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "file_bridge.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// Default hidden-file names hidden from `list`. Matches the Python
/// bundle so cross-runtime workdirs stay symmetric.
pub const DEFAULT_HIDDEN: &[&str] = &[".git", ".env", ".fantastic", "node_modules", "__pycache__"];

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
    fantastic_kernel::refusal::refusal_hint(
        fantastic_kernel::refusal::UNAUTHORIZED,
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
    fn validate_meta(
        &self,
        meta: &serde_json::Map<String, Value>,
        _record: &serde_json::Map<String, Value>,
    ) -> Option<String> {
        // Read the SAME pair the gate reads (`ingress_rule` else the `auth` stem).
        // It used to read only `ingress_rule`, so a leg configured via `auth` could
        // validate clean and then behave differently at the gate — two truths.
        let spec = fantastic_io_bridge::authorizer::manifold_spec(
            meta.get("ingress_rule"),
            meta.get("auth"),
            "_ingress",
        )?;
        let rule = fantastic_io_bridge::authorizer::ingress::resolve(Some(&spec)).ok()?;
        let probe = fantastic_io_bridge::authorizer::Action {
            kind: "call",
            target: "",
            verb: "reflect",
            token: None,
        };
        match rule.authorize(&probe) {
            fantastic_io_bridge::authorizer::Decision::Deny(r)
                if r.contains("unknown ingress rule") =>
            {
                Some(format!(
                    "ingress_rule={spec} is not a rule this bundle implements — \
                 legal values: allow_all, deny_inbound, password"
                ))
            }
            _ => None,
        }
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
        kernel: &std::sync::Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        // Snapshot meta so concurrent updates don't race.
        let agent = match kernel.agents.get(agent_id) {
            Some(e) => std::sync::Arc::clone(&e),
            None => return Ok(Some(json!({"error": format!("no agent {agent_id}")}))),
        };
        let meta = agent.meta.read().expect("meta poisoned").clone();
        // `raw_root` is the configured root verbatim (reflect shows it). Data
        // verbs operate on the CLAMPED root (running-dir law) — resolved below.
        let raw_root = root_of(&meta);
        let readonly = meta
            .get("readonly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let hidden: Vec<String> = match meta.get("hidden").and_then(Value::as_array) {
            Some(arr) => arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            None => DEFAULT_HIDDEN.iter().map(|s| s.to_string()).collect(),
        };

        // GATE — the fs edge is an io_bridge leg: SEALED by default (see `gate`).
        if let Some(denied) = gate(&meta, agent_id, verb) {
            return Ok(Some(denied));
        }

        let reply = match verb {
            // Discovery shows the configured root verbatim (no clamp — py parity).
            "reflect" => reflect_reply(agent_id, &raw_root, readonly, &hidden),
            "boot" | "shutdown" => Value::Null,
            // The PUMP coordinates a SOURCE→SINK copy by id; it never touches
            // bytes/root itself, so it rides the text channel without a clamp.
            "pump" => pump_reply(agent_id, payload, kernel).await,
            // Stream verbs carry RAW BYTES — binary channel only.
            "read_stream" | "write_stream" => json!({
                "error": format!(
                    "{verb} carries raw bytes — call it on the binary channel \
                     (send_with_binary), not text send"
                ),
            }),
            // Every data verb clamps the root to the running dir FIRST (a root
            // that escapes it refuses — the running-dir law).
            _ => {
                let root = match clamp_root(&raw_root, &workdir_base(kernel)) {
                    Ok(r) => r,
                    Err(e) => {
                        return Ok(Some(
                            json!({"error": e, "reason": "root_escapes_running_dir"}),
                        ))
                    }
                };
                match verb {
                    "list" => list_reply(&root, payload, &hidden),
                    "delete" if readonly => readonly_refusal(),
                    "delete" => delete_reply(&root, payload),
                    "rename" if readonly => readonly_refusal(),
                    "rename" => rename_reply(&root, payload),
                    "mkdir" if readonly => readonly_refusal(),
                    "mkdir" => mkdir_reply(&root, payload),
                    other => json!({"error": format!("unknown verb {other:?}")}),
                }
            }
        };
        Ok(Some(reply))
    }

    /// Binary channel: `read_stream` (empty request blob → reply BODY = the
    /// raw chunk) and `write_stream` (request blob = the raw chunk → status
    /// reply, no body). Symmetric with Python's `bytes`-in-the-dict; never
    /// base64. Any other verb falls back to the default (base64→`handle`).
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
            Some(e) => std::sync::Arc::clone(&e),
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
        // Running-dir law — clamp the root for stream ops too.
        let root = match clamp_root(&root_of(&meta), &workdir_base(kernel)) {
            Ok(r) => r,
            Err(e) => {
                return Ok((
                    Some(json!({"error": e, "reason": "root_escapes_running_dir"})),
                    Vec::new(),
                ))
            }
        };
        let readonly = meta
            .get("readonly")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        match verb {
            "read_stream" => {
                let (reply, body) = read_stream_reply(&root, &header);
                Ok((Some(reply), body))
            }
            "write_stream" => {
                if readonly {
                    Ok((Some(readonly_refusal()), Vec::new()))
                } else {
                    Ok((Some(write_stream_reply(&root, &header, &blob)), Vec::new()))
                }
            }
            _ => unreachable!(),
        }
    }
}

/// The sealed-by-default GATE — the fs edge is an io_bridge leg. Every verb
/// except discovery/lifecycle (`reflect`/`boot`/`shutdown`) is denied until
/// the leg is opened (`ingress_rule=allow_all_ingress`). Mirrors py file_bridge's
/// `gate_inbound` choke point. Returns `Some(error)` on deny, `None` to admit.
/// Shared by the text (`handle`) and binary (`handle_binary`) channels so a
/// sealed bridge refuses streamed bytes exactly like a text call.
fn gate(meta: &serde_json::Map<String, Value>, agent_id: &AgentId, verb: &str) -> Option<Value> {
    if matches!(verb, "reflect" | "boot" | "shutdown") {
        return None;
    }
    let spec = fantastic_io_bridge::authorizer::manifold_spec(
        meta.get("ingress_rule"),
        meta.get("auth"),
        "_ingress",
    );
    let rule = match fantastic_io_bridge::authorizer::ingress::resolve(spec.as_ref()) {
        Ok(r) => r,
        Err(e) => return Some(json!({"error": e})),
    };
    let action = fantastic_io_bridge::authorizer::Action {
        kind: "call",
        target: agent_id.0.as_str(),
        verb,
        token: meta.get("auth_token").and_then(Value::as_str),
    };
    if let fantastic_io_bridge::authorizer::Decision::Deny(reason) =
        fantastic_io_bridge::gate_inbound(&rule, &action)
    {
        return Some(json!({
            "error": reason,
            "reason": "unauthorized",
            "hint": "the fs edge is sealed; open it: update_agent <id> ingress_rule=allow_all_ingress",
        }));
    }
    None
}

/// The SOURCE half — read ONE raw chunk at `offset`. Returns
/// `({path, offset, next_offset, eof, size}, raw_bytes)`; the bytes ride the
/// binary channel's reply BODY, never base64. Stateless cursor (no open
/// handle): pull the next chunk by calling again with `offset=next_offset`.
/// Mirrors py `file_bridge._read_stream`.
fn read_stream_reply(root: &Path, header: &Value) -> (Value, Vec<u8>) {
    let path_str = header.get("path").and_then(Value::as_str).unwrap_or("");
    let offset = header.get("offset").and_then(Value::as_u64).unwrap_or(0);
    let length = header
        .get("length")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(65536);
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return (json!({"error": e}), Vec::new()),
    };
    if !target.is_file() {
        return (
            json!({"error": format!("file {path_str:?} not found")}),
            Vec::new(),
        );
    }
    let size = match std::fs::metadata(&target) {
        Ok(m) => m.len(),
        Err(e) => {
            return (
                json!({"error": format!("stat {}: {e}", target.display())}),
                Vec::new(),
            )
        }
    };
    let chunk = match read_chunk(&target, offset, length) {
        Ok(b) => b,
        Err(e) => return (json!({"error": e}), Vec::new()),
    };
    let next_offset = offset + chunk.len() as u64;
    let header = json!({
        "path": path_str,
        "offset": offset,
        "next_offset": next_offset,
        "eof": next_offset >= size,
        "size": size,
        "bytes_len": chunk.len(),
    });
    (header, chunk)
}

/// Read up to `length` bytes from `target` starting at `offset` (seek + read).
fn read_chunk(target: &Path, offset: u64, length: u64) -> Result<Vec<u8>, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f =
        std::fs::File::open(target).map_err(|e| format!("open {}: {e}", target.display()))?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("seek {}: {e}", target.display()))?;
    let mut buf = vec![0u8; length as usize];
    let n = f
        .read(&mut buf)
        .map_err(|e| format!("read {}: {e}", target.display()))?;
    buf.truncate(n);
    Ok(buf)
}

/// The SINK half — write ONE raw chunk (`blob`) at `offset` (default: append
/// at end). `truncate:true` on the first chunk starts fresh. Returns
/// `{path, written, offset, size}`. Mirrors py `file_bridge._write_stream`.
fn write_stream_reply(root: &Path, header: &Value, blob: &[u8]) -> Value {
    let path_str = header.get("path").and_then(Value::as_str).unwrap_or("");
    let truncate = header
        .get("truncate")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let off_in = header.get("offset").and_then(Value::as_u64);
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return json!({"error": format!("mkdir parent: {e}")});
        }
    }
    match write_chunk(&target, blob, off_in, truncate) {
        Ok((offset, size)) => json!({
            "path": path_str,
            "written": blob.len(),
            "offset": offset,
            "size": size,
        }),
        Err(e) => json!({"error": e}),
    }
}

/// Write `blob` at `offset` (None ⇒ append at current end). `truncate` opens
/// the file fresh (length 0) before writing. Returns `(write_offset, new_size)`.
fn write_chunk(
    target: &Path,
    blob: &[u8],
    offset: Option<u64>,
    truncate: bool,
) -> Result<(u64, u64), String> {
    use std::io::{Seek, SeekFrom, Write};
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).write(true);
    if truncate {
        opts.truncate(true);
    }
    let mut f = opts
        .open(target)
        .map_err(|e| format!("open {}: {e}", target.display()))?;
    let off = match offset {
        Some(o) => f
            .seek(SeekFrom::Start(o))
            .map_err(|e| format!("seek {}: {e}", target.display()))?,
        None => f
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("seek end {}: {e}", target.display()))?,
    };
    f.write_all(blob)
        .map_err(|e| format!("write {}: {e}", target.display()))?;
    let size = f
        .metadata()
        .map(|m| m.len())
        .map_err(|e| format!("stat {}: {e}", target.display()))?;
    Ok((off, size))
}

/// The PUMP — a server-side SOURCE→SINK copy, chunk by chunk, in ONE call.
/// Storage-agnostic: both ends are bound BY ID + the duck-typed stream verbs
/// over the binary channel, so a `network_bridge` SOURCE pumps to a
/// `file_bridge` SINK the same as fs→fs. Each end SELF-gates + SELF-clamps —
/// the pump only coordinates, never touching bytes, so a sealed end refuses
/// it. Mirrors py `file_bridge._pump`. Returns `{source, sink, bytes, chunks}`.
async fn pump_reply(agent_id: &AgentId, payload: &Value, kernel: &Arc<Kernel>) -> Value {
    let self_id = agent_id.0.as_str();
    let src = payload
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or(self_id);
    let sink = payload
        .get("sink")
        .and_then(Value::as_str)
        .unwrap_or(self_id);
    let spath = payload
        .get("source_path")
        .or_else(|| payload.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let dpath = payload
        .get("sink_path")
        .and_then(Value::as_str)
        .unwrap_or(spath);
    let chunk = payload
        .get("chunk")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(65536);
    let (src_id, sink_id) = (AgentId::from(src), AgentId::from(sink));
    let (mut offset, mut first, mut chunks) = (0u64, true, 0u64);
    loop {
        let (rmeta, body) = kernel
            .send_with_binary(
                &src_id,
                json!({"type": "read_stream", "path": spath, "offset": offset, "length": chunk}),
                Vec::new(),
            )
            .await;
        if let Some(err) = rmeta.get("error") {
            return json!({"error": format!("pump: read from {src:?} failed: {err}")});
        }
        let (wmeta, _) = kernel
            .send_with_binary(
                &sink_id,
                json!({"type": "write_stream", "path": dpath, "offset": offset, "truncate": first}),
                body,
            )
            .await;
        if let Some(err) = wmeta.get("error") {
            return json!({"error": format!("pump: write to {sink:?} failed: {err}")});
        }
        first = false;
        chunks += 1;
        offset = rmeta
            .get("next_offset")
            .and_then(Value::as_u64)
            .unwrap_or(offset);
        if rmeta.get("eof").and_then(Value::as_bool).unwrap_or(true) {
            break;
        }
    }
    json!({"source": spath, "sink": dpath, "bytes": offset, "chunks": chunks})
}

fn root_of(meta: &serde_json::Map<String, Value>) -> PathBuf {
    match meta.get("root").and_then(Value::as_str) {
        Some(s) if !s.is_empty() => expanduser(s),
        _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
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

/// THE RUNNING-DIR LAW — clamp the configured root to the kernel's running
/// directory (`base`). A relative root resolves under `base`; an absolute root
/// is allowed ONLY if it lies inside `base`; `..`/outside escapes refuse.
/// Mirrors py `fs.resolve_root` (whose base is `Path.cwd()`). Returns the
/// clamped absolute root, or an error string. This guarantees a file_bridge
/// can't be pointed outside the kernel's own dir (the single disk surface stays
/// bounded); reaching OUTSIDE is a separate `external` concern (not yet here),
/// not a misconfigured root.
/// THE RUNNING-DIR LAW, exposed.
///
/// `fs` is the single disk surface, so anything that opens a path goes through
/// this clamp — including the one-time COLD bring-up reads that happen before a
/// `file_bridge` AGENT can exist (the roster manifest). Mirrors python, where
/// `static_loader` imports `file_bridge.fs` for exactly that read: the clamp is
/// not bypassed just because the reader is a different bundle.
pub fn clamp_root(raw: &Path, base: &Path) -> Result<PathBuf, String> {
    // Compare LEXICALLY (no canonicalize) so the check doesn't require the dir
    // to exist AND both sides stay in the same path form — canonicalizing only
    // the base would mismatch a non-canonical absolute root on macOS (`/var`
    // vs `/private/var`). The base is absolutized against cwd if relative.
    let base = if base.is_absolute() {
        normalize_lexical(base)
    } else {
        normalize_lexical(&std::env::current_dir().unwrap_or_default().join(base))
    };
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    let norm = normalize_lexical(&joined);
    if !norm.starts_with(&base) {
        return Err(format!(
            "root {} escapes the running dir {}",
            norm.display(),
            base.display()
        ));
    }
    Ok(norm)
}

/// Absolutize-and-fold `.`/`..` lexically (no filesystem access). Used by the
/// running-dir clamp so a root can be validated before its dir exists.
fn normalize_lexical(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if matches!(out.last(), Some(Component::Normal(_))) {
                    out.pop();
                } else {
                    out.push(c);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// Tilde expansion so `root=~/projects/foo` works the same as Python's
/// `Path("...").expanduser()`. Only handles a leading `~` (no `~user`).
fn expanduser(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if s == "~" {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(s)
}

/// Resolve `path` relative to `root`, refusing escapes.
///
/// Walks the components of the caller-supplied `path` (NOT the
/// already-joined absolute form) so we can refuse any `..` that
/// would climb above the root. Returns the joined-and-resolved
/// absolute path on success. Symlink-following is left to the OS
/// at access time — we only guarantee the LITERAL path can't escape.
fn resolve_safe(root: &Path, path: &str) -> Result<PathBuf, String> {
    // An absolute path is a hard escape — refuse, don't silently
    // strip the leading slash.
    if Path::new(path).is_absolute() {
        return Err(format!("path {path:?} escapes root"));
    }
    let rel = path.trim_start_matches('/');
    let mut depth: i32 = 0;
    for c in Path::new(rel).components() {
        match c {
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("path {path:?} escapes root"));
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_) => {
                depth += 1;
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                // An absolute component inside `path` is a hard
                // escape attempt — refuse rather than silently
                // anchoring to filesystem root.
                return Err(format!("path {path:?} escapes root"));
            }
        }
    }
    Ok(root.join(rel))
}

fn reflect_reply(agent_id: &AgentId, root: &Path, readonly: bool, hidden: &[String]) -> Value {
    json!({
        "id": agent_id.as_str(),
        "sentence": "Filesystem root.",
        "root": root.display().to_string(),
        "readonly": readonly,
        "hidden_count": hidden.len(),
        "verbs": {
            "reflect": "Identity + filesystem root + readonly flag. No args.",
            "boot": "No-op. Returns None.",
            "list": "args: path:str (default '').",
            "delete": "args: path:str (req).",
            "rename": "args: old_path:str (req), new_path:str (req).",
            "mkdir": "args: path:str (req).",
            "read_stream": "BINARY channel. args: path:str (req), offset:int=0, length:int=65536. Reply body = one raw chunk.",
            "write_stream": "BINARY channel. args: path:str (req), offset:int? (default append), truncate:bool=false; request body = one raw chunk.",
            "pump": "args: source:str? + source_path:str (req) + sink:str? + sink_path:str? + chunk:int=65536. Server-side SOURCE→SINK copy.",
        }
    })
}

fn list_reply(root: &Path, payload: &Value, hidden: &[String]) -> Value {
    let path_str = payload.get("path").and_then(Value::as_str).unwrap_or("");
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    let dir = if target.is_dir() {
        target.clone()
    } else {
        return json!({"error": format!("path {path_str:?} is not a directory")});
    };
    let mut files: Vec<Value> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => return json!({"error": format!("list {}: {e}", dir.display())}),
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_string();
        if hidden.contains(&name) {
            continue;
        }
        let p = entry.path();
        let rel = match p.strip_prefix(root) {
            Ok(r) => r.display().to_string(),
            Err(_) => p.display().to_string(),
        };
        let mut obj = serde_json::Map::new();
        obj.insert("name".to_string(), json!(name));
        obj.insert("path".to_string(), json!(rel));
        if p.is_dir() {
            obj.insert("type".to_string(), json!("dir"));
        } else {
            obj.insert("type".to_string(), json!("file"));
            if let Ok(meta) = p.metadata() {
                obj.insert("size".to_string(), json!(meta.len()));
            }
        }
        files.push(Value::Object(obj));
    }
    files.sort_by(|a, b| {
        a.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .cmp(b.get("name").and_then(Value::as_str).unwrap_or(""))
    });
    json!({
        "path": path_str,
        "files": files,
    })
}

fn delete_reply(root: &Path, payload: &Value) -> Value {
    let Some(path_str) = payload.get("path").and_then(Value::as_str) else {
        return json!({"error": "delete requires path"});
    };
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    if !target.exists() {
        return json!({"error": format!("path {path_str:?} not found")});
    }
    let result = if target.is_dir() {
        std::fs::remove_dir_all(&target)
    } else {
        std::fs::remove_file(&target)
    };
    match result {
        Ok(_) => json!({"path": path_str, "deleted": true}),
        Err(e) => json!({"error": format!("delete {}: {e}", target.display())}),
    }
}

fn rename_reply(root: &Path, payload: &Value) -> Value {
    let old = payload.get("old_path").and_then(Value::as_str);
    let new = payload.get("new_path").and_then(Value::as_str);
    let (Some(old), Some(new)) = (old, new) else {
        return json!({"error": "rename requires old_path + new_path"});
    };
    let src = match resolve_safe(root, old) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    let dst = match resolve_safe(root, new) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    if let Some(parent) = dst.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return json!({"error": format!("mkdir parent: {e}")});
        }
    }
    match std::fs::rename(&src, &dst) {
        Ok(_) => json!({"old_path": old, "new_path": new}),
        Err(e) => json!({"error": format!("rename: {e}")}),
    }
}

fn mkdir_reply(root: &Path, payload: &Value) -> Value {
    let Some(path_str) = payload.get("path").and_then(Value::as_str) else {
        return json!({"error": "mkdir requires path"});
    };
    let target = match resolve_safe(root, path_str) {
        Ok(p) => p,
        Err(e) => return json!({"error": e}),
    };
    match std::fs::create_dir_all(&target) {
        Ok(_) => json!({"path": path_str, "created": true}),
        Err(e) => json!({"error": format!("mkdir {}: {e}", target.display())}),
    }
}

#[cfg(test)]
mod tests;
