//! file_bridge — the fs-transport member of the io family.
//!
//! The disk is OUTSIDE the trust domain: crossing kernel↔disk is an IO edge,
//! gated exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule` ⇒
//! every verb except `reflect` denies with the teaching shape). Open it
//! consciously with `ingress_rule=allow_all_ingress`. Even open, the LAW is
//! running-dir only: the `root` AND every path are clamped inside the kernel's
//! running dir (`@state` and `@self` roots are under the state folder and the bridge's own
//! folder; every other root is under the work folder; `../`, absolute escapes, outward symlinks all refuse).
//!
//! Each file_bridge owns a `root` directory and a `readonly` flag. The disk
//! clamp + raw IO live in [`fs`] — the ONE disk surface; this bundle is the GATE
//! (ingress rule) + the verb shape over it, and never touches the disk directly.
//!
//! ## Verbs
//!
//! Declared in [`verbs`] (`reflect`, `docs` render them): `list`, `read`, `write`,
//! `edit`, `append`, `undo`, `read_stream`, `write_stream`, `pump`, `delete`,
//! `rename`, `mkdir`, `watch_path`, `unwatch_path`, `open_view`.
//!
//! The `allow` list is a RECORD field: set at create, changed by `update_agent`.
//!
//! The text verbs are this bridge's own (see [`text`]): a caller edits with ITS
//! OWN grants. Raw bytes move over the stream verbs, never base64.
//! `readonly: true` on the record refuses every mutating verb with the one
//! refusal shape, `reason: "unauthorized"`.

#![deny(missing_docs)]

pub mod fs;
pub mod record;
mod sys;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
mod text;
pub mod verbs;
mod views;
mod watch;

use async_trait::async_trait;
use fantastic_bundle::{is_absolute, py_repr, py_repr_value};
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::refusal as rf;
use fantastic_kernel::{body_at, current_origin, AgentId, Blob, Item, Kernel};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "file_bridge.tools";

/// What the bundle is for — its one short man text.
const MAN: &str = "The filesystem edge: it serves one directory (`root`) to the agents that are granted it, only the paths its `allow` list exposes, sealed until opened. Text documents are read and edited in place, raw bytes move over streams, and every file or folder can be opened as a view agent.";

/// The filesystem bundle. Holds the text verbs' RAM state (undo slots, document
/// locks) for the kernel it is registered in.
#[derive(Default)]
pub struct FileBundle {
    text: text::TextState,
    watch: watch::Watches,
    views: views::Relays,
}

/// A readonly leg refuses every mutating verb — in THE ONE REFUSAL SHAPE.
///
/// No bare `{"error": "agent is readonly"}`: without a `reason` a caller could
/// only branch on the prose. `unauthorized` is the code the io SEAL answers with
/// too — both say "this edge is not open for that", and neither is about the
/// caller, which is why neither carries a `side`.
pub(crate) fn readonly_refusal() -> Value {
    rf::refusal_hint(
        rf::UNAUTHORIZED,
        "agent is readonly",
        "this leg accepts reads only; an operator may clear `readonly`",
    )
}

/// The name of the verb a call carries — what the gate and the clamp judge.
fn verb_name(call: &verbs::Call) -> &str {
    use verbs::Call;
    match call {
        Call::Reflect(_) => "reflect",
        Call::List(_) => "list",
        Call::Read(_) => "read",
        Call::Write(_) => "write",
        Call::Edit(_) => "edit",
        Call::Append(_) => "append",
        Call::Undo(_) => "undo",
        Call::ReadStream(_) => "read_stream",
        Call::WriteStream(_) => "write_stream",
        Call::Pump(_) => "pump",
        Call::Delete(_) => "delete",
        Call::Rename(_) => "rename",
        Call::Mkdir(_) => "mkdir",
        Call::WatchPath(_) => "watch_path",
        Call::UnwatchPath(_) => "unwatch_path",
        Call::OpenView(_) => "open_view",
        Call::Dynamic(c) => &c.name,
    }
}

/// The path a view-scoped verb names: `Some` for the verbs a view may send its
/// bridge, `None` for every other.
fn scoped_path(call: &verbs::Call) -> Option<&str> {
    use verbs::Call;
    match call {
        Call::Read(a) => a.path.as_deref(),
        Call::Write(a) => a.path.as_deref(),
        Call::List(a) => Some(a.path.as_deref().unwrap_or_default()),
        Call::WatchPath(a) => Some(&a.path),
        Call::UnwatchPath(a) => Some(&a.path),
        _ => None,
    }
}

#[async_trait]
impl Handler for FileBundle {
    type Call = verbs::Call;

    /// What the declared record cannot say: a view's path shape.
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        views::view_refusal(meta)
    }

    fn name(&self) -> &str {
        "file_bridge"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    /// A VIEW answers verbs of its own beside the bridge's (`watch`, and for a
    /// folder `reveal`); every other record has none.
    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        views::dynamic_verbs(&agent.record().meta)
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
        Ok(self.handle_binary(agent_id, call, None, kernel).await?.0)
    }

    /// The kernel's dispatch for every verb: `read_stream` (reply BODY = the raw
    /// chunk, whatever the call carried) and `write_stream` (the body at `bytes`
    /// = the raw chunk → status reply, no body). Symmetric with Python's
    /// `bytes`-in-the-dict; never base64. A text verb takes its document from the
    /// body at `bytes`, or from `bytes` as text. A body placed under another key is
    /// not a chunk.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        use verbs::Call;
        let verb = verb_name(&call);
        let chunk = body_at(body.as_ref(), "bytes");
        // Snapshot meta so concurrent updates don't race.
        let agent = match kernel.agents.get(agent_id) {
            Some(e) => Arc::clone(&e),
            None => return Ok((Some(json!({"error": format!("no agent {agent_id}")})), None)),
        };
        let meta = agent.meta.read().expect("meta poisoned").clone();
        // A VIEW is answered by its own verbs, before the bridge's gate (python
        // `file_bridge.tools.handler`): a view is not sealed, its gate is its grant.
        if let Some(kind) = agent
            .record()
            .meta
            .get("view")
            .and_then(Value::as_str)
            .map(str::to_string)
        {
            let reply = views::dispatch(kernel, &self.views, agent_id, &kind, call).await;
            return Ok((Some(reply), None));
        }
        // GATE — the fs edge is an io_bridge leg: SEALED by default. Discovery
        // (`reflect`) is the only exemption.
        if verb != "reflect" {
            if let Some(denied) = gate(&meta, agent_id, verb) {
                return Ok((Some(denied), None));
            }
        }
        // A bridge reads and writes the file a call NAMES (only a view has a file of its own).
        if matches!(
            &call,
            Call::Read(verbs::Read { path: None, .. }) | Call::Write(verbs::Write { path: None, .. })
        ) {
            return Ok((Some(rf::missing(verb, "path")), None));
        }
        // THE PATH GATE FOR A VIEW CALLER — python `tools._view_scope`, after the gate.
        if let Some(rel) = scoped_path(&call) {
            if let Some(refused) =
                view_scope(kernel, agent_id, current_origin().as_ref(), verb, rel)
            {
                return Ok((Some(refused), None));
            }
        }
        let root = clamp_root(&meta, kernel, &agent.root_path);
        let readonly = readonly_flag(&meta);
        let reply = match &call {
            Call::Reflect(_) => reflect_reply(agent_id, &meta, &root),
            Call::OpenView(a) => views::open_view_verb(kernel, &meta, agent_id, a).await,
            Call::Read(_) | Call::Write(_) | Call::Edit(_) | Call::Append(_) | Call::Undo(_) => {
                text::dispatch(&self.text, &agent_id.0, &meta, &root, &call, chunk, readonly)
                    .await
            }
            // The PUMP coordinates a SOURCE→SINK copy by id; it never touches
            // bytes/root itself, so it rides without a clamp.
            Call::Pump(a) => pump_reply(agent_id, a, kernel).await,
            // THE RUNNING-DIR LAW. The clamp is resolved here but applied
            // AFTER each verb's argument checks — python reaches `fs` only
            // once the arguments are good, so a bad argument on a bridge with
            // an escaping root answers `bad_arg`, not `transport_error`.
            // DENY-ALL AT THE PATH LEVEL — after each verb's own argument
            // checks (python's order), before any disk access.
            other => {
                if let Some(refused) = exposure(other, &root, &meta, readonly) {
                    return Ok((Some(refused), None));
                }
                match other {
                    Call::List(a) => {
                        let mut out = list_reply(agent_id, &root, a);
                        keep_exposed(&mut out, &allow_list(&meta), &root);
                        out
                    }
                    // THE JSON HALF of a read: the chunk is the reply body beside it.
                    Call::ReadStream(a) => {
                        let (reply, bytes) = read_stream_reply(agent_id, &root, a);
                        return Ok((Some(reply), bytes.map(|b| Blob::key("bytes", b))));
                    }
                    // THE REQUEST BODY AT `bytes` IS THE CHUNK. Nothing at `bytes`
                    // (no body, or a body under another key) is an empty chunk, which
                    // is a legal write. NO CHUNK COMES BACK from a write — `None`, not
                    // an empty body, so a leg can tell this reply from a 0-byte read.
                    Call::WriteStream(a) => {
                        write_stream_reply(&root, a, Some(chunk.unwrap_or_default()), readonly)
                    }
                    Call::Delete(a) => delete_reply(&root, a, readonly),
                    Call::Rename(a) => rename_reply(&root, a, readonly),
                    Call::Mkdir(a) => mkdir_reply(&root, a, readonly),
                    Call::WatchPath(a) => {
                        let p = a.path.as_str();
                        let r = match clamped(&root) {
                            Ok(r) => r.to_path_buf(),
                            Err(e) => return Ok((Some(e), None)),
                        };
                        self.watch.watch_path(agent_id, &r, p, kernel)
                    }
                    Call::UnwatchPath(a) => {
                        let p = a.path.as_str();
                        let r = match clamped(&root) {
                            Ok(r) => r.to_path_buf(),
                            Err(e) => return Ok((Some(e), None)),
                        };
                        self.watch.unwatch_path(agent_id, &r, p, kernel)
                    }
                    // `watch` and `reveal` are a VIEW's verbs (answered above, by the
                    // view); a bridge declares none, so its entry refused them already.
                    _ => unreachable!("every other verb is answered above"),
                }
            }
        };
        Ok((Some(reply), None))
    }

    /// The bridge is going away — its undo slots and its disk watch go with it. A
    /// VIEW going away ends its own relay and lets go of its bridge's watch.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let is_view = kernel
            .agents
            .get(agent_id)
            .is_some_and(|a| a.record().meta.get("view").is_some());
        if is_view {
            views::stop(kernel, &self.views, agent_id).await;
            return Ok(());
        }
        self.text.drop_slots(&agent_id.0);
        self.watch.drop_bridge(agent_id);
        Ok(())
    }
}

/// python `tools._view_scope`: a VIEW caller reaches only inside its own path, whatever
/// its grant names. None admits; a refusal is returned. An operator or a plain agent is
/// not path-scoped here.
fn view_scope(
    kernel: &Arc<Kernel>,
    agent_id: &AgentId,
    origin: Option<&AgentId>,
    verb: &str,
    rel: &str,
) -> Option<Value> {
    let (kind, own) = views::caller_scope(kernel, &agent_id.0, origin)?;
    let inside = norm(rel).is_some_and(|p| !own.is_empty() && views::inside(&kind, &own, &p));
    if inside {
        return None;
    }
    let mut r = rf::refusal(
        rf::UNAUTHORIZED,
        format!(
            "{verb}: {} is outside this {kind} view's path {}",
            py_repr(rel),
            py_repr(&own)
        ),
    );
    rf::with(
        &mut r,
        "hint",
        json!("a view reaches only its own path; open a view of that path (open_view)"),
    );
    rf::with(&mut r, "path", json!(rel));
    Some(r)
}

/// The sealed-by-default GATE — the fs edge is an io_bridge leg. Mirrors py
/// file_bridge's `gate_inbound` choke point. Returns `Some(error)` on deny,
/// `None` to admit. Shared by the text (`handle`) and binary (`handle_binary`)
/// channels so a sealed bridge refuses streamed bytes exactly like a text call.
fn gate(meta: &Map<String, Value>, agent_id: &AgentId, verb: &str) -> Option<Value> {
    let rules = fantastic_io_bridge::authorizer::Rules::of(meta).unwrap_or_default();
    let rule = fantastic_io_bridge::authorizer::ingress::resolve(&rules);
    // NO token: the fs edge has no wire envelope, so nothing can present one.
    // The gate never reads the record for a credential: a persisted secret in
    // the portable workdir satisfying a `password_ingress` leg is the one thing
    // the password rule exists to prevent. `password_ingress` is not a name of
    // this edge's record.
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
pub(crate) fn clamped(root: &Result<PathBuf, String>) -> Result<&Path, Value> {
    match root {
        Ok(p) => Ok(p.as_path()),
        Err(e) => Err(rf::refusal(rf::TRANSPORT_ERROR, e.clone())),
    }
}

/// The record's `readonly` flag (declared a boolean; absent, off).
pub(crate) fn readonly_flag(meta: &Map<String, Value>) -> bool {
    meta.get("readonly").and_then(Value::as_bool).unwrap_or(false)
}

/// python's `_root_str`: the configured root string (`rec.get("root", "") or ""`).
fn root_str(meta: &Map<String, Value>) -> String {
    meta.get("root")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The clamped root: the configured root under the base the kernel's one rule
/// names ([`Kernel::record_root`]: `@state` → the state folder, `@self` → the
/// bridge's own folder `own`, else the work folder; no folder ⇒ the refusal
/// text).
fn clamp_root(meta: &Map<String, Value>, kernel: &Kernel, own: &Path) -> Result<PathBuf, String> {
    let (base, rel) = kernel.record_root(&root_str(meta), own)?;
    fs::resolve_root(&rel, &base)
}

/// The SOURCE half — read ONE raw chunk at `offset`; the chunk is the reply
/// BODY, at the reply's `bytes` key. Stateless cursor (no open handle): pull
/// the next chunk by calling again with `offset=next_offset`. Mirrors py
/// `_read_stream`.
///
/// THE `bytes` KEY STILL EXISTS, holding `null`, IN ITS PYTHON POSITION between
/// `offset` and `next_offset` — python's reply dict always has it and the codec
/// nulls it in place when it lifts the value onto the wire, so a binary frame's
/// head keeps python's dict shape.
fn read_stream_reply(
    agent_id: &AgentId,
    root: &Result<PathBuf, String>,
    a: &verbs::ReadStream,
) -> (Value, Option<Vec<u8>>) {
    let path_str = a.path.as_str();
    let offset = a.offset;
    let length = a.length;
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return (e, None),
    };
    match fs::is_file(root, path_str) {
        Ok(true) => {}
        Ok(false) => {
            let mut v = rf::refusal(
                rf::NOT_FOUND,
                format!(
                    "file {} not found{}",
                    py_repr(path_str),
                    crate::fs::not_found_hint(root, path_str)
                ),
            );
            rf::with(&mut v, "path", json!(path_str));
            return (v, None);
        }
        Err(e) => return (rf::refusal(rf::TRANSPORT_ERROR, e), None),
    }
    let (chunk, size) = match fs::read_chunk(root, path_str, offset, length) {
        Ok(c) => c,
        Err(e) => return (rf::refusal(rf::TRANSPORT_ERROR, e), None),
    };
    let next_offset = offset + chunk.len() as u64;
    let mut out = Map::new();
    out.insert("path".to_string(), json!(path_str));
    out.insert("offset".to_string(), json!(offset));
    // the slot python's dict holds the chunk in; the chunk is the reply body
    out.insert("bytes".to_string(), Value::Null);
    out.insert("next_offset".to_string(), json!(next_offset));
    out.insert("eof".to_string(), json!(next_offset >= size));
    out.insert("size".to_string(), json!(size));
    out.insert(
        "origin".to_string(),
        json!(format!(
            "{agent_id} {path_str} bytes {offset}-{next_offset} of {size} — read_stream \
             {{path: {path_str}, offset: {offset}, length: {}}}",
            chunk.len()
        )),
    );
    // A CHUNK CAME BACK — `Some`, even when it is empty (an empty file, or a
    // read past the end). Only that tells a leg it may answer octet.
    (Value::Object(out), Some(chunk))
}

/// A path a MUTATING verb names must be INSIDE the root: one that resolves to the
/// root itself (`.`) is `bad_arg`, so `delete {path:".", recursive:true}` can never
/// remove the whole served store — the root is the boundary, not an operand. The
/// shape of the argument (non-empty, root-relative) is the declaration's.
fn target_arg<'a>(
    verb: &str,
    root: &Result<PathBuf, String>,
    name: &str,
    p: &'a str,
) -> Result<&'a str, Value> {
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
    a: &verbs::WriteStream,
    blob: Option<&[u8]>,
    readonly: bool,
) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let path_str = a.path.as_str();
    let Some(blob) = blob else {
        return rf::missing("write_stream", "bytes (one raw chunk)");
    };
    // `offset` absent ⇒ append at the end.
    let off_in = a.offset;
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return e,
    };
    match fs::write_chunk(root, path_str, blob, off_in, a.truncate) {
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
/// BOTH ENDS RUN AS THIS BRIDGE: `read_stream` on the source and `write_stream`
/// on the sink are sent from this bridge's own dispatch, so they are its sends —
/// the bridge's `can_call` and each end's `callable_by` decide (its own `$self`
/// baseline covers a pump within itself). Each end SELF-gates + SELF-clamps — the pump only coordinates, never touching bytes, so
/// a sealed end refuses it. A pump ONTO ITS OWN SOURCE (the sink is the source and
/// `sink_path` names the same file) is refused. Mirrors py `_pump`.
async fn pump_reply(agent_id: &AgentId, a: &verbs::Pump, kernel: &Arc<Kernel>) -> Value {
    // an end the call leaves out is this bridge; the sink file is the source's by default
    let src = a.source.clone().unwrap_or_else(|| agent_id.0.to_string());
    let sink = a.sink.clone().unwrap_or_else(|| agent_id.0.to_string());
    let spath = a.source_path.clone();
    let dpath = a.sink_path.clone().unwrap_or_else(|| spath.clone());
    let chunk = a.chunk;
    if src == sink && one_file(&spath, &dpath) {
        // The first write's `truncate` empties the file the next read comes from:
        // everything past the first chunk would be lost under a success reply —
        // the bare `{source_path}` form, sink and sink_path defaulted (python
        // `_pump`).
        return rf::bad(
            "pump",
            "sink_path",
            "another file than source_path when the sink is the source — a pump \
             onto its own source truncates it after the first chunk",
        );
    }
    let src_id = AgentId::from(src.as_str());
    let sink_id = AgentId::from(sink.as_str());
    let (mut offset, mut first, mut chunks) = (0u64, true, 0u64);
    loop {
        // TWO ORDINARY SENDS, python's: each end is a batch item (pid, rows,
        // `ps` row), the read taking its chunk back raw, the write carrying it at
        // `bytes` in python's key order. Sent from this dispatch, so both are THIS
        // BRIDGE's sends, gated as the bridge.
        let read = kernel
            .send(vec![Item::new(
                src_id.clone(),
                fantastic_kernel::verbs::call("read_stream", json!({"path": spath, "offset": offset, "length": chunk})),
            )])
            .await
            .remove(0);
        let (rmeta, body) = (read.result, read.body);
        // python tests the reply for a `bytes` chunk (`isinstance(r["bytes"],
        // bytes)`); on this channel the chunk rides the body, so the carrier
        // answers the same question — `None`, or a chunk at another slot, is
        // "no chunk came back".
        let Some(body) = body
            .filter(|_| rmeta.is_object())
            .and_then(|b| b.into_at("bytes"))
        else {
            return rf::refusal(
                rf::TRANSPORT_ERROR,
                format!(
                    "pump: read from {} failed: {}",
                    py_repr(&src),
                    rmeta
                ),
            );
        };
        let wmeta = kernel
            .send(vec![Item::new(
                sink_id.clone(),
                fantastic_kernel::verbs::call("write_stream", json!({"path": dpath, "offset": offset, "truncate": first})),
            )
            .with_binary_key("bytes", body)])
            .await
            .remove(0)
            .result;
        if !wmeta.is_object() || wmeta.get("error").is_some_and(|e| !e.is_null()) {
            return rf::refusal(
                rf::TRANSPORT_ERROR,
                format!(
                    "pump: write to {} failed: {}",
                    py_repr(&sink),
                    wmeta
                ),
            );
        }
        first = false;
        chunks += 1;
        let next = rmeta.get("next_offset").and_then(Value::as_u64);
        if rmeta.get("eof").and_then(Value::as_bool) == Some(true) {
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
                        py_repr(&src)
                    ),
                )
            }
        }
    }
    json!({"source": spath, "sink": dpath, "bytes": offset, "chunks": chunks})
}

/// Do two pump paths name ONE file — equal once normalised (`./x` is `x`)?
/// python `_one_file`: only a root-relative path to something inside the root is
/// judged; anything else (absent, absolute, climbing out, the root itself) is its
/// end's to refuse.
fn one_file(a: &str, b: &str) -> bool {
    norm(a).is_some_and(|n| n != "." && Some(&n) == norm(b).as_ref())
}

// ─── the allow list (what this bridge exposes) ──────────────────
//
// DENY-ALL AT THE PATH LEVEL — python `file_bridge` is canonical. The record's
// `allow` list names what the bridge EXPOSES, relative to `root`: a file, or
// everything under a directory (`.` = the whole root). No list exposes NOTHING.
// Every path verb sees only exposed paths; anything else is `not_found` whether
// or not it exists, and `list` also shows the directories on the way to one.

/// python `posixpath.normpath(path or ".")` of a ROOT-RELATIVE path; `None` when
/// it is absolute ([`is_absolute`]) or climbs out.
pub(crate) fn norm(path: &str) -> Option<String> {
    if is_absolute(path) {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    })
}

/// The `allow` list, normalised. Absent or empty ⇒ nothing is exposed.
pub(crate) fn allow_list(meta: &Map<String, Value>) -> Vec<String> {
    meta.get("allow")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|x| x.as_str().and_then(norm)).collect())
        .unwrap_or_default()
}

/// Is `path` exposed — a listed path, or under a listed directory?
pub(crate) fn exposed(allow: &[String], path: &str) -> bool {
    let Some(p) = norm(path) else { return false };
    allow
        .iter()
        .any(|s| s == "." || *s == p || p.starts_with(&format!("{s}/")))
}

/// Is `path` a directory on the way to an exposed path? (`list` shows it.)
fn leads_to(allow: &[String], path: &str) -> bool {
    let Some(p) = norm(path) else { return false };
    !allow.is_empty() && (p == "." || allow.iter().any(|s| s.starts_with(&format!("{p}/"))))
}

/// Where `path` actually LANDS (symlinks followed) must be under the RESOLVED
/// form of an allow entry — entries resolved now, because the disk changes. With
/// [`exposed`] (the path as asked) this is the both-ends rule: a link inside an
/// exposed area that points outside it serves nothing.
fn lands_exposed(root: &Path, allow: &[String], path: &str) -> bool {
    let Ok(target) = fs::resolve(root, path) else {
        return false;
    };
    allow.iter().any(|s| {
        // an entry that itself leads out of the root exposes nothing
        fs::resolve(root, s).is_ok_and(|entry| target.starts_with(&entry))
    })
}

/// The ONE answer for a path outside the list — the same whether it exists or not.
fn unexposed(verb: &str, path: &str) -> Value {
    let mut v = rf::refusal_hint(
        rf::NOT_FOUND,
        format!("{verb}: {} is not exposed by this bridge", py_repr(path)),
        "expose it first — `allow {path}` (whoever holds that grant)",
    );
    rf::with(&mut v, "path", json!(path));
    v
}

/// THE CLAMP FIRST, then the list, on BOTH ends — python `_gate_path`.
pub(crate) fn gate_path(
    verb: &str,
    root: &Result<PathBuf, String>,
    allow: &[String],
    path: &str,
) -> Option<Value> {
    let r = match clamped(root) {
        Ok(r) => r,
        Err(e) => return Some(e),
    };
    if let Err(e) = fs::resolve(r, path) {
        return Some(rf::refusal(rf::TRANSPORT_ERROR, e));
    }
    // BOTH ends: the path as asked AND the file it lands on.
    (!(exposed(allow, path) && lands_exposed(r, allow, path))).then(|| unexposed(verb, path))
}

/// The exposure check for one path verb, placed where python places it: after
/// that verb's own argument checks (an argument fault answers first — `None`
/// here lets the verb report it), before any disk access.
fn exposure(
    call: &verbs::Call,
    root: &Result<PathBuf, String>,
    meta: &Map<String, Value>,
    readonly: bool,
) -> Option<Value> {
    use verbs::Call;
    let allow = allow_list(meta);
    match call {
        Call::List(a) => {
            let p = a.path.as_deref().unwrap_or_default();
            if leads_to(&allow, p) {
                return None;
            }
            gate_path("list", root, &allow, p)
        }
        Call::ReadStream(a) => {
            let p = a.path.as_str();
            gate_path("read_stream", root, &allow, p)
        }
        Call::WatchPath(a) => {
            let p = a.path.as_str();
            gate_path("watch_path", root, &allow, p)
        }
        Call::WriteStream(a) => {
            if readonly {
                return None;
            }
            let p = a.path.as_str();
            gate_path("write_stream", root, &allow, p)
        }
        Call::Delete(a) => {
            if readonly {
                return None;
            }
            let p = target_arg("delete", root, "path", &a.path).ok()?;
            gate_path("delete", root, &allow, p)
        }
        Call::Mkdir(a) => {
            if readonly {
                return None;
            }
            let p = target_arg("mkdir", root, "path", &a.path).ok()?;
            gate_path("mkdir", root, &allow, p)
        }
        Call::Rename(a) => {
            if readonly {
                return None;
            }
            let old = target_arg("rename", root, "old_path", &a.old_path).ok()?;
            let new = target_arg("rename", root, "new_path", &a.new_path).ok()?;
            gate_path("rename", root, &allow, old)
                .or_else(|| gate_path("rename", root, &allow, new))
        }
        _ => None,
    }
}

/// `list` shows only exposed entries (both ends) and the directories on the way
/// to one.
fn keep_exposed(out: &mut Value, allow: &[String], root: &Result<PathBuf, String>) {
    let Ok(r) = clamped(root) else { return };
    if let Some(files) = out.get_mut("files").and_then(Value::as_array_mut) {
        files.retain(|f| {
            let p = f.get("path").and_then(Value::as_str).unwrap_or("");
            (exposed(allow, p) && lands_exposed(r, allow, p))
                || (f.get("type") == Some(&json!("dir")) && leads_to(allow, p))
        });
    }
}

/// How an edit is written — python `_reflect`'s `edit_grammar`, verbatim.
const EDIT_GRAMMAR: &str = "one or more blocks: `<<<<<<< SEARCH` / <old lines> / `=======` / \
     <new lines> / `>>>>>>> REPLACE`; each SEARCH must match its document EXACTLY ONCE. \
     `edit` is a TRANSACTION over `edits:[{path, hunks, if_match?}]` — even one file rides \
     the array; the blob holds every edit's hunks concatenated in `edits` order, split by \
     the per-edit `hunks` count — with ONE file, `hunks` may be left out and every block \
     applies to it; with several files, a block may name its file on its first line \
     (`<<<<<<< SEARCH path`), and then no hunks are needed; any failure anywhere writes \
     nothing. Text that holds conflict markers takes the 8-character fence instead: \
     `<<<<<<<< SEARCH` / `========` / `>>>>>>>> REPLACE` (N `<`, `=`, `>` with N ≥ 8).";

/// How a document rides a call — python `_reflect`'s `compose`, verbatim.
const COMPOSE: &str = "every content-carrying verb (`write`, `edit`, `append`) takes its \
     document in `bytes`, and `bytes` is VERBATIM: write the document between `<bytes>` and \
     `</bytes>` exactly as it should land — real newlines, real quotes, nothing escaped, no \
     fence — then close the call. The document ends at the first `</bytes>`. An edit names \
     its files as tags: <edits><item><path>a.py</path><hunks>2</hunks></item></edits>";

/// python's `_reflect`: identity + the resolved root + the readonly flag + the
/// auth posture, in python's key order. It NEVER refuses — a root that escapes
/// the running dir is shown as configured, with the violation in `root_error`.
fn reflect_reply(
    agent_id: &AgentId,
    meta: &Map<String, Value>,
    clamped: &Result<PathBuf, String>,
) -> Value {
    let raw_root = root_str(meta);
    let (root_display, root_error) = match clamped.clone() {
        Ok(p) => (p.to_string_lossy().into_owned(), None),
        Err(e) => (raw_root, Some(e)),
    };
    let mut out = Map::new();
    out.insert("id".to_string(), json!(agent_id.as_str()));
    out.insert(
        "sentence".to_string(),
        json!(MAN),
    );
    out.insert("root".to_string(), json!(root_display));
    out.insert(
        "readonly".to_string(),
        json!(readonly_flag(meta)),
    );
    out.insert("allow_count".to_string(), json!(allow_list(meta).len()));
    out.insert("edit_grammar".to_string(), json!(EDIT_GRAMMAR));
    out.insert("compose".to_string(), json!(COMPOSE));
    // The auth POSTURE, python's `describe(rec)`: rule NAMES (never config),
    // keyed by the field a caller WRITES (read-key == write-key), + `sealed`.
    if let Some(posture) = fantastic_io_bridge::authorizer::Rules::of(meta)
        .unwrap_or_default()
        .describe()
        .as_object() {
        for (k, v) in posture {
            out.insert(k.clone(), v.clone());
        }
    }
    if let Some(e) = root_error {
        out.insert("root_error".to_string(), json!(e));
    }
    Value::Object(out)
}

fn list_reply(agent_id: &AgentId, root: &Result<PathBuf, String>, a: &verbs::List) -> Value {
    let path_str = a.path.as_deref().unwrap_or_default();
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
        // The entry's path AS A CALLER NAMES IT — under the listed dir, links not
        // followed (python `posixpath.join(_norm(path) or ".", entry.name)`), so an
        // exposed symlinked dir lists what it serves.
        let rel = match norm(path_str).as_deref() {
            None | Some(".") => name.clone(),
            Some(base) => format!("{base}/{name}"),
        };
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
    json!({
        "path": path_str,
        "files": files,
        "origin": format!(
            "{agent_id} list {path_str} — list {{path: {path_str}}}; changes as files change"
        ),
    })
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
/// `rmdir`.
fn delete_reply(root: &Result<PathBuf, String>, a: &verbs::Delete, readonly: bool) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let path_str = match target_arg("delete", root, "path", &a.path) {
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
    match fs::remove(root, path_str, a.recursive) {
        Ok(()) => json!({"path": path_str, "deleted": true}),
        Err(e) => rf::refusal(rf::TRANSPORT_ERROR, e),
    }
}

fn rename_reply(root: &Result<PathBuf, String>, a: &verbs::Rename, readonly: bool) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let old = match target_arg("rename", root, "old_path", &a.old_path) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let new = match target_arg("rename", root, "new_path", &a.new_path) {
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

fn mkdir_reply(root: &Result<PathBuf, String>, a: &verbs::Mkdir, readonly: bool) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let path_str = match target_arg("mkdir", root, "path", &a.path) {
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

/// The typed arguments of a verb, from the JSON a test writes.
#[cfg(test)]
fn args<T: serde::de::DeserializeOwned>(v: Value) -> T {
    serde_json::from_value(v).expect("typed arguments")
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_path_gate;
#[cfg(test)]
mod tests_paths;
#[cfg(test)]
mod tests_pump_identity;
#[cfg(test)]
mod tests_text;
#[cfg(test)]
mod tests_views;
#[cfg(test)]
mod tests_watch;
#[cfg(test)]
mod tests_watch_missing;
