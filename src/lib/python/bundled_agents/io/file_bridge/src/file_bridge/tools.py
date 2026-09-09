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
  read_stream  path offset? length?        -> {bytes, next_offset, eof, size} (SOURCE)
  write_stream path bytes offset? truncate? -> {written, offset, size}        (SINK)
  pump   source? source_path sink? sink_path? -> {bytes, chunks}  (server-side copy)
  delete path        -> {path, deleted: true}            (refused if readonly)
  rename old_path new_path -> {old_path, new_path}        (refused if readonly)
  mkdir  path        -> {path, created: true}            (refused if readonly)
"""

from __future__ import annotations


from file_bridge import fs
from io_bridge import describe as _describe
from io_bridge import gate_inbound, resolve_ingress
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


# ─── argument validation ────────────────────────────────────────
# Every refusal below is `bad`/`missing` — the one shape, branchable on `reason`.
# These used to be `int(...)` coercions OUTSIDE the try (a non-numeric string raised
# out of the handler; a negative `length` read 0 bytes and `pump` spun forever), and
# an empty `path` on delete/rename/mkdir resolved to the ROOT itself — `delete
# {path:"", recursive:true}` removed the whole served store.


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


def _target_arg(verb: str, root: str, payload: dict, name: str = "path"):
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
    path, err = _path_arg("list", payload)
    if err:
        return err
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


async def _delete(id, payload, kernel):
    """args: path:str (req), recursive:bool=False. Returns {path, deleted:true}.
    A bare delete removes a file or an EMPTY dir; `recursive:true` removes a dir
    and everything under it (still clamped to root). Refused if readonly."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    root = _root_str(rec)
    path, err = _target_arg("delete", root, payload)
    if err:
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
    root = _root_str(rec)
    old, err = _target_arg("rename", root, payload, "old_path")
    if err:
        return err
    new, err = _target_arg("rename", root, payload, "new_path")
    if err:
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
    path, err = _target_arg("mkdir", _root_str(rec), payload)
    if err:
        return err
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
    path, err = _path_arg("read_stream", payload)
    if err:
        return err
    offset, err = _int_arg("read_stream", payload, "offset", 0, 0)
    if err:
        return err
    length, err = _int_arg("read_stream", payload, "length", 65536, 1)
    if err:
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
    }


async def _write_stream(id, payload, kernel):
    """args: path:str (req), bytes:bytes (req — ONE raw chunk; a model writes it VERBATIM in the `bytes` field, which is cut out before the call is parsed), offset:int? (default =
    append at end), truncate:bool=False. The SINK half — a producer pushes RAW chunks by
    calling repeatedly. Pass truncate:true on the first chunk to start fresh. Returns
    {path, written, offset, size}. Refused if readonly. The chunk is raw bytes — text
    files go through the whole-file `write` verb, not here."""
    rec = kernel.get(id) or {}
    if err := _readonly_or_none(rec):
        return err
    root = _root_str(rec)
    path, err = _path_arg("write_stream", payload)
    if err:
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
    length, err = _int_arg("pump", payload, "chunk", 65536, 1)
    if err:
        return err
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


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "list": _list,
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
                "legal values: allow_all_ingress, deny_ingress"
            )
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
