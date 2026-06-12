"""file_bridge bundle — the fs-transport member of the io family.

The disk is OUTSIDE the trust domain: crossing kernel↔disk is an IO edge,
gated exactly like a bridge leg — SEALED BY DEFAULT (absent `ingress_rule`
⇒ every verb except `reflect` denies with the teaching shape). Open it
consciously with `ingress_rule=allow_all`. Even open, the LAW is
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


def _readonly_or_none(rec: dict) -> dict | None:
    if rec.get("readonly"):
        return {"error": "agent is readonly"}
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
            return {"error": f"path {path!r} does not exist"}
        if not fs.is_dir(root, path):
            return {"error": f"path {path!r} is not a directory"}
        rdir = fs.resolve_root(root)
        entries = fs.list_dir(root, path)
    except (ValueError, OSError) as e:
        return {"error": str(e)}
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
    """args: path:str (req). Returns {path, content:str} for text, {path, image_base64, mime} for images, {path, bytes:bytes, mime} for any other binary (PDF, fonts, archives, etc.) — raw bytes ride the kernel's binary protocol over WS, zero-copy in-process. Refuses paths outside root."""
    rec = kernel.get(id) or {}
    root = _root_str(rec)
    path = payload.get("path", "")
    ext = PurePosixPath(path).suffix.lower()
    try:
        if not fs.is_file(root, path):
            return {"error": f"file {path!r} not found"}
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
            content = fs.read_text(root, path)
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
        return {"error": str(e)}
    return {"path": path, "content": content}


async def _write(id, payload, kernel):
    """args: path:str (req), content:str (req). Returns {path, written:true, bytes}. Creates parent dirs. Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    path = payload.get("path", "")
    content = payload.get("content", "")
    try:
        fs.write_text(_root_str(rec), path, content)
    except (ValueError, OSError) as e:
        return {"error": str(e)}
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
            return {"error": f"path {path!r} does not exist"}
        fs.remove(root, path, recursive=bool(payload.get("recursive")))
    except (ValueError, OSError) as e:
        return {"error": str(e)}
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
            return {"error": f"path {old!r} does not exist"}
        fs.rename(root, old, new)
    except (ValueError, OSError) as e:
        return {"error": str(e)}
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
        return {"error": str(e)}
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
            return {"error": f"file {path!r} not found"}
        chunk, size = fs.read_chunk(root, path, offset, length)
    except (ValueError, OSError) as e:
        return {"error": str(e)}
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
        return {"error": "write_stream: `bytes` (one raw chunk) required"}
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
        return {"error": str(e)}
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
        r = await kernel.send(
            src,
            {"type": "read_stream", "path": spath, "offset": offset, "length": length},
        )
        if not isinstance(r, dict) or not isinstance(
            r.get("bytes"), (bytes, bytearray)
        ):
            return {"error": f"pump: read from {src!r} failed: {r}"}
        w = await kernel.send(
            sink,
            {
                "type": "write_stream",
                "path": dpath,
                "bytes": r["bytes"],
                "offset": offset,
                "truncate": first,
            },
        )
        if not isinstance(w, dict) or w.get("error"):
            return {"error": f"pump: write to {sink!r} failed: {w}"}
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


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"file_bridge: unknown type {t!r}"}
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
