//! `text_file` — a stateless text-document agent over the file stream.
//!
//! Format-free by design: the substrate imposes no memory format, so a document
//! is whatever text the caller writes (markdown, JSON, source). This agent
//! supplies the TEXT operations — line-read, atomic write, SEARCH/REPLACE
//! patch, append — plus ON-DEMAND versioning (`version` content hash on every
//! reply; an `if_match` guard that fires only when passed) and ON-DEMAND
//! checkpoints (pre-images as plain files under `.ckpt/` on the same surface;
//! retention is the caller's).
//!
//! NO DISK CODE HERE. Every byte routes through the file_bridge AGENT named by
//! `on` (per call) or the record's `target_id`, over its stream verbs — plain
//! GATED sends, sender = this agent (the dispatch scope), so the operator wires
//! both ends (`can_call`/`callable_by`) and the leg's clamp + ingress rule
//! apply unchanged. Never `with_operator`: these verbs take caller-supplied
//! paths, which is exactly what the confused-deputy rule forbids escalating.
//!
//! Atomicity ownership: this agent owns the TRANSACTION protocol — write goes
//! to `<path>.tmp.<pid>` then `rename` (the leg's one atomic op); a
//! checkpointed mutate commits the pre-image BEFORE touching the target; a
//! patch verifies every hunk before writing anything. Checkpoint ids ARE the
//! dispatch's kernel task pid (an int — one id space with `ps`, the trace and
//! the chat message that caused the mutation). Mirrors the canonical Python
//! `text_file` bundle (python is the immutable reference).

use async_trait::async_trait;
use fantastic_bundle::{py_repr, py_repr_value, py_str, py_truthy};
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::refusal::{bad, missing, refusal, with, NOT_FOUND, UNHANDLED};
use fantastic_kernel::{current_task, Agent, AgentId, Item, Kernel};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

pub const README: &str = include_str!("readme.md");

/// `handler_module` key under which this bundle registers.
pub const HANDLER_MODULE: &str = "text_file.tools";

/// Where pre-images live on the TARGET surface — plain files, visible to `list`.
const CKPT_DIR: &str = ".ckpt";
/// read/write chunk size on the stream verbs.
const CHUNK: usize = 65536;

/// python's `dict.get(key)` default — absent reads as `None`, never as a
/// different shape.
static NULL: Value = Value::Null;

/// `payload.get(key)` with python's `None` default.
fn arg<'a>(payload: &'a Value, key: &str) -> &'a Value {
    payload.get(key).unwrap_or(&NULL)
}

/// ABSENT ⇒ `default`. Present, an integer, ≥ `minimum` ⇒ that value. ANYTHING
/// ELSE (a bool, a string, a float, an explicit null, a value under the bound)
/// ⇒ a `bad_arg` refusal.
///
/// A present-but-wrong number is a TYPO, and `as_u64().unwrap_or(default)` hid
/// it: `read {offset: "12"}` silently paged from the top and told the caller
/// nothing.
fn int_arg(
    payload: &Value,
    key: &str,
    default: u64,
    verb: &str,
    minimum: u64,
) -> Result<u64, Value> {
    let Some(raw) = payload.get(key) else {
        return Ok(default);
    };
    match raw.as_u64() {
        Some(n) if n >= minimum => Ok(n),
        _ => {
            let want = if minimum == 0 {
                "a non-negative int".to_string()
            } else {
                format!("an int >= {minimum}")
            };
            Err(bad(verb, key, &want))
        }
    }
}

pub struct TextFileBundle;

// ─── the target surface ─────────────────────────────────────────

/// The file_bridge id this call operates on: `on` wins, else the record's
/// `target_id`. `None` means unwired — every verb fails loud on it.
fn target_of(agent: &Agent, payload: &Value) -> Option<String> {
    if let Some(on) = payload.get("on").and_then(Value::as_str) {
        if !on.is_empty() {
            return Some(on.to_string());
        }
    }
    agent.meta.read().ok().and_then(|g| {
        g.get("target_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    })
}

fn unwired(verb: &str) -> Value {
    json!({
        "error": format!(
            "{verb}: no target surface — pass on=<file_bridge id> or set \
             `target_id` on this agent's record"
        ),
        "reason": "not_found",
        "hint": "the operator wires both ends: this agent's can_call names the \
                 leg, and the leg's callable_by accepts this agent",
    })
}

/// The on-demand version: a content hash, computed, never stored.
fn version_of(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    hex[..16].to_string()
}

/// This dispatch's kernel task pid (a message id) — the checkpoint id space, an
/// INT like python's. `0` outside any dispatch (an operator call): never
/// minted, so never a clash.
fn pid() -> i64 {
    current_task().map_or(0, |t| t.0)
}

// ─── per-document serialization ─────────────────────────────────
// A mutate is read → verify → commit with awaits between; two concurrent
// mutates on ONE document interleaved there would both derive from the same
// pre-image and the second commit would silently discard the first. One lock
// per (target surface, path), held from the first read through the rename;
// `if_match` is checked UNDER it. Multi-edit patches take theirs in sorted key
// order so two transactions over overlapping sets cannot deadlock. Entries are
// pruned once nobody but the map holds them, so the map never grows.

type LockKey = (String, String);
type LockMap = std::sync::Mutex<HashMap<LockKey, Arc<tokio::sync::Mutex<()>>>>;

static LOCKS: OnceLock<LockMap> = OnceLock::new();

fn locks() -> &'static LockMap {
    LOCKS.get_or_init(Default::default)
}

/// The held document locks — released (and pruned) on drop.
struct Held {
    keys: Vec<LockKey>,
    guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
}

async fn hold(fb: &str, paths: &[&str]) -> Held {
    let mut keys: Vec<LockKey> = paths
        .iter()
        .map(|p| (fb.to_string(), (*p).to_string()))
        .collect();
    keys.sort();
    keys.dedup();
    let arcs: Vec<Arc<tokio::sync::Mutex<()>>> = {
        let mut m = locks().lock().unwrap_or_else(|e| e.into_inner());
        keys.iter()
            .map(|k| Arc::clone(m.entry(k.clone()).or_default()))
            .collect()
    };
    let mut guards = Vec::with_capacity(arcs.len());
    for a in arcs {
        guards.push(a.lock_owned().await);
    }
    Held { keys, guards }
}

impl Drop for Held {
    fn drop(&mut self) {
        self.guards.clear();
        let mut m = locks().lock().unwrap_or_else(|e| e.into_inner());
        for k in &self.keys {
            if m.get(k).is_some_and(|a| Arc::strong_count(a) == 1) {
                m.remove(k);
            }
        }
    }
}

/// `_if_match_guard`'s refusal — `expected` echoes the caller's RAW value (a
/// non-string never matches a hex digest, and saying what was passed is the
/// point of the guard).
fn version_mismatch(want: &Value, have: &str) -> Value {
    json!({
        "error": "version mismatch — the document changed since you read it",
        "reason": "version_mismatch",
        "expected": want,
        "actual": have,
    })
}

/// The per-edit form inside a patch transaction: `path` sits between `reason`
/// and `expected` (python writes the dict in that order).
fn version_mismatch_at(path: &str, want: &Value, have: &str) -> Value {
    json!({
        "error": "version mismatch — the document changed since you read it",
        "reason": "version_mismatch",
        "path": path,
        "expected": want,
        "actual": have,
    })
}

// ─── the leg-IO seam ────────────────────────────────────────────
// EVERY byte this agent moves crosses one of these three functions and nothing
// else in the bundle speaks to the leg's stream verbs. The two IO bodies are
// the only place that knows HOW a chunk rides; `leg_failure` is
// channel-agnostic.
//
// A chunk rides THE BATCH (C37): one `Item::with_binary`, so every leg read and
// every leg write is an execution like any other — a call row, a ledger id, a
// task row in `ps`, a timeout, and a settle row. `send_with_binary` bypassed all
// of it, so a `write_stream` the model asked for left no trace the checkpoint
// ids could be read back from.

/// One `read_stream` chunk: the leg's reply and the raw chunk it carried, or
/// `None` when the verb returned no chunk at all (a refusal). `path` rides
/// VERBATIM — a truthy non-string is the leg's to refuse.
async fn leg_read_chunk(
    kernel: &Arc<Kernel>,
    fb: &str,
    path: &Value,
    offset: u64,
    length: u64,
) -> (Value, Option<Vec<u8>>) {
    let out = kernel
        .send(vec![Item::new(
            AgentId::from(fb),
            json!({"type": "read_stream", "path": path, "offset": offset, "length": length}),
        )
        .with_binary(Vec::new())])
        .await
        .remove(0);
    (out.result, out.body)
}

/// One `write_stream` chunk. An absent `offset`/`truncate` is python's append
/// at end (which creates the file).
async fn leg_write_chunk(
    kernel: &Arc<Kernel>,
    fb: &str,
    path: &Value,
    offset: Option<u64>,
    truncate: Option<bool>,
    bytes: Vec<u8>,
) -> Value {
    let mut header = Map::new();
    header.insert("type".to_string(), json!("write_stream"));
    header.insert("path".to_string(), path.clone());
    if let Some(o) = offset {
        header.insert("offset".to_string(), json!(o));
    }
    if let Some(t) = truncate {
        header.insert("truncate".to_string(), json!(t));
    }
    kernel
        .send(vec![
            Item::new(AgentId::from(fb), Value::Object(header)).with_binary(bytes)
        ])
        .await
        .remove(0)
        .result
}

/// python's `if not isinstance(r, dict) or r.get("error"):` — the ONE
/// leg-reply failure test, shared by every call site (the `.absent` probes and
/// the checkpoints listing included). A non-object reply becomes
/// `{"error": str(r)}` (`None` → `"None"`); an object fails only on a TRUTHY
/// `error`, so `{"error": ""}` is a success.
fn leg_failure(r: &Value) -> Option<Value> {
    match r {
        Value::Object(_) => py_truthy(arg(r, "error")).then(|| r.clone()),
        other => Some(json!({"error": py_str(other)})),
    }
}

/// One plain (non-binary) verb to the leg, gated as this agent.
async fn leg_call(kernel: &Arc<Kernel>, fb: &str, payload: Value) -> Value {
    kernel
        .send(vec![Item::new(AgentId::from(fb), payload)])
        .await
        .remove(0)
        .result
}

// ─── stream I/O (every byte through the gated leg) ──────────────

/// Whole file via the leg's `read_stream` chunk loop. `Ok(bytes)` or the leg's
/// own refusal passed through verbatim (not_found, unauthorized).
async fn read_all(kernel: &Arc<Kernel>, fb: &str, path: &Value) -> Result<Vec<u8>, Value> {
    let mut raw: Vec<u8> = Vec::new();
    let mut offset: u64 = 0;
    loop {
        let (r, body) = leg_read_chunk(kernel, fb, path, offset, CHUNK as u64).await;
        if let Some(err) = leg_failure(&r) {
            return Err(err);
        }
        // python `b = r.get("bytes") or b""` — no chunk is an empty chunk, which
        // ends the loop on the next test.
        let body = body.unwrap_or_default();
        raw.extend_from_slice(&body);
        if py_truthy(arg(&r, "eof")) || body.is_empty() {
            break;
        }
        offset = r
            .get("next_offset")
            .and_then(Value::as_u64)
            .unwrap_or(offset + body.len() as u64);
    }
    Ok(raw)
}

/// Write `data` to `path` on the leg — truncate-first, then append chunks.
/// NOT atomic on its own; `write_atomic` adds the tmp+rename commit.
async fn write_chunks(kernel: &Arc<Kernel>, fb: &str, path: &Value, data: &[u8]) -> Option<Value> {
    let mut off: usize = 0;
    let mut first = true;
    while first || off < data.len() {
        let end = (off + CHUNK).min(data.len());
        let chunk = data[off..end].to_vec();
        let clen = chunk.len();
        let r = leg_write_chunk(kernel, fb, path, Some(off as u64), Some(first), chunk).await;
        if let Some(err) = leg_failure(&r) {
            return Some(err);
        }
        off += clen;
        first = false;
    }
    None
}

/// The COMMIT: write to a tmp sibling, then the leg's one atomic `rename`.
/// A crash before the rename leaves only tmp debris.
async fn write_atomic(kernel: &Arc<Kernel>, fb: &str, path: &Value, data: &[u8]) -> Option<Value> {
    let tmp = Value::String(format!("{}.tmp.{}", py_str(path), pid()));
    if let Some(err) = write_chunks(kernel, fb, &tmp, data).await {
        return Some(err);
    }
    let r = leg_call(
        kernel,
        fb,
        json!({"type": "rename", "old_path": tmp.clone(), "new_path": path}),
    )
    .await;
    if let Some(err) = leg_failure(&r) {
        // The tmp sibling is debris now (a refused rename left it on disk,
        // python 2026-09-16); best-effort, the refusal is what is answered.
        let _ = leg_call(kernel, fb, json!({"type": "delete", "path": tmp})).await;
        return Some(err);
    }
    None
}

/// `/` in the document path would nest under `.ckpt`; flatten it so every
/// pre-image of one document lives in ONE listable dir. Both halves are
/// python's f-string, i.e. `str(x)`: a ckpt id is an int.
fn ckpt_path(path: &Value, ckpt_id: &Value) -> String {
    format!(
        "{CKPT_DIR}/{}/{}",
        py_str(path).replace('/', "__"),
        py_str(ckpt_id)
    )
}

/// The on-demand optimistic guard: only fires when the caller passed
/// `if_match` as anything other than null. `Some(refusal)` on mismatch, else
/// `None`.
async fn if_match_guard(
    kernel: &Arc<Kernel>,
    fb: &str,
    path: &Value,
    payload: &Value,
) -> Option<Value> {
    let want = payload.get("if_match")?;
    if want.is_null() {
        return None;
    }
    match read_all(kernel, fb, path).await {
        Err(e) => Some(e),
        Ok(data) => {
            let have = version_of(&data);
            if Value::String(have.clone()) == *want {
                None
            } else {
                Some(version_mismatch(want, &have))
            }
        }
    }
}

/// Commit the pre-image BEFORE any mutate touches the target. A file that does
/// not exist yet checkpoints as ABSENT (restore = delete).
async fn take_checkpoint(kernel: &Arc<Kernel>, fb: &str, path: &Value) -> Result<i64, Value> {
    let ckpt_id = pid();
    let data = match read_all(kernel, fb, path).await {
        Ok(d) => Some(d),
        Err(e) if e.get("reason").and_then(Value::as_str) == Some("not_found") => None,
        Err(e) => return Err(e),
    };
    let dest = ckpt_path(path, &json!(ckpt_id));
    let marker: &[u8] = data.as_deref().unwrap_or(b"");
    if let Some(err) = write_chunks(kernel, fb, &Value::String(dest.clone()), marker).await {
        return Err(err);
    }
    // An absent pre-image is recorded as an empty file with an `.absent` twin —
    // restore then deletes the document instead of emptying it.
    if data.is_none() {
        let twin = Value::String(format!("{dest}.absent"));
        if let Some(err) = write_chunks(kernel, fb, &twin, b"").await {
            return Err(err);
        }
    }
    Ok(ckpt_id)
}

/// The `.absent` twin probe: one 1-byte read that only has to succeed.
async fn absent_twin(kernel: &Arc<Kernel>, fb: &str, src: &str) -> bool {
    let probe = Value::String(format!("{src}.absent"));
    let (absent, _) = leg_read_chunk(kernel, fb, &probe, 0, 1).await;
    leg_failure(&absent).is_none()
}

// ─── patch grammar (moved from file_bridge — same guarantees) ───

const H_START: &str = "<<<<<<< SEARCH";
const H_DIV: &str = "=======";
const H_END: &str = ">>>>>>> REPLACE";

/// Parse SEARCH/REPLACE blocks into `(search, replace)` pairs, VERBATIM inside
/// the markers. `Err` on a block that opens but does not close.
fn parse_hunks(text: &str) -> Result<Vec<(String, String)>, String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut hunks: Vec<(String, String)> = Vec::new();
    let n = lines.len();
    let mut i = 0;
    while i < n {
        if lines[i] != H_START {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut search: Vec<&str> = Vec::new();
        while j < n && lines[j] != H_DIV {
            search.push(lines[j]);
            j += 1;
        }
        if j >= n {
            return Err("a SEARCH block has no `=======` divider".to_string());
        }
        j += 1;
        let mut replace: Vec<&str> = Vec::new();
        while j < n && lines[j] != H_END {
            replace.push(lines[j]);
            j += 1;
        }
        if j >= n {
            return Err("a block has no closing `>>>>>>> REPLACE`".to_string());
        }
        hunks.push((search.join("\n"), replace.join("\n")));
        i = j + 1;
    }
    Ok(hunks)
}

/// py `str.count` (non-overlapping); empty needle mirrors py len+1 so
/// `hits != 1` still fails loud.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return haystack.chars().count() + 1;
    }
    haystack.matches(needle).count()
}

// ─── verbs ──────────────────────────────────────────────────────

/// python's `path = payload.get("path", ""); if not path: missing(...)`. Only a
/// FALSY path is this agent's to refuse; a truthy non-string rides to the leg
/// verbatim and gets the leg's own `bad_arg` (the tag grammar types
/// `<path>2024</path>` as an int).
fn path_of<'a>(payload: &'a Value, verb: &str) -> Result<&'a Value, Value> {
    let p = arg(payload, "path");
    if py_truthy(p) {
        Ok(p)
    } else {
        Err(missing(verb, "path"))
    }
}

/// The document/patch this call carries. A str `bytes` in the payload IS the
/// document, utf-8-encoded (py text courtesy, 2026-09-01: ai_core encodes for
/// the MODEL, but a spawned program's ctrl-fd wire is JSON and has no bytes
/// type, so this is its one write path); otherwise the binary channel's blob.
/// Neither ⇒ python's per-verb `missing`.
fn blob_of<'a>(
    payload: &'a Value,
    blob: Option<&'a [u8]>,
    verb: &str,
    arg_text: &str,
) -> Result<Cow<'a, [u8]>, Value> {
    if let Some(s) = payload.get("bytes").and_then(Value::as_str) {
        return Ok(Cow::Borrowed(s.as_bytes()));
    }
    // PRESENCE, NOT LENGTH. Python asks `isinstance(payload["bytes"], bytes)`,
    // so a document that IS empty is a legal write; testing `blob.is_empty()`
    // refused `<bytes></bytes>` as if the argument had been left out.
    match blob {
        Some(b) => Ok(Cow::Borrowed(b)),
        None => Err(missing(verb, arg_text)),
    }
}

async fn reflect_reply(agent: &Agent) -> Value {
    let sentence = agent
        .meta
        .read()
        .ok()
        .and_then(|g| {
            g.get("description")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            "Text documents on a file surface — any format, the caller's choice: \
             line-read, atomic write, SEARCH/REPLACE patch, append; versioning \
             and checkpoints ON DEMAND."
                .to_string()
        });
    let target_id = agent.meta.read().ok().and_then(|g| {
        g.get("target_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    });
    json!({
        "id": agent.id.0,
        "sentence": sentence,
        "target_id": target_id,
        "patch_grammar": "one or more blocks: `<<<<<<< SEARCH` / <old lines> / \
            `=======` / <new lines> / `>>>>>>> REPLACE`; each SEARCH must match \
            its document EXACTLY ONCE. \
            `patch` is a TRANSACTION over `edits:[{path, hunks, if_match?}]` — \
            even one file rides the array; the blob holds every edit's hunks \
            concatenated in `edits` order, split by the per-edit `hunks` count; \
            any failure anywhere writes nothing",
        "compose": "every content-carrying verb (`write`, `patch`, `append`) \
            takes its document in `bytes`, and `bytes` is VERBATIM: write the \
            document between `<bytes>` and `</bytes>` exactly as it should land — \
            real newlines, real quotes, nothing escaped, no fence — then close \
            the call. The document ends at the first `</bytes>`. A patch names \
            its edits as tags: \
            <edits><item><path>a.py</path><hunks>2</hunks></item></edits>",
        // Every line here is the first line of the python verb's docstring,
        // verbatim (`tools.py` VERBS, rendered by `_reflect`).
        "verbs": {
            "reflect": "no args — identity, verb guide, and the wired default surface.",
            "boot": "no args — no-op; this agent holds no state to hydrate.",
            "read": "args: path:str (req), on:str?, offset:int=0 (line), limit:int=200 — {path, lines:[{n,text}], total, eof, version}; refuses binary.",
            "write": "args: path:str (req), bytes (req — the whole document; str is utf-8-encoded), on:str?, if_match:str?, checkpoint:bool? — atomic tmp+rename; {path, size, version, ckpt_id?}.",
            "patch": "args: edits:[{path:str, hunks:int, if_match:str?}] (req — even for ONE file), bytes (req — ALL hunks concatenated; str is utf-8-encoded), on:str?, dry_run:bool?, checkpoint:bool? — a TRANSACTION: verify every hunk of every edit, checkpoint all, apply all, drop the pre-images; any failure writes nothing (or restores what was written); {patched:true, edits:[{path,hunks,size,version}], ckpt_ids?}.",
            "append": "args: path:str (req), bytes (req — one chunk; str is utf-8-encoded), on:str? — appends (creates if absent); {path, size, version}.",
            "checkpoint": "args: path:str (req), on:str? — commit a pre-image now, mutation-free; {ckpt_id, path, version}.",
            "checkpoints": "args: path:str (req), on:str? — the pre-images held for a document; {path, ckpts:[{id, size}]}.",
            "restore": "args: path:str (req), ckpt_id:str (req), on:str?, if_match:str? — atomic restore of the pre-image (or delete, if it was absent); {restored:true, path, version|deleted}.",
            "drop": "args: path:str (req), ckpt_id:str (req), on:str? — discard one pre-image (retention is the caller's); {dropped:true}.",
        },
    })
}

async fn read_reply(kernel: &Arc<Kernel>, fb: &str, payload: &Value) -> Value {
    let path = match path_of(payload, "read") {
        Ok(p) => p,
        Err(e) => return e,
    };
    // Type the paging args BEFORE the read — a typo'd `offset` costs no IO.
    let off = match int_arg(payload, "offset", 0, "read", 0) {
        Ok(v) => v as usize,
        Err(refusal) => return refusal,
    };
    let limit = match int_arg(payload, "limit", 200, "read", 1) {
        Ok(v) => v as usize,
        Err(refusal) => return refusal,
    };
    let data = match read_all(kernel, fb, path).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    if data.contains(&0u8) {
        return bad("read", "path", "a text file (this one is binary)");
    }
    let text = match std::str::from_utf8(&data) {
        Ok(t) => t,
        Err(_) => return bad("read", "path", "valid utf-8 (this file is not)"),
    };
    let mut all_lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline yields one phantom empty tail element — drop it so
    // `total` counts real lines.
    if all_lines.last() == Some(&"") {
        all_lines.pop();
    }
    let end = off.saturating_add(limit).min(all_lines.len());
    let page: Vec<Value> = all_lines
        .get(off..end)
        .unwrap_or(&[])
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"n": off + i + 1, "text": t}))
        .collect();
    json!({
        "path": path,
        "lines": page,
        "total": all_lines.len(),
        "eof": off.saturating_add(limit) >= all_lines.len(),
        "version": version_of(&data),
    })
}

async fn write_reply(
    kernel: &Arc<Kernel>,
    fb: &str,
    payload: &Value,
    blob: Option<&[u8]>,
) -> Value {
    let path = match path_of(payload, "write") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let data = match blob_of(
        payload,
        blob,
        "write",
        "bytes (the document — str is utf-8-encoded)",
    ) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let key = py_str(path);
    let _held = hold(fb, &[key.as_str()]).await;
    if let Some(err) = if_match_guard(kernel, fb, path, payload).await {
        return err;
    }
    let mut ckpt_id: i64 = 0;
    if py_truthy(arg(payload, "checkpoint")) {
        match take_checkpoint(kernel, fb, path).await {
            Ok(id) => ckpt_id = id,
            Err(e) => return e,
        }
    }
    if let Some(err) = write_atomic(kernel, fb, path, &data).await {
        return err;
    }
    let mut out = json!({"path": path, "size": data.len(), "version": version_of(&data)});
    // python `if ckpt_id:` — an operator write outside any dispatch mints 0,
    // and 0 is not a checkpoint anyone can name.
    if ckpt_id != 0 {
        out["ckpt_id"] = json!(ckpt_id);
    }
    out
}

async fn append_reply(
    kernel: &Arc<Kernel>,
    fb: &str,
    payload: &Value,
    blob: Option<&[u8]>,
) -> Value {
    let path = match path_of(payload, "append") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let data = match blob_of(
        payload,
        blob,
        "append",
        "bytes (one chunk — str is utf-8-encoded)",
    ) {
        Ok(d) => d,
        Err(e) => return e,
    };
    let key = py_str(path);
    let _held = hold(fb, &[key.as_str()]).await;
    // absent offset = append at end (creates the file)
    let r = leg_write_chunk(kernel, fb, path, None, None, data.to_vec()).await;
    if let Some(err) = leg_failure(&r) {
        return err;
    }
    let whole = match read_all(kernel, fb, path).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    json!({
        "path": path,
        "size": arg(&r, "size"),
        "version": version_of(&whole),
    })
}

async fn patch_reply(
    kernel: &Arc<Kernel>,
    fb: &str,
    payload: &Value,
    blob: Option<&[u8]>,
) -> Value {
    // A TRANSACTION over `edits:[{path, hunks, if_match?}]` — even one file
    // rides the array (one shape, no single/multi drift; user decision
    // 2026-09-01). ONE raw hunk blob, split by the per-edit counts; verify
    // every hunk of every edit in memory, checkpoint all, apply all, then
    // self-clean the pre-images (`checkpoint:true` keeps them). A mid-apply
    // failure restores everything already written. Mirrors python (canonical).
    let Some(edits) = payload
        .get("edits")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return missing("patch", "edits (a list of {path, hunks, if_match?})");
    };
    let arg_text = "bytes (the utf-8 patch — all hunks, in edits order)";
    let data = match blob_of(payload, blob, "patch", arg_text) {
        Ok(d) => d,
        Err(e) => return e,
    };
    // python: `not isinstance(data, (bytes, bytearray)) or not data` — an EMPTY
    // patch is a missing document, not a malformed one.
    if data.is_empty() {
        return missing("patch", arg_text);
    }
    let patch = match std::str::from_utf8(&data) {
        Ok(s) => s,
        Err(_) => return bad("patch", "bytes", "valid utf-8 (the patch is text)"),
    };
    let hunks = match parse_hunks(patch) {
        Ok(h) => h,
        Err(e) => return bad("patch", "bytes", &format!("a well-formed patch — {e}")),
    };
    if hunks.is_empty() {
        return bad(
            "patch",
            "bytes",
            "at least one `<<<<<<< SEARCH` / `=======` / `>>>>>>> REPLACE` block",
        );
    }
    // ── address the one hunk stream: split by per-edit counts ──
    struct Edit {
        path: String,
        hunks: Vec<(String, String)>,
        if_match: Value,
        new: Vec<u8>,
    }
    let mut plan: Vec<Edit> = Vec::new();
    let mut cursor = 0usize;
    for (i, e) in edits.iter().enumerate() {
        let Some(path) = e
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            return bad(
                "patch",
                "edits",
                &format!("edit {i}: an object with a `path`"),
            );
        };
        let Some(n) = e.get("hunks").and_then(Value::as_u64).filter(|n| *n >= 1) else {
            return bad(
                "patch",
                "edits",
                &format!("edit {i} ({}): `hunks` (int >= 1)", py_repr(path)),
            );
        };
        let n = n as usize;
        if plan.iter().any(|p| p.path == path) {
            return bad(
                "patch",
                "edits",
                "each path at most once — put all of a file's hunks in one edit",
            );
        }
        // python slices `hunks[cursor:cursor + n]` — it CLAMPS and keeps going,
        // so every edit is validated and `cursor` holds the full claim for the
        // sum check below.
        let lo = cursor.min(hunks.len());
        let hi = cursor.saturating_add(n).min(hunks.len());
        plan.push(Edit {
            path: path.to_string(),
            hunks: hunks[lo..hi].to_vec(),
            if_match: arg(e, "if_match").clone(),
            new: Vec::new(),
        });
        cursor = cursor.saturating_add(n);
    }
    if cursor != hunks.len() {
        return bad(
            "patch",
            "edits",
            &format!(
                "hunk counts that sum to the blob's blocks — edits claim {cursor}, \
                 the patch holds {}",
                hunks.len()
            ),
        );
    }
    // Every edit's document is held from the first read to the last rename.
    let paths: Vec<&str> = plan.iter().map(|e| e.path.as_str()).collect();
    let _held = hold(fb, &paths).await;
    // ── VERIFY ALL BEFORE ANY: compute every file's new content in memory ──
    for e in plan.iter_mut() {
        let path = Value::String(e.path.clone());
        let raw = match read_all(kernel, fb, &path).await {
            Ok(d) => d,
            Err(err) => return err,
        };
        if !e.if_match.is_null() {
            let have = version_of(&raw);
            if Value::String(have.clone()) != e.if_match {
                return version_mismatch_at(&e.path, &e.if_match, &have);
            }
        }
        let mut working = match String::from_utf8(raw) {
            Ok(s) => s,
            Err(_) => {
                return bad(
                    "patch",
                    "edits",
                    &format!("{}: a utf-8 text file", py_repr(&e.path)),
                )
            }
        };
        for (idx, (search, replace)) in e.hunks.iter().enumerate() {
            let hits = count_occurrences(&working, search);
            if hits != 1 {
                let (reason, detail) = if hits == 0 {
                    ("no_match", "search text not found".to_string())
                } else {
                    ("ambiguous", format!("search text matches {hits} places"))
                };
                return json!({
                    "error": "a hunk did not apply — nothing written",
                    "reason": reason,
                    "failed": [{"path": e.path, "index": idx, "reason": detail}],
                });
            }
            working = working.replacen(search, replace, 1);
        }
        e.new = working.into_bytes();
    }
    if py_truthy(arg(payload, "dry_run")) {
        return json!({
            "dry_run": true,
            "edits": plan.iter().map(|e| json!({"path": e.path, "hunks": e.hunks.len()})).collect::<Vec<_>>(),
        });
    }
    // ── CHECKPOINT ALL, then APPLY ALL; a mid-apply failure restores ──
    // everything already written. Pre-images are ALWAYS taken (they ARE the
    // rollback); `checkpoint:true` only KEEPS them.
    let mut ckpts: Vec<(String, i64)> = Vec::new();
    for e in &plan {
        match take_checkpoint(kernel, fb, &Value::String(e.path.clone())).await {
            Ok(id) => ckpts.push((e.path.clone(), id)),
            Err(err) => return err,
        }
    }
    let mut written: Vec<(String, i64)> = Vec::new();
    for (e, (_, ckpt_id)) in plan.iter().zip(&ckpts) {
        if let Some(err) = write_atomic(kernel, fb, &Value::String(e.path.clone()), &e.new).await {
            for (path, ck) in &written {
                restore_from(kernel, fb, path, *ck).await;
            }
            let mut out = err;
            out["restored"] = json!(written.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>());
            out["failed_path"] = json!(e.path);
            return out;
        }
        written.push((e.path.clone(), *ckpt_id));
    }
    let mut out = json!({
        "patched": true,
        "edits": plan
            .iter()
            .map(|e| json!({
                "path": e.path,
                "hunks": e.hunks.len(),
                "size": e.new.len(),
                "version": version_of(&e.new),
            }))
            .collect::<Vec<_>>(),
    });
    if py_truthy(arg(payload, "checkpoint")) {
        out["ckpt_ids"] = json!(ckpts
            .iter()
            .map(|(p, c)| json!({"path": p, "ckpt_id": c}))
            .collect::<Vec<_>>());
    } else {
        for (path, ck) in &ckpts {
            drop_quiet(kernel, fb, path, *ck).await;
        }
    }
    out
}

/// Rollback arm of the patch transaction: put the pre-image back (or delete a
/// file that did not exist). Best-effort — it runs INSIDE a failure path that
/// is already being reported.
async fn restore_from(kernel: &Arc<Kernel>, fb: &str, path: &str, ckpt_id: i64) {
    let path = Value::String(path.to_string());
    let src = ckpt_path(&path, &json!(ckpt_id));
    if absent_twin(kernel, fb, &src).await {
        let _ = leg_call(kernel, fb, json!({"type": "delete", "path": path})).await;
        return;
    }
    if let Ok(data) = read_all(kernel, fb, &Value::String(src)).await {
        let _ = write_atomic(kernel, fb, &path, &data).await;
    }
}

/// Self-cleaning arm: a COMMITTED transaction discards its pre-images.
async fn drop_quiet(kernel: &Arc<Kernel>, fb: &str, path: &str, ckpt_id: i64) {
    let src = ckpt_path(&Value::String(path.to_string()), &json!(ckpt_id));
    let _ = leg_call(kernel, fb, json!({"type": "delete", "path": src.clone()})).await;
    let _ = leg_call(
        kernel,
        fb,
        json!({"type": "delete", "path": format!("{src}.absent")}),
    )
    .await;
}

async fn checkpoint_reply(kernel: &Arc<Kernel>, fb: &str, payload: &Value) -> Value {
    let path = match path_of(payload, "checkpoint") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let data = match read_all(kernel, fb, path).await {
        Ok(d) => d,
        Err(e) => return e,
    };
    match take_checkpoint(kernel, fb, path).await {
        Ok(id) => json!({"ckpt_id": id, "path": path, "version": version_of(&data)}),
        Err(e) => e,
    }
}

async fn checkpoints_reply(kernel: &Arc<Kernel>, fb: &str, payload: &Value) -> Value {
    let path = match path_of(payload, "checkpoints") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dir = format!("{CKPT_DIR}/{}", py_str(path).replace('/', "__"));
    let r = leg_call(kernel, fb, json!({"type": "list", "path": dir})).await;
    if leg_failure(&r).is_some() {
        // no dir yet = no checkpoints, honestly empty
        return json!({"path": path, "ckpts": []});
    }
    let ckpts: Vec<Value> = r
        .get("files")
        .and_then(Value::as_array)
        .map(|files| {
            files
                .iter()
                .filter(|f| {
                    f.get("type").and_then(Value::as_str) == Some("file")
                        && !f
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .ends_with(".absent")
                })
                .map(|f| {
                    let name = f.get("name").and_then(Value::as_str).unwrap_or("");
                    // python `int(name) if name.isdigit() else name` — a ckpt id
                    // IS a pid, and a pid is a number.
                    let id = match name.parse::<i64>() {
                        Ok(n) if !name.is_empty() && name.bytes().all(|b| b.is_ascii_digit()) => {
                            json!(n)
                        }
                        _ => json!(name),
                    };
                    json!({"id": id, "size": arg(f, "size")})
                })
                .collect()
        })
        .unwrap_or_default();
    json!({"path": path, "ckpts": ckpts})
}

async fn restore_reply(kernel: &Arc<Kernel>, fb: &str, payload: &Value) -> Value {
    let path = match path_of(payload, "restore") {
        Ok(p) => p,
        Err(e) => return e,
    };
    // A ckpt id is whatever the notation typed it as — the model writes back
    // the int it was shown. Only a FALSY value is missing.
    let ckpt_id = arg(payload, "ckpt_id");
    if !py_truthy(ckpt_id) {
        return missing("restore", "ckpt_id");
    }
    let key = py_str(path);
    let _held = hold(fb, &[key.as_str()]).await;
    if let Some(err) = if_match_guard(kernel, fb, path, payload).await {
        return err;
    }
    let src = ckpt_path(path, ckpt_id);
    // An `.absent` twin means the pre-image was NO FILE — restore deletes.
    if absent_twin(kernel, fb, &src).await {
        let r = leg_call(kernel, fb, json!({"type": "delete", "path": path})).await;
        if let Some(err) = leg_failure(&r) {
            return err;
        }
        return json!({"restored": true, "path": path, "deleted": true});
    }
    let data = match read_all(kernel, fb, &Value::String(src)).await {
        Ok(d) => d,
        Err(e) => {
            if e.get("reason").and_then(Value::as_str) == Some("not_found") {
                return refusal(
                    NOT_FOUND,
                    format!(
                        "restore: no checkpoint {} for {}",
                        py_repr_value(ckpt_id),
                        py_repr_value(path)
                    ),
                );
            }
            return e;
        }
    };
    if let Some(err) = write_atomic(kernel, fb, path, &data).await {
        return err;
    }
    json!({"restored": true, "path": path, "version": version_of(&data)})
}

async fn drop_reply(kernel: &Arc<Kernel>, fb: &str, payload: &Value) -> Value {
    let path = match path_of(payload, "drop") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let ckpt_id = arg(payload, "ckpt_id");
    if !py_truthy(ckpt_id) {
        return missing("drop", "ckpt_id");
    }
    let src = ckpt_path(path, ckpt_id);
    let r = leg_call(kernel, fb, json!({"type": "delete", "path": src.clone()})).await;
    if let Some(err) = leg_failure(&r) {
        return err;
    }
    // best-effort: the `.absent` twin, if any (its absence is not an error)
    let _ = leg_call(
        kernel,
        fb,
        json!({"type": "delete", "path": format!("{src}.absent")}),
    )
    .await;
    json!({"dropped": true, "ckpt_id": ckpt_id, "path": path})
}

/// python `refusal(UNHANDLED, f"text_file: unknown type {t!r}", verb=t)` — the
/// RAW type value, repr'd (an int bare, a str quoted).
fn unknown_type(verb: &Value) -> Value {
    let mut r = refusal(
        UNHANDLED,
        format!("text_file: unknown type {}", py_repr_value(verb)),
    );
    with(&mut r, "verb", verb.clone());
    r
}

/// One dispatch used by BOTH channels — the byte verbs take the blob, the rest
/// ignore it.
async fn dispatch(
    agent: &Agent,
    payload: &Value,
    blob: Option<&[u8]>,
    kernel: &Arc<Kernel>,
) -> Value {
    let raw_verb = arg(payload, "type");
    let verb = raw_verb.as_str().unwrap_or("");
    match verb {
        "reflect" => return reflect_reply(agent).await,
        "boot" => return Value::Null,
        // python's VERBS map has no `shutdown`, so it is an unknown type.
        "read" | "write" | "patch" | "append" | "checkpoint" | "checkpoints" | "restore"
        | "drop" => {}
        _ => return unknown_type(raw_verb),
    }
    let Some(fb) = target_of(agent, payload) else {
        return unwired(verb);
    };
    match verb {
        "read" => read_reply(kernel, &fb, payload).await,
        "write" => write_reply(kernel, &fb, payload, blob).await,
        "patch" => patch_reply(kernel, &fb, payload, blob).await,
        "append" => append_reply(kernel, &fb, payload, blob).await,
        "checkpoint" => checkpoint_reply(kernel, &fb, payload).await,
        "checkpoints" => checkpoints_reply(kernel, &fb, payload).await,
        "restore" => restore_reply(kernel, &fb, payload).await,
        _ => drop_reply(kernel, &fb, payload).await,
    }
}

#[async_trait]
impl Bundle for TextFileBundle {
    fn name(&self) -> &str {
        "text_file"
    }

    fn readme(&self) -> Option<&'static str> {
        Some(README)
    }

    /// python `validate_meta`: `target_id`, when present, must be a non-empty
    /// agent-id string (the default surface this agent operates on).
    fn validate_meta(
        &self,
        meta: &Map<String, Value>,
        _record: &Map<String, Value>,
    ) -> Option<String> {
        match meta.get("target_id") {
            Some(t) if t.as_str().is_some_and(|s| !s.is_empty()) => None,
            Some(_) => Some("target_id must be a non-empty file_bridge agent id".to_string()),
            None => None,
        }
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
            return Ok(Some(json!({"error": format!("no agent {agent_id}")})));
        };
        // NO early bytes refusal here: python types the args IN the verb, so an
        // unwired agent, or a missing `path`, is reported before anyone looks
        // for a document. `blob_of` raises the per-verb `missing` in its place.
        Ok(Some(dispatch(&agent, payload, None, kernel).await))
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
        let Some(agent) = kernel.agents.get(agent_id).map(|e| Arc::clone(&e)) else {
            return Ok((Some(json!({"error": format!("no agent {agent_id}")})), None));
        };
        // text_file answers documents, never raw chunks — no reply body (C37).
        // THE BLOB IS THE `bytes` ARGUMENT, present even when empty — that is
        // the bytes-TYPED carrier a model's `<bytes>` document rides (A13.19).
        Ok((
            Some(dispatch(&agent, &header, Some(&blob), kernel).await),
            None,
        ))
    }
}

#[cfg(test)]
mod tests;
