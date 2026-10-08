"""file_bridge bundle — the fs-transport member of the io family.

The disk is OUTSIDE the trust domain: crossing kernel↔disk is an IO edge,
gated exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule`
⇒ every verb except `reflect` denies with the teaching shape). Open it
consciously with `ingress_rule=allow_all_ingress`. Even open, the LAW is
running-dir only: the `root` AND every path are clamped inside the kernel's
running dir (`Kernel.workdir`; `../`, `~`, absolute escapes, outward symlinks all
refuse).

Each file_bridge owns a `root` directory and a `readonly` flag. Path safety:
every verb resolves `root/path` and refuses if the result escapes `root`.

Verbs:
  reflect            -> {sentence, root, readonly, allow_count, edit_grammar, compose, ingress_rule, egress_rule, sealed, …}
  list   path?       -> {files: [{name, path, type, size?}, ...]}
  read   path offset? limit?   -> {lines:[{n,text}], total, eof, version}   (text)
  write  path bytes if_match?  -> {path, size, version}   (atomic, whole document)
  edit   edits bytes dry_run?  -> {edited, edits:[…]}     (SEARCH/REPLACE transaction)
  append path bytes            -> {path, size, version}
  undo   path                  -> {undone, path, version|deleted}   (one RAM slot)
  read_stream  path offset? length?        -> {bytes, next_offset, eof, size} (SOURCE)
  write_stream path bytes offset? truncate? -> {written, offset, size}        (SINK)
  pump   source? source_path sink? sink_path? -> {bytes, chunks}  (server-side copy)
  delete path        -> {path, deleted: true}            (refused if readonly)
  rename old_path new_path -> {old_path, new_path}        (refused if readonly)
  mkdir  path        -> {path, created: true}            (refused if readonly)

THE TEXT VERBS are this bridge's own: a caller edits with ITS OWN grants on the
bridge. `write`/`edit` commit through `fs.write_bytes` (a tmp sibling + atomic
replace); the tmp is judged by the TARGET path's exposure, never on its own. `version` (a content hash) rides
every reply and `if_match` guards any write. UNDO is ONE slot per path in RAM:
each `write`/`edit`/`append` keeps the bytes it replaced; `undo` swaps them back
(a second `undo` is redo); nothing on disk, lost on restart.

DENY-ALL AT THE PATH LEVEL. The `allow` list on the record names what this bridge
EXPOSES, relative to `root`: a listed file, or everything under a listed
directory (`.` exposes the whole root). No list, or an empty one, exposes
NOTHING. Every path verb (`list`, `read_stream`, `write_stream`, `delete`,
`rename`, `mkdir`, and `pump` through them) sees only exposed paths; anything
else is `not_found` whether or not it exists, so nothing leaks. `list` also shows
the directories on the way to an exposed path. The list is record state — set at
`create_agent` or by `update_agent`, persisted by the loader. Writes are still
governed by `readonly`.
"""

from __future__ import annotations

import asyncio
import hashlib
import posixpath
from contextlib import asynccontextmanager

from file_bridge import fs
from io_bridge import describe as _describe
from io_bridge import gate_inbound, resolve_ingress, unknown_ingress_refusal
from io_bridge._base import parse_spec
from kernel._refusal import (
    NOT_FOUND,
    TRANSPORT_ERROR,
    UNAUTHORIZED,
    UNHANDLED,
    bad,
    missing,
    refusal,
)

IMAGE_EXT = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"}

# The disk clamp + raw IO live in `fs` — the ONE disk surface. This agent is the
# GATE (ingress rule) + verb shape over it; it never touches the disk directly.


def _root_str(rec: dict) -> str:
    """The bridge's configured root string."""
    return rec.get("root", "") or ""


def _root(kernel, rec: dict) -> fs.Root:
    """The clamp boundary `fs` enforces: the configured root under the kernel's
    running dir (`Kernel.workdir`, the one source)."""
    return fs.Root(kernel.ctx.workdir, _root_str(rec))


# ─── argument validation ────────────────────────────────────────
# Every refusal below is `bad`/`missing` — the one shape, branchable on `reason`.
# These used to be `int(...)` coercions OUTSIDE the try (a non-numeric string raised
# out of the handler; a negative `length` read 0 bytes and `pump` spun forever), and
# an empty `path` on delete/rename/mkdir resolved to the ROOT itself — `delete
# {path:"", recursive:true}` removed the whole served store.


def _norm(path: str) -> str | None:
    """A root-relative path, normalised (`posixpath.normpath`); None when it is
    absolute or climbs out. The empty path is the root, `.`."""
    if path.startswith("/"):
        return None
    p = posixpath.normpath(path or ".")
    if p == ".." or p.startswith("../"):
        return None
    return p


def _allow(rec: dict) -> list[str]:
    """The `allow` list, normalised. Absent or empty ⇒ nothing is exposed."""
    raw = rec.get("allow")
    out = []
    for s in raw if isinstance(raw, list) else []:
        n = _norm(s) if isinstance(s, str) else None
        if n is not None:
            out.append(n)
    return out


def _exposed(allow: list[str], path: str) -> bool:
    """Is `path` exposed — a listed path, or under a listed directory?"""
    p = _norm(path)
    if p is None:
        return False
    return any(s == "." or p == s or p.startswith(s + "/") for s in allow)


def _leads_to(allow: list[str], path: str) -> bool:
    """Is `path` a directory on the way to an exposed path? (`list` shows it.)"""
    p = _norm(path)
    if p is None or not allow:
        return False
    return p == "." or any(s.startswith(p + "/") for s in allow)


def _lands_exposed(root: fs.Root, allow: list[str], path: str) -> bool:
    """Where `path` actually LANDS (symlinks followed) must be under the RESOLVED
    form of an allow entry — entries resolved now, because the disk changes. With
    `_exposed` (the path as asked) this is the both-ends rule: a link inside an
    exposed area that points outside it serves nothing (audit 2026-09-28)."""
    try:
        target = fs.resolve(root, path)
    except ValueError:
        return False
    for s in allow:
        try:
            entry = fs.resolve(root, s)
        except ValueError:
            continue  # an entry that itself leads out of the root exposes nothing
        if target == entry or entry in target.parents:
            return True
    return False


def _gate_path(verb: str, root: fs.Root, allow: list[str], path: str) -> dict | None:
    """THE CLAMP FIRST, then the list — on BOTH ends: a root or a path that leaves
    the running dir answers the clamp's own refusal; an in-root path is exposed only
    when the path as asked AND the file it lands on are both inside the list, else
    `_unexposed`."""
    try:
        fs.resolve(root, path)
    except ValueError as e:
        return refusal(TRANSPORT_ERROR, str(e))
    if not (_exposed(allow, path) and _lands_exposed(root, allow, path)):
        return _unexposed(verb, path)
    return None


def _unexposed(verb: str, path: str) -> dict:
    """The ONE answer for a path outside the `allow` list — the same whether it
    exists or not, so a caller learns nothing about what lies beyond the list."""
    return refusal(
        NOT_FOUND,
        f"{verb}: {path!r} is not exposed by this bridge",
        hint="expose it first — add it to the bridge's `allow` record field "
        "(update_agent)",
        path=path,
    )


def _int_arg(verb: str, payload: dict, name: str, default: int, minimum: int):
    """`(value, None)` or `(None, refusal)`. Absent/None ⇒ default. A bool is NOT
    an int here (JSON `true` must not read as offset 1)."""
    v = payload.get(name)
    if v is None:
        return default, None
    if isinstance(v, bool) or not isinstance(v, int) or v < minimum:
        return None, bad(verb, name, f"an integer >= {minimum}")
    return v, None


def _path_arg(verb: str, payload: dict, name: str = "path"):
    """`(path, None)` or `(None, refusal)`: a string, root-relative (no leading
    '/'). The absolute form used to be silently re-rooted (`lstrip("/")`), which
    contradicted the clamp's own promise; the ports always refused it."""
    p = payload.get(name, "")
    if p is None:
        p = ""
    if not isinstance(p, str):
        return None, bad(verb, name, "a string")
    if p.startswith("/"):
        return None, bad(verb, name, "a root-relative path (no leading '/')")
    return p, None


def _target_arg(verb: str, root: fs.Root, payload: dict, name: str = "path"):
    """`_path_arg` for a MUTATING verb: the path must name something INSIDE the
    root — empty/`.` is `missing`, anything resolving to the root itself is
    `bad`. The root is the served boundary, not an operand."""
    p, err = _path_arg(verb, payload, name)
    if err:
        return None, err
    if not p or p == ".":
        return None, missing(verb, name)
    try:
        if fs.resolve(root, p) == fs.resolve_root(root):
            return None, bad(verb, name, "a path inside the root, not the root itself")
    except ValueError as e:
        return None, refusal(TRANSPORT_ERROR, str(e))
    return p, None


# One `read` used to return a whole file, however large. MEASURED 2026-08-07: an AI agent
# read an 86 KB source file, its conversation went to 21k tokens, and the NEXT message was
# cancelled mid-prefill because prefill at ~95 tok/s outran the backend's 180s send
# timeout. Nothing caught it — the context window (32k) was never exceeded, so the
# compaction handshake never fired and `context_insufficient` never appeared. The message
# became un-completable on TIME while staying legal on SIZE, and no guard covers that.
#
# So a text read is WINDOWED. Not silently: an unread tail is reported with the totals and
# the offset to continue from, because a caller that cannot tell a whole file from its
# first page will reason about the wrong content.
DEFAULT_MAX_READ_CHARS = 16384


def _readonly_or_none(rec: dict) -> dict | None:
    """A readonly leg refuses every mutating verb — in THE ONE REFUSAL SHAPE.

    This answered a bare `{"error": "agent is readonly"}` until 2026-08-27, when
    the shared agentic corpus caught it: no `reason`, so a caller could only
    branch on the prose. `unauthorized` is the same code the io SEAL answers with
    — both say "this edge is not open for that", and neither is about the caller,
    which is why neither carries a `side`."""
    if rec.get("readonly"):
        return refusal(
            UNAUTHORIZED,
            "agent is readonly",
            hint="this leg accepts reads only; an operator may clear `readonly`",
        )
    return None


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + filesystem root + readonly flag + auth posture (ingress/egress/sealed/see). No args."""
    rec = kernel.get(id) or {}
    try:
        root_display = str(fs.resolve_root(_root(kernel, rec)))
        root_error = None
    except ValueError as e:
        # Reflect NEVER raises — show the configured string + the violation.
        root_display = _root_str(rec)
        root_error = str(e)
    out = {
        "id": id,
        "sentence": "Filesystem edge of the io family — sealed by default, running-dir only.",
        "root": root_display,
        "readonly": bool(rec.get("readonly")),
        "allow_count": len(_allow(rec)),
        "edit_grammar": (
            "one or more blocks: `<<<<<<< SEARCH` / <old lines> / `=======` / "
            "<new lines> / `>>>>>>> REPLACE`; each SEARCH must match its "
            "document EXACTLY ONCE. `edit` is a TRANSACTION over "
            "`edits:[{path, hunks, if_match?}]` — even one file rides the "
            "array; the blob holds every edit's hunks concatenated in `edits` "
            "order, split by the per-edit `hunks` count; any failure anywhere "
            "writes nothing"
        ),
        "compose": (
            "every content-carrying verb (`write`, `edit`, `append`) takes its "
            "document in `bytes`, and `bytes` is VERBATIM: write the document "
            "between `<bytes>` and `</bytes>` exactly as it should land — real "
            "newlines, real quotes, nothing escaped, no fence — then close the "
            "call. The document ends at the first `</bytes>`. An edit names its "
            "files as tags: <edits><item><path>a.py</path><hunks>2</hunks></item></edits>"
        ),
        **_describe(rec),
    }
    if root_error:
        out["root_error"] = root_error
    return out


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
    return None


async def _list(id, payload, kernel):
    """args: path:str (default ''). Returns {path, files:[{name, path, type, size?}, ...]} — the exposed entries and the directories on the way to one."""
    rec = kernel.get(id) or {}
    root = _root(kernel, rec)
    path, err = _path_arg("list", payload)
    if err:
        return err
    allow = _allow(rec)
    if not _leads_to(allow, path) and (err := _gate_path("list", root, allow, path)):
        return err
    try:
        if not fs.exists(root, path):
            return refusal(NOT_FOUND, f"path {path!r} does not exist", path=path)
        if not fs.is_dir(root, path):
            return bad("list", "path", f"a directory; {path!r} is not one")
        entries = fs.list_dir(root, path)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    files = []
    for entry in entries:
        # the entry's path AS A CALLER NAMES IT (under the listed dir, links not
        # followed), judged by the same both-ends rule as every other verb
        rel = posixpath.normpath(posixpath.join(_norm(path) or ".", entry.name))
        shown = _exposed(allow, rel) and _lands_exposed(root, allow, rel)
        if not (shown or (entry.is_dir() and _leads_to(allow, rel))):
            continue
        if entry.is_dir():
            files.append({"name": entry.name, "path": rel, "type": "dir"})
        else:
            try:
                sz = entry.stat().st_size
            except OSError:
                sz = -1
            files.append({"name": entry.name, "path": rel, "type": "file", "size": sz})
    return {
        "path": path,
        "files": files,
        "origin": f"{id} list {path} — list {{path: {path}}}; changes as files change",
    }


async def _delete(id, payload, kernel):
    """args: path:str (req), recursive:bool=False. Returns {path, deleted:true}.
    A bare delete removes a file or an EMPTY dir; `recursive:true` removes a dir
    and everything under it (still clamped to root). Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    root = _root(kernel, rec)
    path, err = _target_arg("delete", root, payload)
    if err:
        return err
    if err := _gate_path("delete", root, _allow(rec), path):
        return err
    try:
        # `exists` follows symlinks — a DANGLING link is still a deletable name.
        if not fs.exists(root, path) and not fs.is_symlink(root, path):
            return refusal(NOT_FOUND, f"path {path!r} does not exist", path=path)
        fs.remove(root, path, recursive=bool(payload.get("recursive")))
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"path": path, "deleted": True}


async def _rename(id, payload, kernel):
    """args: old_path:str, new_path:str (both req). Returns {old_path, new_path}. Creates new parent dirs. Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    root = _root(kernel, rec)
    old, err = _target_arg("rename", root, payload, "old_path")
    if err:
        return err
    new, err = _target_arg("rename", root, payload, "new_path")
    if err:
        return err
    for p in (old, new):
        if err := _gate_path("rename", root, _allow(rec), p):
            return err
    try:
        if not fs.exists(root, old):
            return refusal(NOT_FOUND, f"path {old!r} does not exist", path=old)
        fs.rename(root, old, new)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"old_path": old, "new_path": new}


async def _mkdir(id, payload, kernel):
    """args: path:str (req). Returns {path, created:true}. Recursive (parents ok). Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    path, err = _target_arg("mkdir", _root(kernel, rec), payload)
    if err:
        return err
    if err := _gate_path("mkdir", _root(kernel, rec), _allow(rec), path):
        return err
    try:
        fs.mkdir(_root(kernel, rec), path)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"path": path, "created": True}


# ─── the stream protocol (SOURCE / SINK) ────────────────────────
# A stateless cursor stream: any consumer (file serving, a pump, kernel_state)
# pulls/pushes bytes by id + verb — weak-coupled, no session handle. file_bridge is
# the fs IMPLEMENTOR of this protocol; a network_bridge implements the same verbs over
# the wire, so a consumer is storage-agnostic (G1: the provider is just an id).


async def _read_stream(id, payload, kernel):
    """args: path:str (req), offset:int=0, length:int=65536. The SOURCE half — returns ONE chunk {path, offset, bytes, next_offset, eof, size}.
    Pull the next by calling
    again with offset=next_offset until eof. `bytes` is the RAW chunk — it rides every
    transport as raw bytes (web_ws / bridge binary frame, in-proc dict), NEVER base64.
    Stateless cursor — no open handle, weak-coupled by id."""
    rec = kernel.get(id) or {}
    root = _root(kernel, rec)
    path, err = _path_arg("read_stream", payload)
    if err:
        return err
    offset, err = _int_arg("read_stream", payload, "offset", 0, 0)
    if err:
        return err
    length, err = _int_arg("read_stream", payload, "length", 65536, 1)
    if err:
        return err
    if err := _gate_path("read_stream", root, _allow(rec), path):
        return err
    try:
        if not fs.is_file(root, path):
            return refusal(NOT_FOUND, f"file {path!r} not found", path=path)
        chunk, size = fs.read_chunk(root, path, offset, length)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    next_offset = offset + len(chunk)
    return {
        "path": path,
        "offset": offset,
        "bytes": chunk,
        "next_offset": next_offset,
        "eof": next_offset >= size,
        "size": size,
        "origin": f"{id} {path} bytes {offset}-{next_offset} of {size} — "
        f"read_stream {{path: {path}, offset: {offset}, length: {len(chunk)}}}",
    }


async def _write_stream(id, payload, kernel):
    """args: path:str (req), bytes:bytes (req — ONE raw chunk; a model writes it VERBATIM between `<bytes>` and `</bytes>`, nothing escaped), offset:int? (default = append at end), truncate:bool=False. The SINK half — a producer pushes RAW chunks by offset.
    A producer pushes RAW chunks by
    calling repeatedly. Pass truncate:true on the first chunk to start fresh. Returns
    {path, written, offset, size}. Refused if readonly. The chunk is raw bytes — a
    text document goes through `write`/`edit` (atomic, versioned, undoable)."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    root = _root(kernel, rec)
    path, err = _path_arg("write_stream", payload)
    if err:
        return err
    if err := _gate_path("write_stream", root, _allow(rec), path):
        return err
    data = payload.get("bytes", b"")
    if not isinstance(data, (bytes, bytearray)):
        return missing("write_stream", "bytes (one raw chunk)")
    data = bytes(data)
    off_in, err = _int_arg("write_stream", payload, "offset", -1, 0)
    if err:
        return err
    try:
        off, size = fs.write_chunk(
            root,
            path,
            data,
            None if off_in < 0 else off_in,
            truncate=bool(payload.get("truncate")),
        )
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"path": path, "written": len(data), "offset": off, "size": size}


def _one_file(a, b) -> bool:
    """Do two pump paths name ONE file — equal once normalised (`./x` is `x`)? Only
    a root-relative path to something inside the root is judged; anything else
    (absent, not a string, absolute, climbing out, the root itself) is its end's
    to refuse."""
    if not (isinstance(a, str) and isinstance(b, str)):
        return False
    n = _norm(a)
    return n not in (None, ".") and n == _norm(b)


async def _pump(id, payload, kernel):
    """The PUMP — a server-side stream copy SOURCE→SINK, chunk by chunk, in ONE call.
    (vs a consumer driving each read/write over the wire). Storage-agnostic:
    both ends are bound BY ID + the duck-typed stream verbs, so a `network_bridge`
    SOURCE pumps to a `file_bridge` SINK the same as fs→fs.

    args: source:str? (provider id, default self) + source_path:str (req) + sink:str?
    (provider id, default self) + sink_path:str? (default = source_path) +
    chunk:int=65536. BOTH ENDS RUN AS THIS BRIDGE: `read_stream` on the source and
    `write_stream` on the sink are this bridge's own sends, so the bridge needs the
    grants on both — a caller holding `pump` on it borrows exactly that reach, which
    is what granting `pump` means. Each end SELF-gates + SELF-clamps
    (the pump only coordinates — it never reads/writes bytes itself, so a sealed end
    refuses it). A pump ONTO ITS OWN SOURCE (the sink is the source and sink_path
    names the same file) is refused. Returns {source, sink, bytes, chunks}."""
    src = payload.get("source") or id
    sink = payload.get("sink") or id
    spath = payload.get("source_path") or ""
    dpath = payload.get("sink_path") or spath
    length, err = _int_arg("pump", payload, "chunk", 65536, 1)
    if err:
        return err
    if src == sink and _one_file(spath, dpath):
        # The first write's `truncate` empties the file the next read comes from:
        # everything past the first chunk was lost under a success reply — the
        # bare `{source_path}` form did it, sink and sink_path defaulted (audit
        # BV-7).
        return bad(
            "pump",
            "sink_path",
            "another file than source_path when the sink is the source — a pump "
            "onto its own source truncates it after the first chunk",
        )
    offset, first, chunks = 0, True, 0

    async def own_send(item: dict):
        # Sent from this dispatch: identity comes only from dispatch, so both ends
        # are THIS BRIDGE's sends, gated as it.
        return (await kernel.send([item]))[0]["result"]

    while True:
        r = await own_send(
            {
                "target": src,
                "payload": {
                    "type": "read_stream",
                    "path": spath,
                    "offset": offset,
                    "length": length,
                },
            }
        )
        if not isinstance(r, dict) or not isinstance(
            r.get("bytes"), (bytes, bytearray)
        ):
            return refusal(TRANSPORT_ERROR, f"pump: read from {src!r} failed: {r}")
        w = await own_send(
            {
                "target": sink,
                "payload": {
                    "type": "write_stream",
                    "path": dpath,
                    "bytes": r["bytes"],
                    "offset": offset,
                    "truncate": first,
                },
            }
        )
        if not isinstance(w, dict) or w.get("error"):
            return refusal(TRANSPORT_ERROR, f"pump: write to {sink!r} failed: {w}")
        first, chunks = False, chunks + 1
        next_offset = r.get("next_offset")
        if r.get("eof"):
            offset = next_offset if isinstance(next_offset, int) else offset
            break
        if not isinstance(next_offset, int) or next_offset <= offset:
            # No forward progress and no eof: a SOURCE that would loop forever.
            return refusal(
                TRANSPORT_ERROR,
                f"pump: source {src!r} made no progress at offset {offset}",
            )
        offset = next_offset
    return {"source": spath, "sink": dpath, "bytes": offset, "chunks": chunks}


# ─── the text verbs (the editing organ) ─────────────────────────
# A text document is edited HERE, with the caller's OWN grants on this bridge —
# no deputy sends to a bridge as itself. Every byte goes through `fs` under this
# bridge's clamp, `allow` list and `readonly`; a commit is `fs.write_bytes`
# (tmp sibling + atomic replace), the tmp judged by the TARGET path's exposure.

#: lines per `read` page when the caller names no `limit`.
READ_LIMIT = 200
_H_START, _H_DIV, _H_END = "<<<<<<< SEARCH", "=======", ">>>>>>> REPLACE"

#: ONE undo slot per (bridge, path): the bytes the last `write`/`edit`/`append`
#: replaced (None = the file did not exist). `undo` swaps file ↔ slot, so a
#: second `undo` is redo. RAM only — nothing on disk, lost on restart.
#: TODO: memory leak possible — one slot per edited path, unbounded paths × size.
_UNDO: dict[tuple[str, str], bytes | None] = {}


# ─── per-document serialization ─────────────────────────────────
# A mutate is read → verify → commit; two concurrent mutates on ONE document
# interleaved there would both derive from the same pre-image and the second
# commit would silently discard the first. One lock per (bridge, path), held
# from the first read through the commit; `if_match` is checked UNDER it. A
# multi-file edit takes its locks in sorted order so overlapping transactions
# cannot deadlock. Entries are ref-counted and dropped at zero.
_LOCKS: dict[tuple[str, str], list] = {}


@asynccontextmanager
async def _held(bridge: str, *paths: str):
    table = _LOCKS
    keys = sorted({(bridge, p) for p in paths})
    entries: list[list] = []
    for k in keys:
        e = table.get(k)
        if e is None:
            e = table[k] = [asyncio.Lock(), 0]
        e[1] += 1
        entries.append(e)
    acquired: list[list] = []
    try:
        for e in entries:
            await e[0].acquire()
            acquired.append(e)
        yield
    finally:
        for e in reversed(acquired):
            e[0].release()
        for k, e in zip(keys, entries):
            e[1] -= 1
            if e[1] == 0:
                table.pop(k, None)


def _version(data: bytes) -> str:
    """The on-demand version: a content hash, computed, never stored."""
    return hashlib.sha256(data).hexdigest()[:16]


def _doc(verb: str, payload: dict, what: str):
    """`(bytes, None)` or `(None, refusal)`. A str is utf-8-encoded: a spawned
    program's ctrl-fd wire is JSON (no bytes type), and every caller gets the same
    treatment."""
    data = payload.get("bytes")
    if isinstance(data, str):
        data = data.encode("utf-8")
    if not isinstance(data, (bytes, bytearray)):
        return None, missing(verb, f"bytes ({what} — str is utf-8-encoded)")
    return bytes(data), None


def _current(root: fs.Root, path: str) -> bytes | None:
    """The document's bytes, or None when no file is there."""
    return fs.read_bytes(root, path) if fs.is_file(root, path) else None


def _mismatch(path: str, want: str, have_bytes: bytes | None) -> dict | None:
    """The on-demand optimistic guard: a refusal when `if_match` names another
    version (an absent document has no version and never matches)."""
    have = None if have_bytes is None else _version(have_bytes)
    if have == want:
        return None
    return {
        "error": "version mismatch — the document changed since you read it",
        "reason": "version_mismatch",
        "path": path,
        "expected": want,
        "actual": have,
    }


def _text_gate(verb: str, kernel, rec: dict, path: str, mutate: bool):
    """`(root, None)` or `(None, refusal)`: readonly for a mutate, a real path,
    the clamp and the allow list on both ends."""
    if mutate and (err := _readonly_or_none(rec)):
        return None, err
    if not path:
        return None, missing(verb, "path")
    root = _root(kernel, rec)
    if err := _gate_path(verb, root, _allow(rec), path):
        return None, err
    return root, None


async def _read(id, payload, kernel):
    """args: path:str (req), offset:int=0 (line), limit:int=200 — a text page {path, lines:[{n,text}], total, eof, version, origin}; refuses binary."""
    rec = kernel.get(id) or {}
    path, err = _path_arg("read", payload)
    if err:
        return err
    off, err = _int_arg("read", payload, "offset", 0, 0)
    if err:
        return err
    limit, err = _int_arg("read", payload, "limit", READ_LIMIT, 1)
    if err:
        return err
    root, err = _text_gate("read", kernel, rec, path, mutate=False)
    if err:
        return err
    try:
        data = _current(root, path)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    if data is None:
        return refusal(NOT_FOUND, f"file {path!r} not found", path=path)
    if b"\x00" in data:
        return bad("read", "path", "a text file (this one is binary)")
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return bad("read", "path", "valid utf-8 (this file is not)")
    all_lines = text.split("\n")
    # A trailing newline yields one phantom empty tail element — dropped so
    # `total` counts real lines.
    if all_lines and all_lines[-1] == "":
        all_lines.pop()
    page = all_lines[off : off + limit]
    ver = _version(data)
    return {
        "path": path,
        "lines": [{"n": off + i + 1, "text": t} for i, t in enumerate(page)],
        "total": len(all_lines),
        "eof": off + limit >= len(all_lines),
        "version": ver,
        # HOW TO GET THIS BODY AGAIN — one line; a reader that keeps no body keeps this.
        "origin": f"{id} {path} lines {off + 1}-{off + len(page)} version {ver} — "
        f"read {{path: {path}, offset: {off}, limit: {limit}}}; a different version "
        "means the file changed since",
    }


async def _write(id, payload, kernel):
    """args: path:str (req), bytes (req — the whole document; str is utf-8-encoded), if_match:str? — atomic whole-document write; {path, size, version}."""
    rec = kernel.get(id) or {}
    path, err = _path_arg("write", payload)
    if err:
        return err
    root, err = _text_gate("write", kernel, rec, path, mutate=True)
    if err:
        return err
    data, err = _doc("write", payload, "the whole document")
    if err:
        return err
    async with _held(id, _norm(path)):
        try:
            before = _current(root, path)
            want = payload.get("if_match")
            if want is not None and (err := _mismatch(path, want, before)):
                return err
            fs.write_bytes(root, path, data)
        except (ValueError, OSError) as e:
            return refusal(TRANSPORT_ERROR, str(e))
        _UNDO[(id, _norm(path))] = before
    return {"path": path, "size": len(data), "version": _version(data)}


async def _append(id, payload, kernel):
    """args: path:str (req), bytes (req — one chunk; str is utf-8-encoded) — appends (creates if absent); {path, size, version}."""
    rec = kernel.get(id) or {}
    path, err = _path_arg("append", payload)
    if err:
        return err
    root, err = _text_gate("append", kernel, rec, path, mutate=True)
    if err:
        return err
    data, err = _doc("append", payload, "one chunk")
    if err:
        return err
    async with _held(id, _norm(path)):
        try:
            before = _current(root, path)
            _, size = fs.write_chunk(root, path, data, None)
            whole = fs.read_bytes(root, path)
        except (ValueError, OSError) as e:
            return refusal(TRANSPORT_ERROR, str(e))
        _UNDO[(id, _norm(path))] = before
    return {"path": path, "size": size, "version": _version(whole)}


def _parse_hunks(text: str) -> list[tuple[str, str]]:
    """SEARCH/REPLACE blocks → (search, replace) pairs, taken VERBATIM (the marker
    lines excluded). Raises ValueError on a block that opens but does not close."""
    lines = text.split("\n")
    hunks: list[tuple[str, str]] = []
    i, n = 0, len(lines)
    while i < n:
        if lines[i] != _H_START:
            i += 1
            continue
        j = i + 1
        search: list[str] = []
        while j < n and lines[j] != _H_DIV:
            search.append(lines[j])
            j += 1
        if j >= n:
            raise ValueError("a SEARCH block has no `=======` divider")
        j += 1
        replace: list[str] = []
        while j < n and lines[j] != _H_END:
            replace.append(lines[j])
            j += 1
        if j >= n:
            raise ValueError("a block has no closing `>>>>>>> REPLACE`")
        hunks.append(("\n".join(search), "\n".join(replace)))
        i = j + 1
    return hunks


async def _edit(id, payload, kernel):
    """args: edits:[{path:str, hunks:int, if_match:str?}] (req — even for ONE file), bytes (req — ALL hunks concatenated; str is utf-8-encoded), dry_run:bool? — a TRANSACTION: every hunk of every edit verified before any write, any failure writes nothing (or restores what was written); {edited:true, edits:[{path,hunks,size,version}]}."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    edits = payload.get("edits")
    if not isinstance(edits, list) or not edits:
        return missing("edit", "edits (a list of {path, hunks, if_match?})")
    data, err = _doc("edit", payload, "the patch — all hunks, in edits order")
    if err:
        return err
    if not data:
        return missing("edit", "bytes (the patch — all hunks, in edits order)")
    try:
        hunks = _parse_hunks(data.decode("utf-8"))
    except UnicodeDecodeError:
        return bad("edit", "bytes", "valid utf-8 (the patch is text)")
    except ValueError as e:
        return bad("edit", "bytes", f"a well-formed patch — {e}")
    if not hunks:
        return bad(
            "edit",
            "bytes",
            "at least one `<<<<<<< SEARCH` / `=======` / `>>>>>>> REPLACE` block",
        )
    root = _root(kernel, rec)
    allow = _allow(rec)
    plan: list[dict] = []
    cursor = 0
    for i, e in enumerate(edits):
        if (
            not isinstance(e, dict)
            or not isinstance(e.get("path"), str)
            or not e["path"]
        ):
            return bad("edit", "edits", f"edit {i}: an object with a `path`")
        n = e.get("hunks")
        if not isinstance(n, int) or isinstance(n, bool) or n < 1:
            return bad("edit", "edits", f"edit {i} ({e['path']!r}): `hunks` (int >= 1)")
        if any(_norm(p["path"]) == _norm(e["path"]) for p in plan):
            return bad(
                "edit",
                "edits",
                "each path at most once — put all of a file's hunks in one edit",
            )
        if gate := _gate_path("edit", root, allow, e["path"]):
            return gate
        plan.append(
            {
                "path": e["path"],
                "hunks": hunks[cursor : cursor + n],
                "if_match": e.get("if_match"),
            }
        )
        cursor += n
    if cursor != len(hunks):
        return bad(
            "edit",
            "edits",
            f"hunk counts that sum to the blob's blocks — edits claim {cursor}, "
            f"the patch holds {len(hunks)}",
        )
    async with _held(id, *(_norm(e["path"]) for e in plan)):
        return _edit_txn(id, root, plan, bool(payload.get("dry_run")), _UNDO)


def _edit_txn(id: str, root: fs.Root, plan: list[dict], dry_run: bool, slots: dict):
    """The locked half of `edit`: verify every hunk against the CURRENT document
    (`if_match` checked here, under the lock), then commit all; a mid-commit
    failure restores every document already written from its pre-image."""
    try:
        for e in plan:
            raw = _current(root, e["path"])
            if raw is None:
                return refusal(
                    NOT_FOUND, f"file {e['path']!r} not found", path=e["path"]
                )
            if e["if_match"] is not None and (
                err := _mismatch(e["path"], e["if_match"], raw)
            ):
                return err
            try:
                working = raw.decode("utf-8")
            except UnicodeDecodeError:
                return bad("edit", "edits", f"{e['path']!r}: a utf-8 text file")
            for idx, (search, replace) in enumerate(e["hunks"]):
                hits = working.count(search)
                if hits != 1:
                    return {
                        "error": "a hunk did not apply — nothing written",
                        "reason": "no_match" if hits == 0 else "ambiguous",
                        "failed": [
                            {
                                "path": e["path"],
                                "index": idx,
                                "reason": "search text not found"
                                if hits == 0
                                else f"search text matches {hits} places",
                            }
                        ],
                    }
                working = working.replace(search, replace, 1)
            e["before"] = raw
            e["new"] = working.encode("utf-8")
    except (ValueError, OSError) as err:
        return refusal(TRANSPORT_ERROR, str(err))
    if dry_run:
        return {
            "dry_run": True,
            "edits": [{"path": e["path"], "hunks": len(e["hunks"])} for e in plan],
        }
    written: list[dict] = []
    for e in plan:
        try:
            fs.write_bytes(root, e["path"], e["new"])
        except (ValueError, OSError) as err:
            # ROLL BACK what already landed — every pre-image is in hand.
            for w in written:
                try:
                    fs.write_bytes(root, w["path"], w["before"])
                except (ValueError, OSError):
                    pass
            return {
                **refusal(TRANSPORT_ERROR, str(err)),
                "restored": [w["path"] for w in written],
                "failed_path": e["path"],
            }
        written.append(e)
    for e in plan:
        slots[(id, _norm(e["path"]))] = e["before"]
    return {
        "edited": True,
        "edits": [
            {
                "path": e["path"],
                "hunks": len(e["hunks"]),
                "size": len(e["new"]),
                "version": _version(e["new"]),
            }
            for e in plan
        ],
    }


async def _undo(id, payload, kernel):
    """args: path:str (req) — swap the document with the bytes the last write/edit/append replaced (a second undo is redo); ONE slot per path, in RAM, lost on restart; {undone:true, path, version|deleted}."""
    rec = kernel.get(id) or {}
    path, err = _path_arg("undo", payload)
    if err:
        return err
    root, err = _text_gate("undo", kernel, rec, path, mutate=True)
    if err:
        return err
    key = (id, _norm(path))
    slots = _UNDO
    async with _held(*key):
        if key not in slots:
            return refusal(
                NOT_FOUND,
                f"undo: nothing to undo for {path!r}",
                hint="the slot holds what the LAST write/edit/append on this path "
                "replaced; it lives in RAM, one per path, and is gone after a restart",
                path=path,
            )
        slot = slots[key]
        try:
            now = _current(root, path)
            if slot is None:
                if now is not None:
                    fs.remove(root, path)
            else:
                fs.write_bytes(root, path, slot)
        except (ValueError, OSError) as e:
            return refusal(TRANSPORT_ERROR, str(e))
        slots[key] = now
    if slot is None:
        return {"undone": True, "path": path, "deleted": True}
    return {"undone": True, "path": path, "version": _version(slot)}


async def on_delete(agent) -> None:
    """The bridge is going away — its undo slots and its disk watch go with it."""
    slots = _UNDO
    for k in [k for k in slots if k[0] == agent.id]:
        del slots[k]
    st = _WATCH.pop(agent.id, None)
    if st and st["task"] is not None:
        st["task"].cancel()


# ─── watching paths (owner, 2026-10-08) ─────────────────────────
#
# `watch_path {path}` puts an EXPOSED file or directory under a disk watcher
# (`watchfiles` — native events: FSEvents / inotify / kqueue): any change to it, by
# any writer (this bridge, an editor, another process), is emitted on this bridge's
# own inbox as `{type: changed, path, kind}`. A caller receives them by WATCHING this
# bridge. A file is watched through its parent directory, so an editor's atomic
# save (write a temp file, rename it over) is seen. Each path is held per caller;
# the disk watch stops when no caller holds any path. RAM only: a kernel restart
# drops every watch (a caller re-arms on boot).

#: A burst of disk events (an editor's save) inside this many ms is one `changed`.
WATCH_DEBOUNCE_MS = 50

# bridge id → {"paths": {norm path: {caller ids}}, "task": asyncio.Task | None}
_WATCH: dict[str, dict] = {}


def _caller() -> str:
    """Who holds a watch: the sending agent, or the operator."""
    from kernel import current_origin

    return current_origin() or "kernel"


async def _watch_path(id, payload, kernel):
    """args: path:str (req — an exposed file or directory). Emit `{type: changed, path, kind}` on this bridge's inbox whenever it changes on disk, by any writer; WATCH this bridge to receive them. Returns {watching, paths}.

    `kind` is `added`, `modified` or `deleted` as the disk reported it — treat any
    `changed` as "read it again". A directory reports its entries (not recursive).
    Held per caller until `unwatch_path`; RAM only."""
    rec = kernel.get(id) or {}
    root = _root(kernel, rec)
    path, err = _path_arg("watch_path", payload)
    if err:
        return err
    if err := _gate_path("watch_path", root, _allow(rec), path):
        return err
    norm = _norm(path) or "."
    parent = posixpath.dirname(norm) or "."
    try:
        is_dir = fs.is_dir(root, norm)
        if not (is_dir or fs.is_dir(root, parent)):
            return refusal(
                NOT_FOUND,
                f"watch_path: no directory {parent!r} to watch {path!r} in",
                path=path,
            )
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    st = _WATCH.setdefault(id, {"paths": {}, "task": None})
    st["paths"].setdefault(norm, set()).add(_caller())
    _rewatch(id, kernel, root)
    return {"watching": norm, "paths": sorted(st["paths"])}


async def _unwatch_path(id, payload, kernel):
    """args: path:str (req). Stop YOUR watch of a path; the disk watch ends when no caller holds a path. Returns {watching:false, paths}."""
    rec = kernel.get(id) or {}
    path, err = _path_arg("unwatch_path", payload)
    if err:
        return err
    norm = _norm(path) or "."
    st = _WATCH.get(id) or {"paths": {}, "task": None}
    holders = st["paths"].get(norm)
    if holders is not None:
        holders.discard(_caller())
        if not holders:
            del st["paths"][norm]
    _rewatch(id, kernel, _root(kernel, rec))
    return {"watching": False, "paths": sorted(st["paths"])}


def _rewatch(id: str, kernel, root: fs.Root) -> None:
    """(Re)start the one disk watcher this bridge runs, over the paths held now."""
    st = _WATCH.get(id)
    if st is None:
        return
    if st["task"] is not None:
        st["task"].cancel()
        st["task"] = None
    if not st["paths"]:
        return
    files: dict[str, str] = {}  # absolute file → its path under the root
    dirs: dict[str, str] = {}  # absolute dir → its path under the root
    for norm in st["paths"]:
        target = fs.resolve(root, "" if norm == "." else norm)
        if fs.is_dir(root, norm):
            dirs[str(target)] = norm
        else:
            files[str(target)] = norm
    tid = kernel.ctx.spawn_task(id, "watch_path", _watch_loop(id, kernel, files, dirs))
    st["task"] = kernel.ctx.tasks[tid]["_task"]


async def _watch_loop(id: str, kernel, files: dict[str, str], dirs: dict[str, str]):
    import os

    import watchfiles

    targets = sorted({os.path.dirname(f) for f in files} | set(dirs))
    async for changes in watchfiles.awatch(
        *targets, recursive=False, debounce=WATCH_DEBOUNCE_MS, raise_interrupt=False
    ):
        seen: dict[str, str] = {}
        for change, raw in changes:
            p = os.path.realpath(raw)
            if p in files:
                seen[files[p]] = change.name
                continue
            d = dirs.get(os.path.dirname(p))
            if d is not None:
                name = os.path.basename(p)
                seen[name if d == "." else f"{d}/{name}"] = change.name
        for rel, kind in seen.items():
            await kernel.emit(id, {"type": "changed", "path": rel, "kind": kind})


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "list": _list,
    "read": _read,
    "write": _write,
    "edit": _edit,
    "append": _append,
    "undo": _undo,
    "read_stream": _read_stream,
    "write_stream": _write_stream,
    "pump": _pump,
    "delete": _delete,
    "rename": _rename,
    "mkdir": _mkdir,
    "watch_path": _watch_path,
    "unwatch_path": _unwatch_path,
}

EMITS = {
    "changed": "{path, kind: added|modified|deleted} — a path under `watch_path` changed "
    "on disk, by any writer; watch this bridge to receive it",
}

UNGATED = {"reflect"}  # discovery must work on a sealed bridge


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a write this bundle cannot honour.

    `record` is the CURRENT record (empty at create) so a bundle can validate the
    MERGED state rather than the patch alone — this one needs only the patch, but the
    hook passes both so a cross-field rule is expressible (see `exec_bridge`, where
    checking the patch alone refused legal writes). Called by `update_agent` BEFORE the
    record changes (duck-typed — the substrate never imports this module).

    The only judgement made here is on the auth posture, because that is the one
    where silently accepting a value you do not implement is actively dangerous: the
    operator is told `{"updated": true}` and believes a control is in place. An
    unknown rule now seals rather than raising (see io_bridge.ingress_rules), so the
    edge is SAFE either way — but safe-and-wrong is still wrong, and the mistake
    belongs at the write, next to the typo, not three calls later."""
    if "allow" in meta:
        a = meta["allow"]
        if not isinstance(a, list) or not all(
            isinstance(x, str) and x and _norm(x) is not None for x in a
        ):
            return (
                "allow must be a list of paths relative to the root (a file, or a "
                "directory exposing everything under it; `.` = the whole root)"
            )
    if bad := unknown_ingress_refusal(meta, ("allow_all_ingress", "deny_ingress")):
        return bad
    return _password_refusal(meta, "fs edge")


def _password_refusal(meta: dict, edge: str) -> str | None:
    """`password_ingress` is meaningless on an in-process leg: the token is read
    off a WIRE ENVELOPE, and the fs/process edges have none — no frame ever
    presents one, so the rule could only ever refuse (python) or, worse, be
    satisfied by a secret persisted ON THE RECORD (the ports, until 2026-09-02).
    Refused at the write, next to the mistake. Shared with exec_bridge."""
    for field, stems in (
        ("ingress_rule", ("password_ingress",)),
        ("auth", ("password",)),
    ):
        if field in meta:
            name, _ = parse_spec(meta[field])
            if name in stems:
                return (
                    f"{field}={meta[field]!r} is not applicable to the {edge}: a leg "
                    "with no wire envelope can never present a token — "
                    "legal values: allow_all_ingress, deny_ingress"
                )
    return None


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"file_bridge: unknown type {t!r}", verb=t)
    if t not in UNGATED:
        # AUTH GATE — the fs edge, gated like any io_bridge leg (sealed by default).
        decision = gate_inbound(
            resolve_ingress(kernel.get(id) or {}),
            "call",
            {"target": id, "payload": payload},
        )
        if not decision.allowed:
            out = {"error": decision.reason, "reason": "unauthorized"}
            if decision.hint:
                out["hint"] = decision.hint
            if decision.see:
                out["see"] = decision.see
            return out
    return await fn(id, payload, kernel)
