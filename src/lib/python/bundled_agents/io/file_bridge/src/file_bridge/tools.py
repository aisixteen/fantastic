"""file_bridge bundle — the fs-transport member of the io family.

The disk is OUTSIDE the trust domain: crossing kernel↔disk is an IO edge,
gated exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule`
⇒ every verb except `reflect` denies with the teaching shape). Open it
consciously with `ingress_rule=allow_all_ingress`. Even open, the LAW is
running-dir only: the `root` AND every path are clamped inside the dir the
kernel runs in (`../`, `~`, absolute escapes, outward symlinks all refuse).

Each file_bridge owns a `root` directory and a `readonly` flag. Path safety:
every verb resolves `root/path` and refuses if the result escapes `root`.

Verbs:
  reflect            -> {sentence, root, readonly, hidden_count, ingress, egress, sealed, see}
  list   path?       -> {files: [{name, path, type, size?}, ...]}
  read   path        -> {path, content} | {path, image_base64, mime}
  write  path text   -> {path, written: true}            (refused if readonly)
  read_stream  path offset? length?        -> {bytes, next_offset, eof, size} (SOURCE)
  write_stream path bytes offset? truncate? -> {written, offset, size}        (SINK)
  pump   source? source_path sink? sink_path? -> {bytes, chunks}  (server-side copy)
  delete path        -> {path, deleted: true}            (refused if readonly)
  rename old_path new_path -> {old_path, new_path}        (refused if readonly)
  mkdir  path        -> {path, created: true}            (refused if readonly)
"""

from __future__ import annotations

import base64
import mimetypes
from pathlib import PurePosixPath

from file_bridge import fs
from io_bridge import describe as _describe
from io_bridge import gate_inbound, resolve_ingress
from kernel._refusal import (
    NOT_FOUND,
    TRANSPORT_ERROR,
    UNAUTHORIZED,
    UNHANDLED,
    bad,
    missing,
    refusal,
)

DEFAULT_HIDDEN = [".git", ".env", ".fantastic", "node_modules", "__pycache__"]
IMAGE_EXT = {".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg"}

# The disk clamp + raw IO live in `fs` — the ONE disk surface. This agent is the
# GATE (ingress rule) + verb shape over it; it never touches the disk directly.


def _root_str(rec: dict) -> str:
    """The bridge's configured root string — the clamp boundary `fs` enforces."""
    return rec.get("root", "") or ""


def _hidden(rec: dict) -> list[str]:
    return list(rec.get("hidden", DEFAULT_HIDDEN))


def _is_hidden(name: str, hidden: list[str]) -> bool:
    return name in hidden or any(name == p for p in hidden)


# One `read` used to return a whole file, however large. MEASURED 2026-08-07: an AI agent
# read an 86 KB source file, its conversation went to 21k tokens, and the NEXT turn was
# cancelled mid-prefill because prefill at ~95 tok/s outran the backend's 180s send
# timeout. Nothing caught it — the context window (32k) was never exceeded, so the
# compaction handshake never fired and `context_insufficient` never appeared. The turn
# became un-completable on TIME while staying legal on SIZE, and no guard covers that.
#
# So a text read is WINDOWED. Not silently: an unread tail is reported with the totals and
# the offset to continue from, because a caller that cannot tell a whole file from its
# first page will reason about the wrong content.
DEFAULT_MAX_READ_CHARS = 16384


def _max_read_chars(rec: dict) -> int:
    """The leg's text-read window. Per-record like `hidden` — an operator serving small
    config files can raise it; the default protects a conversation from one call."""
    try:
        n = int(rec.get("max_read_chars", DEFAULT_MAX_READ_CHARS))
    except (TypeError, ValueError):
        return DEFAULT_MAX_READ_CHARS
    return n if n > 0 else DEFAULT_MAX_READ_CHARS


def _window_text(rec: dict, payload: dict, text: str) -> tuple[str, dict]:
    """(slice, reply-extras). `offset`/`limit` page a large file explicitly."""
    total = len(text)
    try:
        offset = max(0, int(payload.get("offset", 0)))
    except (TypeError, ValueError):
        offset = 0
    cap = _max_read_chars(rec)
    try:
        limit = int(payload.get("limit", cap))
    except (TypeError, ValueError):
        limit = cap
    limit = min(max(1, limit), cap)
    chunk = text[offset : offset + limit]
    end = offset + len(chunk)
    if offset == 0 and end >= total:
        return chunk, {}
    extras = {
        "total_chars": total,
        "offset": offset,
        "returned_chars": len(chunk),
        "truncated": end < total,
    }
    if end < total:
        extras["next_offset"] = end
        extras["hint"] = (
            f"{total - end} of {total} chars unread — continue with "
            f"{{type:'read', path:…, offset:{end}}} (max {cap} per call, raise the "
            "leg's `max_read_chars` to widen it)"
        )
    return chunk, extras


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
        root_display = str(fs.resolve_root(_root_str(rec)))
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
        "hidden_count": len(_hidden(rec)),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        **_describe(rec),
    }
    if root_error:
        out["root_error"] = root_error
    return out


async def _boot(id, payload, kernel):
    """No-op. Returns None."""
    return None


async def _list(id, payload, kernel):
    """args: path:str (default ''). Returns {path, files:[{name, path, type, size?}, ...]}. Hides DEFAULT_HIDDEN entries (.git, .env, .fantastic, …)."""
    rec = kernel.get(id) or {}
    root = _root_str(rec)
    path = payload.get("path", "")
    try:
        if not fs.exists(root, path):
            return refusal(NOT_FOUND, f"path {path!r} does not exist", path=path)
        if not fs.is_dir(root, path):
            return bad("list", "path", f"a directory; {path!r} is not one")
        rdir = fs.resolve_root(root)
        entries = fs.list_dir(root, path)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    hidden = _hidden(rec)
    files = []
    for entry in entries:
        if _is_hidden(entry.name, hidden):
            continue
        rel = str(entry.relative_to(rdir))
        if entry.is_dir():
            files.append({"name": entry.name, "path": rel, "type": "dir"})
        else:
            try:
                sz = entry.stat().st_size
            except OSError:
                sz = -1
            files.append({"name": entry.name, "path": rel, "type": "file", "size": sz})
    return {"path": path, "files": files}


async def _read(id, payload, kernel):
    """args: path:str (req), offset:int? (0), limit:int? (the leg's max_read_chars, default 16384). Returns {path, content:str} for text — WINDOWED: a file longer than the window also carries {total_chars, offset, returned_chars, truncated, next_offset, hint} so the tail is fetchable and never silently dropped. {path, image_base64, mime} for images, {path, bytes:bytes, mime} for any other binary (PDF, fonts, archives) — raw bytes ride the kernel's binary protocol over WS, zero-copy in-process. Refuses paths outside root."""
    rec = kernel.get(id) or {}
    root = _root_str(rec)
    path = payload.get("path", "")
    ext = PurePosixPath(path).suffix.lower()
    try:
        if not fs.is_file(root, path):
            return refusal(NOT_FOUND, f"file {path!r} not found", path=path)
        if ext in IMAGE_EXT:
            data = fs.read_bytes(root, path)
            mime = {
                ".png": "image/png",
                ".jpg": "image/jpeg",
                ".jpeg": "image/jpeg",
                ".gif": "image/gif",
                ".webp": "image/webp",
                ".svg": "image/svg+xml",
            }.get(ext, "application/octet-stream")
            return {
                "path": path,
                "image_base64": base64.b64encode(data).decode("ascii"),
                "mime": mime,
            }
        try:
            content, window = _window_text(rec, payload, fs.read_text(root, path))
        except UnicodeDecodeError:
            # Generic binary: PDF, font, archive, audio, video. Raw bytes — over WS
            # the kernel's binary protocol ships a binary frame; in-process the
            # webapp's /file/ route pipes them straight to the HTTP response.
            data = fs.read_bytes(root, path)
            mime, _ = mimetypes.guess_type(path)
            return {
                "path": path,
                "bytes": data,
                "mime": mime or "application/octet-stream",
            }
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"path": path, "content": content, **window}


def _as_text(value) -> str:
    """`content` as a string, accepting a LIST OF LINES.

    A multi-line program written as one JSON string is the single most fragile
    thing an LLM emits: every newline is an escape and every inner quote is
    another, so one missed `\"` swallows the rest of the object and the whole
    call is lost. Measured on gemma-4-12B, and again on `subai_graph`, which
    burned three consecutive turns on it.

    A list sidesteps the failure rather than mitigating it: newlines are the
    array structure, not escapes, and each element is short enough that a lost
    quote breaks ONE line instead of the document.

    A LIST is the only newly accepted shape. Everything else is handed back
    untouched so the existing refusals still fire — a bare `42` is a mistake,
    not a one-line program, and quietly stringifying it would spawn an
    interpreter on nonsense.
    """
    if isinstance(value, (list, tuple)):
        return "\n".join("" if v is None else str(v) for v in value)
    return value


async def _write(id, payload, kernel):
    """args: path:str (req), content:str|list[str] (req — a LIST is joined with
    newlines, which is the safer shape for anything with lines in it). Returns
    {path, written:true, bytes}. Creates parent dirs. Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    path = payload.get("path", "")
    content = _as_text(payload.get("content", ""))
    try:
        fs.write_text(_root_str(rec), path, content)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"path": path, "written": True, "bytes": len(content.encode("utf-8"))}


async def _delete(id, payload, kernel):
    """args: path:str (req), recursive:bool=False. Returns {path, deleted:true}.
    A bare delete removes a file or an EMPTY dir; `recursive:true` removes a dir
    and everything under it (still clamped to root). Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    root = _root_str(rec)
    path = payload.get("path", "")
    try:
        if not fs.exists(root, path):
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
    root = _root_str(rec)
    old = payload.get("old_path", "")
    new = payload.get("new_path", "")
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
    path = payload.get("path", "")
    try:
        fs.mkdir(_root_str(rec), path)
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"path": path, "created": True}


# ─── the stream protocol (SOURCE / SINK) ────────────────────────
# A stateless cursor stream: any consumer (file serving, a pump, kernel_state)
# pulls/pushes bytes by id + verb — weak-coupled, no session handle. file_bridge is
# the fs IMPLEMENTOR of this protocol; a network_bridge implements the same verbs over
# the wire, so a consumer is storage-agnostic (G1: the provider is just an id).


async def _read_stream(id, payload, kernel):
    """args: path:str (req), offset:int=0, length:int=65536. The SOURCE half — returns
    ONE chunk {path, offset, bytes, next_offset, eof, size}; pull the next by calling
    again with offset=next_offset until eof. `bytes` is the RAW chunk — it rides every
    transport as raw bytes (web_ws / bridge binary frame, in-proc dict), NEVER base64.
    Stateless cursor — no open handle, weak-coupled by id."""
    rec = kernel.get(id) or {}
    root = _root_str(rec)
    path = payload.get("path", "")
    offset = int(payload.get("offset", 0) or 0)
    length = int(payload.get("length", 65536) or 65536)
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
    }


async def _write_stream(id, payload, kernel):
    """args: path:str (req), bytes:bytes (req — ONE raw chunk), offset:int? (default =
    append at end), truncate:bool=False. The SINK half — a producer pushes RAW chunks by
    calling repeatedly. Pass truncate:true on the first chunk to start fresh. Returns
    {path, written, offset, size}. Refused if readonly. The chunk is raw bytes — text
    files go through the whole-file `write` verb, not here."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    root = _root_str(rec)
    path = payload.get("path", "")
    data = payload.get("bytes", b"")
    if not isinstance(data, (bytes, bytearray)):
        return missing("write_stream", "bytes (one raw chunk)")
    data = bytes(data)
    off_in = payload.get("offset")
    try:
        off, size = fs.write_chunk(
            root,
            path,
            data,
            None if off_in is None else int(off_in),
            truncate=bool(payload.get("truncate")),
        )
    except (ValueError, OSError) as e:
        return refusal(TRANSPORT_ERROR, str(e))
    return {"path": path, "written": len(data), "offset": off, "size": size}


async def _pump(id, payload, kernel):
    """The PUMP — a server-side stream copy SOURCE→SINK, chunk by chunk, in ONE
    call (vs a consumer driving each read/write over the wire). Storage-agnostic:
    both ends are bound BY ID + the duck-typed stream verbs, so a `network_bridge`
    SOURCE pumps to a `file_bridge` SINK the same as fs→fs.

    args: source:str? (provider id, default self) + source_path:str (req) + sink:str?
    (provider id, default self) + sink_path:str? (default = source_path) +
    chunk:int=65536. Each end SELF-gates + SELF-clamps (the pump only coordinates —
    it never reads/writes bytes itself, so a sealed end refuses it). Returns
    {source, sink, bytes, chunks}."""
    src = payload.get("source") or id
    sink = payload.get("sink") or id
    spath = payload.get("source_path") or payload.get("path") or ""
    dpath = payload.get("sink_path") or spath
    length = int(payload.get("chunk", 65536) or 65536)
    offset, first, chunks = 0, True, 0
    while True:
        r = (
            await kernel.send(
                [
                    {
                        "target": src,
                        "payload": {
                            "type": "read_stream",
                            "path": spath,
                            "offset": offset,
                            "length": length,
                        },
                    }
                ]
            )
        )[0]["result"]
        if not isinstance(r, dict) or not isinstance(
            r.get("bytes"), (bytes, bytearray)
        ):
            return refusal(TRANSPORT_ERROR, f"pump: read from {src!r} failed: {r}")
        w = (
            await kernel.send(
                [
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
                ]
            )
        )[0]["result"]
        if not isinstance(w, dict) or w.get("error"):
            return refusal(TRANSPORT_ERROR, f"pump: write to {sink!r} failed: {w}")
        first, chunks = False, chunks + 1
        offset = r["next_offset"]
        if r.get("eof"):
            break
    return {"source": spath, "sink": dpath, "bytes": offset, "chunks": chunks}


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "list": _list,
    "read": _read,
    "write": _write,
    "read_stream": _read_stream,
    "write_stream": _write_stream,
    "pump": _pump,
    "delete": _delete,
    "rename": _rename,
    "mkdir": _mkdir,
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
    if "ingress_rule" in meta:
        spec = meta["ingress_rule"]
        if getattr(resolve_ingress({"ingress_rule": spec}), "unknown", None):
            return (
                f"ingress_rule={spec!r} is not a rule this bundle implements — "
                "legal values: allow_all_ingress, deny_ingress, password_ingress"
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
