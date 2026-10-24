//! The text verbs — the editing organ, python `file_bridge` `_read` / `_write` /
//! `_edit` / `_append` / `_undo`.
//!
//! A text document is edited HERE, with the caller's OWN grants on this bridge.
//! Every byte goes through [`crate::fs`] under this bridge's clamp, `allow` list
//! and `readonly`; a commit is [`crate::fs::write_bytes`] (tmp sibling + atomic
//! replace), the tmp judged by the TARGET path's exposure.

use crate::{allow_list, clamped, fs, gate_path, int_arg, norm, path_arg, readonly_refusal};
use fantastic_bundle::{py_repr, py_truthy};
use fantastic_kernel::refusal as rf;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Lines per `read` page when the caller names no `limit`.
pub(crate) const READ_LIMIT: u64 = 200;

/// The verbs this module answers.
pub(crate) const TEXT_VERBS: [&str; 5] = ["read", "write", "edit", "append", "undo"];

type Key = (String, String);
type LockMap = Mutex<HashMap<Key, Arc<tokio::sync::Mutex<()>>>>;

/// The text verbs' RAM state, one per registered [`crate::FileBundle::default()`] (so per
/// kernel): the undo slots and the per-document locks.
#[derive(Default)]
pub(crate) struct TextState {
    /// ONE undo slot per (bridge, path): the bytes the last `write`/`edit`/
    /// `append` replaced (`None` = the file did not exist). `undo` swaps file ↔
    /// slot, so a second `undo` is redo. RAM only — nothing on disk, lost on
    /// restart.
    /// TODO: memory leak possible — one slot per edited path, unbounded paths × size.
    undo: Mutex<HashMap<Key, Option<Vec<u8>>>>,
    locks: LockMap,
}

impl TextState {
    fn slots(&self) -> MutexGuard<'_, HashMap<Key, Option<Vec<u8>>>> {
        self.undo.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The bridge is going away — its undo slots go with it.
    pub(crate) fn drop_slots(&self, bridge: &str) {
        self.slots().retain(|(b, _), _| b != bridge);
    }

    // ─── per-document serialization ─────────────────────────────────
    // A mutate is read → verify → commit; two concurrent mutates on ONE document
    // interleaved there would both derive from the same pre-image and the second
    // commit would silently discard the first. One lock per (bridge, path), held from
    // the first read through the commit; `if_match` is checked UNDER it. A multi-file
    // edit takes its locks in sorted order so overlapping transactions cannot
    // deadlock. Entries are pruned once nobody but the map holds them.

    async fn hold(&self, bridge: &str, paths: &[String]) -> Held<'_> {
        let mut keys: Vec<Key> = paths
            .iter()
            .map(|p| (bridge.to_string(), p.clone()))
            .collect();
        keys.sort();
        keys.dedup();
        let arcs: Vec<Arc<tokio::sync::Mutex<()>>> = {
            let mut m = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            keys.iter()
                .map(|k| Arc::clone(m.entry(k.clone()).or_default()))
                .collect()
        };
        let mut guards = Vec::with_capacity(arcs.len());
        for a in arcs {
            guards.push(a.lock_owned().await);
        }
        Held {
            map: &self.locks,
            keys,
            guards,
        }
    }
}

/// The held document locks — released (and pruned) on drop.
struct Held<'a> {
    map: &'a LockMap,
    keys: Vec<Key>,
    guards: Vec<tokio::sync::OwnedMutexGuard<()>>,
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.guards.clear();
        let mut m = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        for k in &self.keys {
            if m.get(k).is_some_and(|a| Arc::strong_count(a) == 1) {
                m.remove(k);
            }
        }
    }
}

/// The on-demand version: a content hash, computed, never stored.
pub(crate) fn version_of(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    let hex: String = d.iter().map(|b| format!("{b:02x}")).collect();
    let mut v = hex[..16].to_string();
    // A version that parses as a number would arrive as one and lose its digits;
    // hex never starts with a non-digit number form, so 'a' breaks the parse.
    if v.parse::<f64>().is_ok() {
        v.replace_range(0..1, "a");
    }
    v
}

/// The lock / slot key of a path — its normalized form.
fn key_of(path: &str) -> String {
    norm(path).unwrap_or_else(|| path.to_string())
}

/// python `_doc`: the document this call carries. On the BINARY channel the blob
/// IS the `bytes` argument (present even when empty); on the text channel a str
/// `bytes` is utf-8-encoded (a spawned program's ctrl-fd wire is JSON and has no
/// bytes type). Anything else is `missing`.
fn doc(verb: &str, payload: &Value, blob: Option<&[u8]>, what: &str) -> Result<Vec<u8>, Value> {
    if let Some(b) = blob {
        return Ok(b.to_vec());
    }
    match payload.get("bytes") {
        Some(Value::String(s)) => Ok(s.as_bytes().to_vec()),
        _ => Err(rf::missing(
            verb,
            &format!("bytes ({what} — str is utf-8-encoded)"),
        )),
    }
}

/// The document's bytes, or `None` when no file is there.
fn current(root: &Path, path: &str) -> Result<Option<Vec<u8>>, String> {
    if fs::is_file(root, path)? {
        fs::read_bytes(root, path).map(Some)
    } else {
        Ok(None)
    }
}

/// The on-demand optimistic guard: a refusal when `if_match` names another
/// version (an absent document has no version and never matches). `expected`
/// echoes the caller's RAW value.
/// Does the caller's `if_match` name this version? A string compares as text;
/// a JSON number matches when its text form is the version, or when the version
/// parses as the same JSON number (e.g. `12e45` arrives as f64 and was never
/// the digits the caller meant).
pub(crate) fn same_version(want: &Value, have: &str) -> bool {
    match want {
        Value::String(s) => s == have,
        Value::Number(n) => {
            n.to_string() == have
                || serde_json::from_str::<Value>(have)
                    .ok()
                    .is_some_and(|v| v.as_number() == Some(n))
        }
        _ => false,
    }
}

fn mismatch(path: &str, want: &Value, have_bytes: Option<&[u8]>) -> Option<Value> {
    let have = have_bytes.map(version_of);
    if have.as_deref().is_some_and(|h| same_version(want, h)) {
        return None;
    }
    Some(json!({
        "error": "version mismatch — the document changed since you read it",
        "reason": "version_mismatch",
        "path": path,
        "expected": want,
        "actual": have,
    }))
}

/// `if_match` as python reads it: present and not null.
fn if_match(payload: &Value) -> Option<&Value> {
    payload.get("if_match").filter(|v| !v.is_null())
}

/// python `_text_gate`: readonly for a mutate, a real path, the clamp and the
/// allow list on both ends. `Ok(root)` or the refusal.
fn text_gate<'a>(
    verb: &str,
    root: &'a Result<PathBuf, String>,
    meta: &Map<String, Value>,
    path: &str,
    mutate: bool,
    readonly: bool,
) -> Result<&'a Path, Value> {
    if mutate && readonly {
        return Err(readonly_refusal());
    }
    if path.is_empty() {
        return Err(rf::missing(verb, "path"));
    }
    if let Some(err) = gate_path(verb, root, &allow_list(meta), path) {
        return Err(err);
    }
    clamped(root)
}

fn transport(e: String) -> Value {
    rf::refusal(rf::TRANSPORT_ERROR, e)
}

fn not_found(root: &std::path::Path, path: &str) -> Value {
    let hint = crate::fs::not_found_hint(root, path);
    let mut v = rf::refusal(
        rf::NOT_FOUND,
        format!("file {} not found{hint}", py_repr(path)),
    );
    rf::with(&mut v, "path", json!(path));
    v
}

/// One text verb. `blob` is the binary channel's body (`None` on the text
/// channel).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn dispatch(
    st: &TextState,
    verb: &str,
    bridge: &str,
    meta: &Map<String, Value>,
    root: &Result<PathBuf, String>,
    payload: &Value,
    blob: Option<&[u8]>,
    readonly: bool,
) -> Value {
    match verb {
        "read" => read(bridge, meta, root, payload),
        "write" => write(st, bridge, meta, root, payload, blob, readonly).await,
        "append" => append(st, bridge, meta, root, payload, blob, readonly).await,
        "edit" => edit(st, bridge, meta, root, payload, blob, readonly).await,
        _ => undo(st, bridge, meta, root, payload, readonly).await,
    }
}

fn read(
    bridge: &str,
    meta: &Map<String, Value>,
    root: &Result<PathBuf, String>,
    payload: &Value,
) -> Value {
    let path = match path_arg("read", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let off = match int_arg("read", payload, "offset", 0, 0) {
        Ok(n) => n as usize,
        Err(e) => return e,
    };
    let limit = match int_arg("read", payload, "limit", READ_LIMIT, 1) {
        Ok(n) => n as usize,
        Err(e) => return e,
    };
    let root = match text_gate("read", root, meta, path, false, false) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let data = match current(root, path) {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(root, path),
        Err(e) => return transport(e),
    };
    if data.contains(&0u8) {
        return rf::bad("read", "path", "a text file (this one is binary)");
    }
    let Ok(text) = std::str::from_utf8(&data) else {
        return rf::bad("read", "path", "valid utf-8 (this file is not)");
    };
    let mut all_lines: Vec<&str> = text.split('\n').collect();
    // A trailing newline yields one phantom empty tail element — dropped so
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
    let ver = version_of(&data);
    // HOW TO GET THIS BODY AGAIN — one line; a reader that keeps no body keeps this.
    let origin = format!(
        "{bridge} {path} lines {}-{} version {ver} — read {{path: {path}, offset: {off}, \
         limit: {limit}}}; a different version means the file changed since",
        off + 1,
        off + page.len()
    );
    json!({
        "path": path,
        "lines": page,
        "total": all_lines.len(),
        "eof": off.saturating_add(limit) >= all_lines.len(),
        "version": ver,
        "origin": origin,
    })
}

async fn write(
    st: &TextState,
    bridge: &str,
    meta: &Map<String, Value>,
    root: &Result<PathBuf, String>,
    payload: &Value,
    blob: Option<&[u8]>,
    readonly: bool,
) -> Value {
    let path = match path_arg("write", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let root = match text_gate("write", root, meta, path, true, readonly) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let data = match doc("write", payload, blob, "the whole document") {
        Ok(d) => d,
        Err(e) => return e,
    };
    let key = key_of(path);
    let _held = st.hold(bridge, std::slice::from_ref(&key)).await;
    let before = match current(root, path) {
        Ok(b) => b,
        Err(e) => return transport(e),
    };
    if let Some(want) = if_match(payload) {
        if let Some(err) = mismatch(path, want, before.as_deref()) {
            return err;
        }
    }
    if let Err(e) = fs::write_bytes(root, path, &data, true, None) {
        return transport(e);
    }
    st.slots().insert((bridge.to_string(), key), before);
    json!({"path": path, "size": data.len(), "version": version_of(&data)})
}

async fn append(
    st: &TextState,
    bridge: &str,
    meta: &Map<String, Value>,
    root: &Result<PathBuf, String>,
    payload: &Value,
    blob: Option<&[u8]>,
    readonly: bool,
) -> Value {
    let path = match path_arg("append", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let root = match text_gate("append", root, meta, path, true, readonly) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let data = match doc("append", payload, blob, "one chunk") {
        Ok(d) => d,
        Err(e) => return e,
    };
    let key = key_of(path);
    let _held = st.hold(bridge, std::slice::from_ref(&key)).await;
    let before = match current(root, path) {
        Ok(b) => b,
        Err(e) => return transport(e),
    };
    let size = match fs::write_chunk(root, path, &data, None, false) {
        Ok((_, size)) => size,
        Err(e) => return transport(e),
    };
    let whole = match fs::read_bytes(root, path) {
        Ok(w) => w,
        Err(e) => return transport(e),
    };
    st.slots().insert((bridge.to_string(), key), before);
    json!({"path": path, "size": size, "version": version_of(&whole)})
}

/// SEARCH/REPLACE blocks → `(file, search, replace)` triples, taken VERBATIM (the
/// marker lines excluded). A block may name its file on its opening line
/// (`<<<<<<< SEARCH path/to/file`); then `file` is `Some`, otherwise `None`.
/// A block is fenced by N `<` (N ≥ 7; use 8 when the text holds conflict
/// markers): its divider is exactly N `=` and its close exactly N `>` + ` REPLACE`.
/// `Err` on a block that opens but does not close.
fn parse_hunks(text: &str) -> Result<Vec<(Option<String>, String, String)>, String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut hunks: Vec<(Option<String>, String, String)> = Vec::new();
    let n = lines.len();
    let mut i = 0;
    while i < n {
        let Some((width, file)) = fence_open(lines[i]) else {
            i += 1;
            continue;
        };
        let divider = "=".repeat(width);
        let close = format!("{} REPLACE", ">".repeat(width));
        let mut j = i + 1;
        let mut search: Vec<&str> = Vec::new();
        while j < n && lines[j] != divider.as_str() {
            search.push(lines[j]);
            j += 1;
        }
        if j >= n {
            return Err(format!("a SEARCH block has no `{divider}` divider"));
        }
        j += 1;
        let mut replace: Vec<&str> = Vec::new();
        while j < n && lines[j] != close.as_str() {
            if lines[j] == divider.as_str() {
                return Err(format!(
                    "a block has a second `{divider}` line; the text contains conflict markers: use the 8-character fence"
                ));
            }
            replace.push(lines[j]);
            j += 1;
        }
        if j >= n {
            return Err(format!("a block has no closing `{close}`"));
        }
        hunks.push((file, search.join("\n"), replace.join("\n")));
        i = j + 1;
    }
    Ok(hunks)
}

/// An opener: N `<` (N ≥ 7), then ` SEARCH`, then optionally ` <path>`.
/// Returns the fence width N and the named file, if any.
fn fence_open(line: &str) -> Option<(usize, Option<String>)> {
    let width = line.bytes().take_while(|b| *b == b'<').count();
    if width < 7 {
        return None;
    }
    let rest = line[width..].strip_prefix(" SEARCH")?;
    if rest.is_empty() {
        return Some((width, None));
    }
    let path = rest.strip_prefix(' ')?.trim();
    Some((width, (!path.is_empty()).then(|| path.to_string())))
}

/// python `str.count` (non-overlapping); an empty needle counts code points + 1,
/// so `hits != 1` still fails loud.
fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return haystack.chars().count() + 1;
    }
    haystack.matches(needle).count()
}

/// One file of an edit transaction.
struct Edit {
    path: String,
    hunks: Vec<(String, String)>,
    if_match: Option<Value>,
    before: Vec<u8>,
    new: Vec<u8>,
}

async fn edit(
    st: &TextState,
    bridge: &str,
    meta: &Map<String, Value>,
    root: &Result<PathBuf, String>,
    payload: &Value,
    blob: Option<&[u8]>,
    readonly: bool,
) -> Value {
    if readonly {
        return readonly_refusal();
    }
    let Some(edits) = payload
        .get("edits")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty())
    else {
        return rf::missing("edit", "edits (a list of {path, hunks, if_match?})");
    };
    let data = match doc(
        "edit",
        payload,
        blob,
        "the patch — all hunks, in edits order",
    ) {
        Ok(d) => d,
        Err(e) => return e,
    };
    if data.is_empty() {
        return rf::missing("edit", "bytes (the patch — all hunks, in edits order)");
    }
    let Ok(text) = std::str::from_utf8(&data) else {
        return rf::bad("edit", "bytes", "valid utf-8 (the patch is text)");
    };
    let hunks = match parse_hunks(text) {
        Ok(h) => h,
        Err(e) => return rf::bad("edit", "bytes", &format!("a well-formed patch — {e}")),
    };
    if hunks.is_empty() {
        return rf::bad(
            "edit",
            "bytes",
            "at least one `<<<<<<< SEARCH` / `=======` / `>>>>>>> REPLACE` block",
        );
    }
    let allow = allow_list(meta);
    // #206 — a block may name its file on its opening line. Either every block
    // names one or none does; named blocks are matched to `edits` by path, so the
    // per-edit `hunks` counts are not needed.
    let named = hunks.iter().any(|(f, _, _)| f.is_some());
    if named {
        if hunks.iter().any(|(f, _, _)| f.is_none()) {
            return rf::bad("edit", "bytes", "name the file on every block or on none");
        }
        let known: Vec<Option<String>> = edits
            .iter()
            .filter_map(|e| e.get("path").and_then(Value::as_str))
            .map(norm)
            .collect();
        for (file, _, _) in &hunks {
            let Some(file) = file else { continue };
            if !known.iter().any(|k| *k == norm(file)) {
                return rf::bad(
                    "edit",
                    "bytes",
                    &format!("a block names {file}, which is not in edits"),
                );
            }
        }
    }
    let mut plan: Vec<Edit> = Vec::new();
    let mut cursor = 0usize;
    for (i, e) in edits.iter().enumerate() {
        let Some(path) = e
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty())
        else {
            return rf::bad(
                "edit",
                "edits",
                &format!("edit {i}: an object with a `path`"),
            );
        };
        if plan.iter().any(|p| norm(&p.path) == norm(path)) {
            return rf::bad(
                "edit",
                "edits",
                "each path at most once — put all of a file's hunks in one edit",
            );
        }
        if let Some(gate) = gate_path("edit", root, &allow, path) {
            return gate;
        }
        if named {
            let want = norm(path);
            let mine: Vec<(String, String)> = hunks
                .iter()
                .filter(|(f, _, _)| f.as_deref().and_then(norm) == want)
                .map(|(_, s, r)| (s.clone(), r.clone()))
                .collect();
            if mine.is_empty() {
                return rf::bad(
                    "edit",
                    "edits",
                    &format!("edit {i} ({}): no block names it", py_repr(path)),
                );
            }
            if let Some(stated) = e.get("hunks").filter(|v| !v.is_null()) {
                if stated.as_u64() != Some(mine.len() as u64) {
                    return rf::bad(
                        "edit",
                        "edits",
                        &format!(
                            "edit {i} ({}): hunks says {stated}, the blob has {} \
                             block(s) naming it — leave hunks out",
                            py_repr(path),
                            mine.len()
                        ),
                    );
                }
            }
            cursor = cursor.saturating_add(mine.len());
            plan.push(Edit {
                path: path.to_string(),
                hunks: mine,
                if_match: e.get("if_match").filter(|v| !v.is_null()).cloned(),
                before: Vec::new(),
                new: Vec::new(),
            });
            continue;
        }
        // #206 — with exactly one file, `hunks` may be left out: every block of
        // the blob applies to it. With several files the split must be stated.
        let n = if edits.len() == 1 && e.get("hunks").filter(|v| !v.is_null()).is_none() {
            hunks.len()
        } else {
            let Some(n) = e.get("hunks").and_then(Value::as_u64).filter(|n| *n >= 1) else {
                return rf::bad(
                    "edit",
                    "edits",
                    &format!("edit {i} ({}): `hunks` (int >= 1)", py_repr(path)),
                );
            };
            usize::try_from(n).unwrap_or(usize::MAX)
        };
        // python slices `hunks[cursor:cursor + n]` — it CLAMPS and keeps going,
        // so every edit is validated and `cursor` holds the full claim.
        let lo = cursor.min(hunks.len());
        let hi = cursor.saturating_add(n).min(hunks.len());
        plan.push(Edit {
            path: path.to_string(),
            hunks: hunks[lo..hi]
                .iter()
                .map(|(_, s, r)| (s.clone(), r.clone()))
                .collect(),
            if_match: e.get("if_match").filter(|v| !v.is_null()).cloned(),
            before: Vec::new(),
            new: Vec::new(),
        });
        cursor = cursor.saturating_add(n);
    }
    if cursor != hunks.len() {
        let hint = if edits.len() == 1 {
            " — with one file, leave hunks out and every block applies"
        } else {
            ""
        };
        return rf::bad(
            "edit",
            "edits",
            &format!(
                "hunk counts that sum to the blob's blocks — edits claim {cursor}, \
                 the patch holds {}{hint}",
                hunks.len()
            ),
        );
    }
    let root = match clamped(root) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let keys: Vec<String> = plan.iter().map(|e| key_of(&e.path)).collect();
    let _held = st.hold(bridge, &keys).await;
    edit_txn(
        st,
        bridge,
        root,
        plan,
        py_truthy(payload.get("dry_run").unwrap_or(&Value::Null)),
    )
}

/// The locked half of `edit`: verify every hunk against the CURRENT document
/// (`if_match` checked here, under the lock), then commit all; a mid-commit
/// failure restores every document already written from its pre-image.
fn edit_txn(
    st: &TextState,
    bridge: &str,
    root: &Path,
    mut plan: Vec<Edit>,
    dry_run: bool,
) -> Value {
    for e in &mut plan {
        let raw = match current(root, &e.path) {
            Ok(Some(r)) => r,
            Ok(None) => return not_found(root, &e.path),
            Err(err) => return transport(err),
        };
        if let Some(want) = &e.if_match {
            if let Some(err) = mismatch(&e.path, want, Some(&raw)) {
                return err;
            }
        }
        let Ok(mut working) = String::from_utf8(raw.clone()) else {
            return rf::bad(
                "edit",
                "edits",
                &format!("{}: a utf-8 text file", py_repr(&e.path)),
            );
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
            working = working.replacen(search.as_str(), replace, 1);
        }
        e.before = raw;
        e.new = working.into_bytes();
    }
    if dry_run {
        return json!({
            "dry_run": true,
            "edits": plan
                .iter()
                .map(|e| json!({"path": e.path, "hunks": e.hunks.len()}))
                .collect::<Vec<_>>(),
        });
    }
    let mut written: Vec<usize> = Vec::new();
    for (i, e) in plan.iter().enumerate() {
        if let Err(err) = fs::write_bytes(root, &e.path, &e.new, true, None) {
            // ROLL BACK what already landed — every pre-image is in hand.
            for &w in &written {
                let _ = fs::write_bytes(root, &plan[w].path, &plan[w].before, true, None);
            }
            let mut out = transport(err);
            rf::with(
                &mut out,
                "restored",
                json!(written
                    .iter()
                    .map(|&w| plan[w].path.clone())
                    .collect::<Vec<_>>()),
            );
            rf::with(&mut out, "failed_path", json!(e.path));
            return out;
        }
        written.push(i);
    }
    {
        let mut slots = st.slots();
        for e in &plan {
            slots.insert(
                (bridge.to_string(), key_of(&e.path)),
                Some(e.before.clone()),
            );
        }
    }
    json!({
        "edited": true,
        "edits": plan
            .iter()
            .map(|e| json!({
                "path": e.path,
                "hunks": e.hunks.len(),
                "size": e.new.len(),
                "version": version_of(&e.new),
            }))
            .collect::<Vec<_>>(),
    })
}

async fn undo(
    st: &TextState,
    bridge: &str,
    meta: &Map<String, Value>,
    root: &Result<PathBuf, String>,
    payload: &Value,
    readonly: bool,
) -> Value {
    let path = match path_arg("undo", payload, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let root = match text_gate("undo", root, meta, path, true, readonly) {
        Ok(r) => r,
        Err(e) => return e,
    };
    let key = key_of(path);
    let _held = st.hold(bridge, std::slice::from_ref(&key)).await;
    let slot_key = (bridge.to_string(), key);
    let Some(slot) = st.slots().get(&slot_key).cloned() else {
        let mut v = rf::refusal_hint(
            rf::NOT_FOUND,
            format!("undo: nothing to undo for {}", py_repr(path)),
            "the slot holds what the LAST write/edit/append on this path replaced; it \
             lives in RAM, one per path, and is gone after a restart",
        );
        rf::with(&mut v, "path", json!(path));
        return v;
    };
    let now = match current(root, path) {
        Ok(n) => n,
        Err(e) => return transport(e),
    };
    let done = match &slot {
        None => match now {
            Some(_) => fs::remove(root, path, false),
            None => Ok(()),
        },
        Some(bytes) => fs::write_bytes(root, path, bytes, true, None),
    };
    if let Err(e) = done {
        return transport(e);
    }
    st.slots().insert(slot_key, now);
    match slot {
        None => json!({"undone": true, "path": path, "deleted": true}),
        Some(bytes) => json!({"undone": true, "path": path, "version": version_of(&bytes)}),
    }
}
