//! file_bridge — the fs-transport member of the io family.
//!
//! The disk is OUTSIDE the trust domain: crossing kernel↔disk is an IO edge,
//! gated exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule` ⇒
//! every verb except `reflect` denies with the teaching shape). Open it
//! consciously with `ingress_rule=allow_all_ingress`. Even open, the LAW is
//! running-dir only: the `root` AND every path are clamped inside the kernel's
//! running dir ([`Kernel::workdir`] — the dir holding its root's `.fantastic`;
//! `../`, absolute escapes, outward symlinks all refuse).
//!
//! Each file_bridge owns a `root` directory and a `readonly` flag. The disk
//! clamp + raw IO live in [`fs`] — the ONE disk surface; this bundle is the GATE
//! (ingress rule) + the verb shape over it, and never touches the disk directly.
//!
//! ## Verbs
//!
//! | verb     | args                       | reply                                 |
//! |----------|----------------------------|---------------------------------------|
//! | `reflect`| _none_                     | `{id, sentence, root, readonly, allow_count, edit_grammar, compose, verbs, ingress_rule, egress_rule, sealed}` |
//! | `boot`   | _none_                     | `null`                                |
//! | `list`   | `path?`                    | `{path, files: [{name, path, type, size?}]}` |
//! | `read`   | `path`, `offset?`, `limit?` | `{path, lines:[{n,text}], total, eof, version, origin}` |
//! | `write`  | `path`, `bytes`, `if_match?` | `{path, size, version}` (atomic)     |
//! | `edit`   | `edits`, `bytes`, `dry_run?` | `{edited, edits:[{path,hunks,size,version}]}` |
//! | `append` | `path`, `bytes`            | `{path, size, version}`               |
//! | `undo`   | `path`                     | `{undone, path, version|deleted}`     |
//! | `read_stream` | `path`, `offset?`, `length?` | `{path, offset, bytes, next_offset, eof, size}` |
//! | `write_stream`| `path`, `bytes`, `offset?`, `truncate?` | `{path, written, offset, size}` |
//! | `pump`   | `source?`, `source_path`, `sink?`, `sink_path?` | server-side SOURCE→SINK copy |
//! | `delete` | `path`, `recursive?`       | `{path, deleted: true}`               |
//! | `rename` | `old_path`, `new_path`     | `{old_path, new_path}`                |
//! | `mkdir`  | `path`                     | `{path, created: true}`               |
//!
//! The `allow` list is a RECORD field: set at create, changed by `update_agent`.
//!
//! The text verbs are this bridge's own (see [`text`]): a caller edits with ITS
//! OWN grants. Raw bytes move over the stream verbs, never base64.
//! `readonly: true` on the record refuses every mutating verb with the one
//! refusal shape, `reason: "unauthorized"`.

#![deny(missing_docs)]

pub mod fs;
mod sys;
#[doc(hidden)]
pub mod test_support;
mod text;
mod views;
mod watch;

use async_trait::async_trait;
use fantastic_bundle::{is_absolute, py_repr, py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal as rf;
use fantastic_kernel::{body_at, current_origin, AgentId, Blob, Item, Kernel};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "file_bridge.tools";

/// readme.md auto-seeded into the agent's dir on creation.
pub const README: &str = include_str!("readme.md");

/// The verb table, in python's `VERBS` order — reflect's `verbs` map, the
/// unknown-verb refusal and the gate all read it.
const VERBS: [(&str, &str); 17] = [
    (
        "reflect",
        "Identity + filesystem root + readonly flag + auth posture (ingress/egress/sealed/see). No args.",
    ),
    ("boot", "No-op. Returns None."),
    (
        "list",
        "args: path:str (default ''). Returns {path, files:[{name, path, type, size?}, ...]} — the exposed entries and the directories on the way to one.",
    ),
    (
        "read",
        "args: path:str (req), offset:int=0 (line), limit:int=200 — a text page {path, lines:[{n,text}], total, eof, version, origin}; refuses binary.",
    ),
    (
        "write",
        "args: path:str (req), bytes (req — the whole document; str is utf-8-encoded), if_match:str? — atomic whole-document write; {path, size, version}.",
    ),
    (
        "edit",
        "args: edits:[{path:str, hunks:int, if_match:str?}] (req — even for ONE file), bytes (req — ALL hunks concatenated; str is utf-8-encoded), dry_run:bool? — a TRANSACTION: every hunk of every edit verified before any write, any failure writes nothing (or restores what was written); {edited:true, edits:[{path,hunks,size,version}]}.",
    ),
    (
        "append",
        "args: path:str (req), bytes (req — one chunk; str is utf-8-encoded) — appends (creates if absent); {path, size, version}.",
    ),
    (
        "undo",
        "args: path:str (req) — swap the document with the bytes the last write/edit/append replaced (a second undo is redo); ONE slot per path, in RAM, lost on restart; {undone:true, path, version|deleted}.",
    ),
    (
        "read_stream",
        "args: path:str (req), offset:int=0, length:int=65536. The SOURCE half — returns ONE chunk {path, offset, bytes, next_offset, eof, size}.",
    ),
    (
        "write_stream",
        "args: path:str (req), bytes:bytes (req — ONE raw chunk; a model writes it VERBATIM between `<bytes>` and `</bytes>`, nothing escaped), offset:int? (default = append at end), truncate:bool=False. The SINK half — a producer pushes RAW chunks by offset.",
    ),
    (
        "pump",
        "The PUMP — a server-side stream copy SOURCE→SINK, chunk by chunk, in ONE call.",
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
    (
        "watch_path",
        "args: path:str (req — an exposed file or directory). Emit `{type: changed, path, kind}` on this bridge's inbox whenever it changes on disk, by any writer; WATCH this bridge to receive them. Returns {watching, paths}.\n\n`kind` is `added`, `modified` or `deleted` as the disk reported it — treat any\n`changed` as \"read it again\". A directory reports its entries (not recursive).\nHeld per caller until `unwatch_path`; RAM only.",
    ),
    (
        "unwatch_path",
        "args: path:str (req). Stop YOUR watch of a path; the disk watch ends when no caller holds a path. Returns {watching:false, paths}.",
    ),
    (
        "open_view",
        "args: kind:str (req — `file` or `folder`), path:str (req — root-relative, exposed by `allow`; `.` = the whole root, folders only). Reuses the view of that path or makes it: {id, kind, path, created}. The new view's grant on THIS bridge is its own verbs only (views.GRANTS).",
    ),
];

/// Discovery must work on a sealed bridge — python's `UNGATED`.
const UNGATED: [&str; 1] = ["reflect"];

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
        if let Some(bad) = views::view_refusal(meta) {
            return Some(bad);
        }
        if let Some(a) = meta.get("allow") {
            let ok = a.as_array().is_some_and(|items| {
                items.iter().all(|x| {
                    x.as_str()
                        .is_some_and(|s| !s.is_empty() && !is_absolute(s) && norm(s).is_some())
                })
            });
            if !ok {
                return Some(
                    "allow must be a list of paths relative to the root (a file, or a \
                     directory exposing everything under it; `.` = the whole root)"
                        .to_string(),
                );
            }
        }
        fantastic_io_bridge::authorizer::ingress::unknown_refusal(
            meta,
            &["allow_all_ingress", "deny_ingress"],
        )
        .or_else(|| password_refusal(meta, "fs edge"))
    }

    fn name(&self) -> &str {
        "file_bridge"
    }

    fn verb_docs(&self) -> Value {
        let mut verbs = Map::new();
        for (name, doc) in VERBS {
            verbs.insert(name.to_string(), json!(doc));
        }
        Value::Object(verbs)
    }

    /// A VIEW answers its own verbs, so `docs of=<view>` serves its table (python
    /// `instance_verbs`); every other record keeps the bridge's.
    fn instance_verb_docs(&self, agent: &fantastic_kernel::Agent) -> Option<Value> {
        views::verb_docs_of(&agent.record().meta)
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(json!({
            "changed": "{path, kind: added|modified|deleted} — a path under `watch_path` changed on disk, by any writer; watch this bridge to receive it",
        }))
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
        // THE PATH GATE FOR A VIEW CALLER — python `tools._view_scope`, after the gate.
        if SCOPED_VERBS.contains(&verb) {
            if let Some(refused) =
                view_scope(kernel, agent_id, current_origin().as_ref(), verb, payload)
            {
                return Ok(Some(refused));
            }
        }
        let base = kernel.workdir();
        let readonly = py_truthy(meta.get("readonly").unwrap_or(&Value::Null));
        let reply = match verb {
            "reflect" => reflect_reply(agent_id, &meta, &base),
            "boot" => Value::Null,
            "open_view" => views::open_view_verb(kernel, &meta, agent_id, payload).await,
            v if text::TEXT_VERBS.contains(&v) => {
                let root = clamp_root(&meta, &base);
                text::dispatch(
                    &self.text,
                    v,
                    &agent_id.0,
                    &meta,
                    &root,
                    payload,
                    None,
                    readonly,
                )
                .await
            }
            // The PUMP coordinates a SOURCE→SINK copy by id; it never touches
            // bytes/root itself, so it rides without a clamp.
            "pump" => pump_reply(agent_id, payload, kernel).await,
            other => {
                // THE RUNNING-DIR LAW. The clamp is resolved here but applied
                // AFTER each verb's argument checks — python reaches `fs` only
                // once the arguments are good, so a bad argument on a bridge with
                // an escaping root answers `bad_arg`, not `transport_error`.
                let root = clamp_root(&meta, &base);
                // DENY-ALL AT THE PATH LEVEL — after each verb's own argument
                // checks (python's order), before any disk access.
                if let Some(refused) = exposure(other, &root, &meta, payload, readonly) {
                    return Ok(Some(refused));
                }
                match other {
                    "list" => {
                        let mut out = list_reply(agent_id, &root, payload);
                        keep_exposed(&mut out, &allow_list(&meta), &root);
                        out
                    }
                    // THE JSON HALF of a read: the chunk is the reply body
                    // `handle_binary` returns beside it (the kernel's dispatch);
                    // a JSON-only reply holds `bytes: null` in its slot.
                    "read_stream" => read_stream_reply(agent_id, &root, payload).0,
                    // `bytes` IN THE JSON IS NEVER A CHUNK. python types the
                    // argument (`isinstance(data, (bytes, bytearray))`), and
                    // JSON has no bytes, so ANY present value here — a string, a
                    // number — is `missing_arg`, and nothing is written.
                    // A raw chunk reaches this bundle as a request body only;
                    // that is the carrier a model's `<bytes>` document rides too.
                    // Absent ⇒ python's `payload.get("bytes", b"")`: an empty
                    // chunk, which is a legal write.
                    "write_stream" => {
                        let chunk = match payload.get("bytes") {
                            None => Some(&b""[..]),
                            Some(_) => None,
                        };
                        write_stream_reply(&root, payload, chunk, readonly)
                    }
                    "delete" => delete_reply(&root, payload, readonly),
                    "rename" => rename_reply(&root, payload, readonly),
                    "mkdir" => mkdir_reply(&root, payload, readonly),
                    "watch_path" | "unwatch_path" => {
                        let p = match path_arg(other, payload, "path") {
                            Ok(p) => p,
                            Err(e) => return Ok(Some(e)),
                        };
                        let r = match clamped(&root) {
                            Ok(r) => r.to_path_buf(),
                            Err(e) => return Ok(Some(e)),
                        };
                        if other == "watch_path" {
                            self.watch.watch_path(agent_id, &r, p, kernel)
                        } else {
                            self.watch.unwatch_path(agent_id, &r, p, kernel)
                        }
                    }
                    _ => unreachable!("every VERBS entry is dispatched"),
                }
            }
        };
        Ok(Some(reply))
    }

    /// The kernel's dispatch for every verb: `read_stream` (reply BODY = the raw
    /// chunk, whatever the call carried) and `write_stream` (the body at `bytes`
    /// = the raw chunk → status reply, no body). Symmetric with Python's
    /// `bytes`-in-the-dict; never base64. Every verb reads the body at ITS key,
    /// `bytes` — python's `payload.get("bytes")` — so a body placed under
    /// another key is not a chunk. Any other verb takes `handle`.
    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        body: Option<Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Blob>), BundleError> {
        let verb = header.get("type").and_then(Value::as_str).unwrap_or("");
        let chunk = body_at(body.as_ref(), "bytes");
        // A VIEW is answered by its own verbs, before the bridge's table and gate (python
        // `file_bridge.tools.handler`): a view is not sealed, its gate is its grant.
        let view = kernel.agents.get(agent_id).and_then(|a| {
            a.record()
                .meta
                .get("view")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        if let Some(kind) = view {
            let reply = views::dispatch(kernel, &self.views, agent_id, &kind, verb, &header).await;
            return Ok((Some(reply), None));
        }
        if text::TEXT_VERBS.contains(&verb) {
            // THE BODY AT `bytes` IS THE `bytes` ARGUMENT, present even when
            // empty — the bytes-typed carrier a model's `<bytes>` document rides.
            // A text verb answers a document, never a raw chunk: no reply body.
            let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
                return Ok((Some(json!({"error": format!("no agent {agent_id}")})), None));
            };
            let meta = agent.meta.read().expect("meta poisoned").clone();
            if let Some(denied) = gate(&meta, agent_id, verb) {
                return Ok((Some(denied), None));
            }
            // THE PATH GATE FOR A VIEW CALLER, as `handle` runs it after the gate: the
            // binary channel is no way round it (python's single `handler` gates every entry).
            if SCOPED_VERBS.contains(&verb) {
                if let Some(refused) =
                    view_scope(kernel, agent_id, current_origin().as_ref(), verb, &header)
                {
                    return Ok((Some(refused), None));
                }
            }
            let readonly = py_truthy(meta.get("readonly").unwrap_or(&Value::Null));
            let root = clamp_root(&meta, &kernel.workdir());
            let reply = text::dispatch(
                &self.text,
                verb,
                &agent_id.0,
                &meta,
                &root,
                &header,
                chunk,
                readonly,
            )
            .await;
            return Ok((Some(reply), None));
        }
        if !matches!(verb, "read_stream" | "write_stream") {
            // file_bridge has no other verb that takes or answers bytes — route
            // the header through `handle` (which gates + dispatches); a request
            // body is unused.
            let reply = self.handle(agent_id, &header, kernel).await?;
            return Ok((reply, None));
        }
        let agent = match kernel.agents.get(agent_id) {
            Some(e) => Arc::clone(&e),
            None => return Ok((Some(json!({"error": format!("no agent {agent_id}")})), None)),
        };
        let meta = agent.meta.read().expect("meta poisoned").clone();
        // GATE — same sealed-by-default choke point as `handle`.
        if let Some(denied) = gate(&meta, agent_id, verb) {
            return Ok((Some(denied), None));
        }
        let root = clamp_root(&meta, &kernel.workdir());
        let readonly = py_truthy(meta.get("readonly").unwrap_or(&Value::Null));
        if let Some(refused) = exposure(verb, &root, &meta, &header, readonly) {
            return Ok((Some(refused), None));
        }
        match verb {
            "read_stream" => {
                // the chunk is the reply's `bytes`, as python's dict holds it
                let (reply, body) = read_stream_reply(agent_id, &root, &header);
                Ok((Some(reply), body.map(|b| Blob::key("bytes", b))))
            }
            "write_stream" => {
                // THE REQUEST BODY AT `bytes` IS THE CHUNK — python's
                // `payload.get("bytes", b"")`. A header that carries a `bytes`
                // key is a JSON value where raw bytes belong —
                // `missing("write_stream", "bytes (one raw chunk)")`, whatever it
                // holds. Nothing at `bytes` (no body, or a body under another
                // key) is python's default `b""`: `written: 0`.
                //
                // PYTHON'S ORDER: readonly FIRST (`_readonly_or_none` precedes the
                // chunk check), so a readonly leg answers `readonly` whatever the
                // header holds — `handle` already does this.
                if readonly {
                    return Ok((Some(readonly_refusal()), None));
                }
                if header.get("bytes").is_some() {
                    return Ok((
                        Some(rf::missing("write_stream", "bytes (one raw chunk)")),
                        None,
                    ));
                }
                // NO CHUNK COMES BACK from a write — `None`, not an empty body,
                // so a leg can tell this reply from a 0-byte read.
                Ok((
                    Some(write_stream_reply(
                        &root,
                        &header,
                        Some(chunk.unwrap_or_default()),
                        readonly,
                    )),
                    None,
                ))
            }
            _ => unreachable!("only the two stream verbs reach here"),
        }
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

/// The verbs a view may send its bridge that name one path of the disk (python
/// `tools.SCOPED_VERBS`).
const SCOPED_VERBS: [&str; 5] = ["read", "write", "list", "watch_path", "unwatch_path"];

/// python `tools._view_scope`: a VIEW caller reaches only inside its own path, whatever
/// its grant names. None admits; a refusal is returned. An operator or a plain agent is
/// not path-scoped here.
fn view_scope(
    kernel: &Arc<Kernel>,
    agent_id: &AgentId,
    origin: Option<&AgentId>,
    verb: &str,
    payload: &Value,
) -> Option<Value> {
    let (kind, own) = views::caller_scope(kernel, &agent_id.0, origin)?;
    let rel = match payload.get("path") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Some(rf::bad(verb, "path", "a string")),
    };
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
    let spec = fantastic_io_bridge::authorizer::manifold_spec(
        meta.get("ingress_rule"),
        meta.get("auth"),
        "_ingress",
    );
    let rule = fantastic_io_bridge::authorizer::ingress::resolve(spec.as_ref());
    // NO token: the fs edge has no wire envelope, so nothing can present one.
    // The gate never reads the record for a credential: a persisted secret in
    // the portable workdir satisfying a `password_ingress` leg is the one thing
    // the password rule exists to prevent. `password_ingress` is refused at
    // `validate_meta`.
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

/// python's `_root_str`: the configured root string (`rec.get("root", "") or ""`).
fn root_str(meta: &Map<String, Value>) -> String {
    match meta.get("root") {
        Some(v) if py_truthy(v) => py_str(v),
        _ => String::new(),
    }
}

/// The clamped root: the configured root under the kernel's running dir
/// ([`Kernel::workdir`] — the ONE source; no root ⇒ the refusal text).
fn clamp_root(
    meta: &Map<String, Value>,
    base: &Result<PathBuf, String>,
) -> Result<PathBuf, String> {
    base.clone()
        .and_then(|b| fs::resolve_root(&root_str(meta), &b))
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
    payload: &Value,
) -> (Value, Option<Vec<u8>>) {
    let path_str = match path_arg("read_stream", payload, "path") {
        Ok(p) => p,
        Err(e) => return (e, None),
    };
    let offset = match int_arg("read_stream", payload, "offset", 0, 0) {
        Ok(n) => n,
        Err(e) => return (e, None),
    };
    let length = match int_arg("read_stream", payload, "length", 65536, 1) {
        Ok(n) => n,
        Err(e) => return (e, None),
    };
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return (e, None),
    };
    match fs::is_file(root, path_str) {
        Ok(true) => {}
        Ok(false) => {
            let mut v = rf::refusal(
                rf::NOT_FOUND,
                format!("file {} not found", py_repr(path_str)),
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

/// python's `missing("write_stream", "bytes (one raw chunk)")`.
fn missing_bytes() -> Value {
    rf::missing("write_stream", "bytes (one raw chunk)")
}

/// A string argument that is a ROOT-RELATIVE path: absent/null ⇒ `""`; a
/// non-string or an absolute path is `bad_arg` (the absolute form is refused,
/// never silently re-rooted).
pub(crate) fn path_arg<'a>(verb: &str, payload: &'a Value, name: &str) -> Result<&'a str, Value> {
    let p = match payload.get(name) {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return Err(rf::bad(verb, name, "a string")),
    };
    if is_absolute(p) {
        return Err(rf::bad(verb, name, "a root-relative path (no leading '/')"));
    }
    Ok(p)
}

/// An integer argument with a floor: absent/null ⇒ `default`; anything that is
/// not a JSON integer (a bool, a float, a string) or is below `minimum` is
/// `bad_arg` naming the arg. Mirrors py `_int_arg` — a negative `offset` used
/// to trap swift and a huge `length` abort rust; now every runtime refuses.
pub(crate) fn int_arg(
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
/// `bad_arg`, so `delete {path:"", recursive:true}` can never remove the whole
/// served store: the root is the boundary, not an operand.
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
    blob: Option<&[u8]>,
    readonly: bool,
) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let path_str = match path_arg("write_stream", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    // PYTHON'S ORDER: readonly, path, then the chunk's TYPE, then `offset`. A
    // caller that passed a JSON value where raw bytes belong learns that before
    // its offset is graded.
    let Some(blob) = blob else {
        return missing_bytes();
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
/// BOTH ENDS RUN AS THIS BRIDGE: `read_stream` on the source and `write_stream`
/// on the sink are sent from this bridge's own dispatch, so they are its sends —
/// the bridge's `can_call` and each end's `callable_by` decide (its own `$self`
/// baseline covers a pump within itself). Each end SELF-gates + SELF-clamps — the pump only coordinates, never touching bytes, so
/// a sealed end refuses it. A pump ONTO ITS OWN SOURCE (the sink is the source and
/// `sink_path` names the same file) is refused. Mirrors py `_pump`.
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
    let spath = truthy_or("source_path", json!(""));
    let dpath = truthy_or("sink_path", spath.clone());
    let chunk = match int_arg("pump", payload, "chunk", 65536, 1) {
        Ok(n) => n,
        Err(e) => return e,
    };
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
    let src_id = AgentId::from(target_id(&src));
    let sink_id = AgentId::from(target_id(&sink));
    let (mut offset, mut first, mut chunks) = (0u64, true, 0u64);
    loop {
        // TWO ORDINARY SENDS, python's: each end is a batch item (pid, rows,
        // `ps` row), the read taking its chunk back raw, the write carrying it at
        // `bytes` in python's key order. Sent from this dispatch, so both are THIS
        // BRIDGE's sends, gated as the bridge.
        let read = kernel
            .send(vec![Item::new(
                src_id.clone(),
                json!({"type": "read_stream", "path": spath, "offset": offset, "length": chunk}),
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
                    py_repr_value(&src),
                    py_str(&rmeta)
                ),
            );
        };
        let wmeta = kernel
            .send(vec![Item::new(
                sink_id.clone(),
                json!({"type": "write_stream", "path": dpath, "bytes": null, "offset": offset, "truncate": first}),
            )
            .with_binary_key("bytes", body)])
            .await
            .remove(0)
            .result;
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

/// Do two pump paths name ONE file — equal once normalised (`./x` is `x`)?
/// python `_one_file`: only a root-relative path to something inside the root is
/// judged; anything else (absent, not a string, absolute, climbing out, the root
/// itself) is its end's to refuse.
fn one_file(a: &Value, b: &Value) -> bool {
    match (a.as_str(), b.as_str()) {
        (Some(a), Some(b)) => norm(a).is_some_and(|n| n != "." && Some(&n) == norm(b).as_ref()),
        _ => false,
    }
}

/// The id a pump end addresses: a string as written, anything else as python
/// would print it into the `send` target.
fn target_id(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => py_str(other),
    }
}

/// `password_ingress` is meaningless on an in-process leg: the token is read
/// off a WIRE ENVELOPE, and the fs/process edges have none — no frame ever
/// presents one, so the rule could only ever refuse or, worse, be
/// satisfied by a secret persisted ON THE RECORD. Refused at the write, next
/// to the mistake. Both
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
    verb: &str,
    root: &Result<PathBuf, String>,
    meta: &Map<String, Value>,
    payload: &Value,
    readonly: bool,
) -> Option<Value> {
    let allow = allow_list(meta);
    match verb {
        "list" => {
            let p = path_arg("list", payload, "path").ok()?;
            if leads_to(&allow, p) {
                return None;
            }
            gate_path("list", root, &allow, p)
        }
        "read_stream" => {
            let p = path_arg("read_stream", payload, "path").ok()?;
            int_arg("read_stream", payload, "offset", 0, 0).ok()?;
            int_arg("read_stream", payload, "length", 65536, 1).ok()?;
            gate_path("read_stream", root, &allow, p)
        }
        "watch_path" => {
            let p = path_arg("watch_path", payload, "path").ok()?;
            gate_path("watch_path", root, &allow, p)
        }
        "write_stream" => {
            if readonly {
                return None;
            }
            let p = path_arg("write_stream", payload, "path").ok()?;
            gate_path("write_stream", root, &allow, p)
        }
        "delete" | "mkdir" => {
            if readonly {
                return None;
            }
            let p = target_arg(verb, root, payload, "path").ok()?;
            gate_path(verb, root, &allow, p)
        }
        "rename" => {
            if readonly {
                return None;
            }
            let old = target_arg("rename", root, payload, "old_path").ok()?;
            let new = target_arg("rename", root, payload, "new_path").ok()?;
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
     nothing";

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
    base: &Result<PathBuf, String>,
) -> Value {
    let raw_root = root_str(meta);
    let (root_display, root_error) = match clamp_root(meta, base) {
        Ok(p) => (p.to_string_lossy().into_owned(), None),
        Err(e) => (raw_root, Some(e)),
    };
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
    out.insert("allow_count".to_string(), json!(allow_list(meta).len()));
    out.insert("edit_grammar".to_string(), json!(EDIT_GRAMMAR));
    out.insert("compose".to_string(), json!(COMPOSE));
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

fn list_reply(agent_id: &AgentId, root: &Result<PathBuf, String>, payload: &Value) -> Value {
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
#[cfg(test)]
mod tests_pump_identity;
#[cfg(test)]
mod tests_text;
#[cfg(test)]
mod tests_views;
#[cfg(test)]
mod tests_watch;
