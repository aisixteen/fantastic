"""text_file bundle — a stateless text-document agent over the file stream.

Format-free by design: the substrate imposes no memory format, so a document is
whatever text the caller writes (markdown, JSON, source). This agent supplies
the TEXT operations — line-read, atomic write, SEARCH/REPLACE patch, append —
plus ON-DEMAND versioning (`version` content hash on every reply; an
`if_match` guard that fires only when passed) and ON-DEMAND checkpoints
(pre-images as plain files under `.ckpt/` on the same surface; retention is the
caller's).

NO DISK CODE HERE. Every byte routes through the file_bridge AGENT named by
`on` (per call) or the record's `target_id`, over its stream verbs — plain
GATED sends, sender = this agent, so the operator wires both ends
(`can_call`/`callable_by`) and the leg's clamp + ingress rule apply unchanged.
`operator_context()` is deliberately NOT used: these verbs take caller-supplied
paths, which is exactly what the confused-deputy rule forbids escalating.

Atomicity ownership: this agent owns the TRANSACTION protocol — write goes to
`<path>.tmp.<pid>` then `rename` (the leg's one atomic op); a checkpointed
mutate commits the pre-image BEFORE touching the target; a patch verifies every
hunk before writing anything. The residual failure is a stream call dying
mid-sequence: rare, and biased SAFE by ordering — the crash leaves visible
tmp/`.ckpt` debris, never lost data or a half-written target.

Checkpoint ids ARE the dispatch's kernel task pid (a message id, an int) — one id
space with `ps`, the trace, and the chat turn that caused the mutation.
POSIX-legal verbatim as a filename (this project does not target Windows).
"""

from __future__ import annotations

import asyncio
import hashlib
from contextlib import asynccontextmanager

from kernel import current_task
from kernel._refusal import (
    NOT_FOUND,
    UNHANDLED,
    bad,
    missing,
    refusal,
)

#: where pre-images live on the TARGET surface — plain files, visible to `list`.
CKPT_DIR = ".ckpt"
#: read/write chunk size on the stream verbs.
CHUNK = 65536


# ─── argument typing ────────────────────────────────────────────


def _int_arg(payload: dict, key: str, default: int, verb: str, minimum: int):
    """ABSENT ⇒ `default`. Present, a plain int, ≥ `minimum` ⇒ that value.
    ANYTHING ELSE ⇒ a `bad_arg` refusal.

    A present-but-wrong number is a TYPO, and coercing it hides the typo: `read
    {offset: "ten"}` raised `ValueError` out of the handler, and `{offset: null}`
    silently paged from the top. `True` is an int in python and is rejected
    explicitly — `limit: true` would otherwise return exactly one line."""
    if key not in payload:
        return default
    v = payload[key]
    if isinstance(v, bool) or not isinstance(v, int) or v < minimum:
        want = "a non-negative int" if minimum == 0 else f"an int >= {minimum}"
        return bad(verb, key, want)
    return v


# ─── the target surface ─────────────────────────────────────────


def _target(id: str, payload: dict, kernel) -> str | None:
    """The file_bridge id this call operates on: `on` wins, else the record's
    `target_id`. None means unwired — every verb fails loud on it."""
    on = payload.get("on")
    if isinstance(on, str) and on:
        return on
    rec = kernel.get(id) or {}
    tid = rec.get("target_id")
    return tid if isinstance(tid, str) and tid else None


def _unwired(verb: str) -> dict:
    return refusal(
        NOT_FOUND,
        f"{verb}: no target surface — pass on=<file_bridge id> or set "
        "`target_id` on this agent's record",
        hint="the operator wires both ends: this agent's can_call names the "
        "leg, and the leg's callable_by accepts this agent",
    )


def _version(data: bytes) -> str:
    """The on-demand version: a content hash, computed, never stored."""
    return hashlib.sha256(data).hexdigest()[:16]


def _pid() -> int:
    """This dispatch's kernel task pid (a message id) — the checkpoint id space.
    0 outside any dispatch (an operator call): never minted, so never a clash."""
    return current_task() or 0


# ─── per-document serialization ─────────────────────────────────
# A mutate is read → verify → commit with awaits between; two concurrent
# mutates on ONE document interleaved there would both derive from the same
# pre-image and the second commit would silently discard the first (measured:
# two patches, distinct hunks, both `patched:true`, one hunk gone). One lock
# per (target surface, path), held from the first read through the rename;
# `if_match` is checked UNDER it. Multi-edit patches take theirs in sorted
# key order so two transactions over overlapping sets cannot deadlock.
# Entries are ref-counted and dropped at zero so the map never grows.

_LOCKS: dict[tuple[str, str], list] = {}


@asynccontextmanager
async def _held(fb: str, *paths: str):
    keys = sorted({(fb, p) for p in paths})
    entries: list[list] = []
    for k in keys:
        e = _LOCKS.get(k)
        if e is None:
            e = _LOCKS[k] = [asyncio.Lock(), 0]
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
                _LOCKS.pop(k, None)


# ─── stream I/O (every byte through the gated leg) ──────────────


async def _read_all(kernel, fb: str, path: str):
    """Whole file via the leg's `read_stream` chunk loop. Returns
    `(bytes, None)` or `(None, refusal-dict)` — the leg's own refusal
    (not_found, unauthorized) passes through verbatim."""
    chunks: list[bytes] = []
    offset = 0
    while True:
        r = (
            await kernel.send(
                [
                    {
                        "target": fb,
                        "payload": {
                            "type": "read_stream",
                            "path": path,
                            "offset": offset,
                            "length": CHUNK,
                        },
                    }
                ]
            )
        )[0]["result"]
        if not isinstance(r, dict) or r.get("error"):
            return None, (r if isinstance(r, dict) else {"error": str(r)})
        b = r.get("bytes") or b""
        chunks.append(bytes(b))
        if r.get("eof") or not b:
            break
        offset = r.get("next_offset", offset + len(b))
    return b"".join(chunks), None


async def _write_chunks(kernel, fb: str, path: str, data: bytes):
    """Write `data` to `path` on the leg — truncate-first, then append chunks.
    NOT atomic on its own; `_write_atomic` adds the tmp+rename commit."""
    off = 0
    first = True
    while first or off < len(data):
        chunk = data[off : off + CHUNK]
        r = (
            await kernel.send(
                [
                    {
                        "target": fb,
                        "payload": {
                            "type": "write_stream",
                            "path": path,
                            "bytes": chunk,
                            "offset": off,
                            "truncate": first,
                        },
                    }
                ]
            )
        )[0]["result"]
        if not isinstance(r, dict) or r.get("error"):
            return r if isinstance(r, dict) else {"error": str(r)}
        off += len(chunk)
        first = False
    return None


async def _write_atomic(kernel, fb: str, path: str, data: bytes):
    """The COMMIT: write to a tmp sibling, then the leg's one atomic `rename`.
    A crash before the rename leaves only tmp debris."""
    tmp = f"{path}.tmp.{_pid()}"
    if err := await _write_chunks(kernel, fb, tmp, data):
        return err
    r = (
        await kernel.send(
            [
                {
                    "target": fb,
                    "payload": {"type": "rename", "old_path": tmp, "new_path": path},
                }
            ]
        )
    )[0]["result"]
    if not isinstance(r, dict) or r.get("error"):
        return r if isinstance(r, dict) else {"error": str(r)}
    return None


def _ckpt_path(path: str, ckpt_id: int | str) -> str:
    # `/` in the document path would nest under .ckpt; flatten it so every
    # pre-image of one document lives in ONE listable dir.
    return f"{CKPT_DIR}/{path.replace('/', '__')}/{ckpt_id}"


async def _if_match_guard(kernel, fb: str, path: str, want: str | None):
    """The on-demand optimistic guard: only fires when the caller passed
    `if_match`. Returns a refusal dict on mismatch, else None."""
    if want is None:
        return None
    data, err = await _read_all(kernel, fb, path)
    if err is not None:
        return err
    have = _version(data)
    if have != want:
        return {
            "error": "version mismatch — the document changed since you read it",
            "reason": "version_mismatch",
            "expected": want,
            "actual": have,
        }
    return None


async def _take_checkpoint(kernel, fb: str, path: str):
    """Commit the pre-image BEFORE any mutate touches the target. A file that
    does not exist yet checkpoints as ABSENT (restore = delete)."""
    ckpt_id = _pid()
    data, err = await _read_all(kernel, fb, path)
    if err is not None and err.get("reason") == "not_found":
        data = None  # pre-image: absence
    elif err is not None:
        return None, err
    marker = data if data is not None else b""
    dest = _ckpt_path(path, ckpt_id)
    # An absent pre-image is recorded as an empty file with an `.absent` twin —
    # restore then deletes the document instead of emptying it.
    if err := await _write_chunks(kernel, fb, dest, marker):
        return None, err
    if data is None:
        if err := await _write_chunks(kernel, fb, dest + ".absent", b""):
            return None, err
    return ckpt_id, None


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """no args — identity, verb guide, and the wired default surface."""
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": rec.get("description")
        or "Text documents on a file surface — any format, the caller's choice: "
        "line-read, atomic write, SEARCH/REPLACE patch, append; versioning and "
        "checkpoints ON DEMAND.",
        "target_id": rec.get("target_id"),
        "patch_grammar": (
            "one or more blocks: `<<<<<<< SEARCH` / <old lines> / `=======` / "
            "<new lines> / `>>>>>>> REPLACE`; each SEARCH must match its "
            "document EXACTLY ONCE. `patch` is a TRANSACTION over "
            "`edits:[{path, hunks, if_match?}]` — even one file rides the "
            "array; the blob holds every edit's hunks concatenated in `edits` "
            "order, split by the per-edit `hunks` count; any failure anywhere "
            "writes nothing"
        ),
        "compose": (
            "every content-carrying verb (`write`, `patch`, `append`) takes its "
            "document in `bytes`, and `bytes` is VERBATIM: write the document "
            "between `<bytes>` and `</bytes>` exactly as it should land — real "
            "newlines, real quotes, nothing escaped, no fence — then close the "
            "call. The document ends at the first `</bytes>`. A patch names its "
            "edits as tags: <edits><item><path>a.py</path><hunks>2</hunks></item></edits>"
        ),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _boot(id, payload, kernel):
    """no args — no-op; this agent holds no state to hydrate."""
    return None


async def _read(id, payload, kernel):
    """args: path:str (req), on:str?, offset:int=0 (line), limit:int=200 — {path, lines:[{n,text}], total, eof, version}; refuses binary."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("read")
    path = payload.get("path", "")
    if not path:
        return missing("read", "path")
    # Type the paging args BEFORE the read — a typo'd `offset` costs no IO.
    off = _int_arg(payload, "offset", 0, "read", 0)
    if isinstance(off, dict):
        return off
    limit = _int_arg(payload, "limit", 200, "read", 1)
    if isinstance(limit, dict):
        return limit
    data, err = await _read_all(kernel, fb, path)
    if err is not None:
        return err
    if b"\x00" in data:
        return bad("read", "path", "a text file (this one is binary)")
    try:
        text = data.decode("utf-8")
    except UnicodeDecodeError:
        return bad("read", "path", "valid utf-8 (this file is not)")
    all_lines = text.split("\n")
    # A trailing newline yields one phantom empty tail element — drop it so
    # `total` counts real lines.
    if all_lines and all_lines[-1] == "":
        all_lines.pop()
    page = all_lines[off : off + limit]
    return {
        "path": path,
        "lines": [{"n": off + i + 1, "text": t} for i, t in enumerate(page)],
        "total": len(all_lines),
        "eof": off + limit >= len(all_lines),
        "version": _version(data),
    }


async def _write(id, payload, kernel):
    """args: path:str (req), bytes (req — the whole document; str is utf-8-encoded), on:str?, if_match:str?, checkpoint:bool? — atomic tmp+rename; {path, size, version, ckpt_id?}."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("write")
    path = payload.get("path", "")
    if not path:
        return missing("write", "path")
    data = payload.get("bytes")
    if isinstance(data, str):
        # TEXT agent, text courtesy: ai_core utf-8-encodes for the MODEL, but a
        # spawned program's ctrl-fd wire is JSON (no bytes type) — encode here
        # so every caller gets the same treatment (measured 2026-09-01, E20:
        # a program's write was refused and the model improvised base64).
        data = data.encode("utf-8")
    if not isinstance(data, (bytes, bytearray)):
        return missing("write", "bytes (the document — str is utf-8-encoded)")
    data = bytes(data)
    async with _held(fb, path):
        if err := await _if_match_guard(kernel, fb, path, payload.get("if_match")):
            return err
        ckpt_id = None
        if payload.get("checkpoint"):
            ckpt_id, err = await _take_checkpoint(kernel, fb, path)
            if err is not None:
                return err
        if err := await _write_atomic(kernel, fb, path, data):
            return err
    out = {"path": path, "size": len(data), "version": _version(data)}
    if ckpt_id:
        out["ckpt_id"] = ckpt_id
    return out


async def _append(id, payload, kernel):
    """args: path:str (req), bytes (req — one chunk; str is utf-8-encoded), on:str? — appends (creates if absent); {path, size, version}."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("append")
    path = payload.get("path", "")
    if not path:
        return missing("append", "path")
    data = payload.get("bytes")
    if isinstance(data, str):
        data = data.encode("utf-8")  # text courtesy — same as `write`
    if not isinstance(data, (bytes, bytearray)):
        return missing("append", "bytes (one chunk — str is utf-8-encoded)")
    async with _held(fb, path):
        r = (
            await kernel.send(
                [
                    {
                        "target": fb,
                        "payload": {
                            "type": "write_stream",
                            "path": path,
                            "bytes": bytes(data),
                            # absent offset = append at end (creates the file)
                        },
                    }
                ]
            )
        )[0]["result"]
        if not isinstance(r, dict) or r.get("error"):
            return r if isinstance(r, dict) else {"error": str(r)}
        whole, err = await _read_all(kernel, fb, path)
        if err is not None:
            return err
    return {"path": path, "size": r.get("size"), "version": _version(whole)}


# ─── patch (moved from file_bridge — same grammar, same guarantees) ─────
# A STRUCTURED whole-document edit: SEARCH/REPLACE hunks applied against the
# EVOLVING content, each hunk required to match EXACTLY ONCE; any failure
# writes NOTHING (all-or-nothing, no fuzzy apply). The patch is TEXT (utf-8)
# but rides as raw BYTES so quote/newline-heavy hunks are never JSON-escaped.
_H_START, _H_DIV, _H_END = "<<<<<<< SEARCH", "=======", ">>>>>>> REPLACE"


def _parse_hunks(text: str) -> list[tuple[str, str]]:
    """Parse SEARCH/REPLACE blocks into (search, replace) pairs. The text inside a
    block is taken VERBATIM (the marker lines excluded). Raises ValueError on a block
    that opens but does not close."""
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


async def _patch(id, payload, kernel):
    """args: edits:[{path:str, hunks:int, if_match:str?}] (req — even for ONE file), bytes (req — ALL hunks concatenated; str is utf-8-encoded), on:str?, dry_run:bool?, checkpoint:bool? — a TRANSACTION: verify every hunk of every edit, checkpoint all, apply all, drop the pre-images; any failure writes nothing (or restores what was written); {patched:true, edits:[{path,hunks,size,version}], ckpt_ids?}."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("patch")
    edits = payload.get("edits")
    if not isinstance(edits, list) or not edits:
        return missing("patch", "edits (a list of {path, hunks, if_match?})")
    data = payload.get("bytes")
    if isinstance(data, str):
        data = data.encode("utf-8")  # text courtesy — same as `write`
    if not isinstance(data, (bytes, bytearray)) or not data:
        return missing("patch", "bytes (the utf-8 patch — all hunks, in edits order)")
    try:
        patch = bytes(data).decode("utf-8")
    except UnicodeDecodeError:
        return bad("patch", "bytes", "valid utf-8 (the patch is text)")
    try:
        hunks = _parse_hunks(patch)
    except ValueError as e:
        return bad("patch", "bytes", f"a well-formed patch — {e}")
    if not hunks:
        return bad(
            "patch",
            "bytes",
            "at least one `<<<<<<< SEARCH` / `=======` / `>>>>>>> REPLACE` block",
        )
    # ── address the one hunk stream: split by per-edit counts ──
    plan: list[dict] = []
    cursor = 0
    for i, e in enumerate(edits):
        if (
            not isinstance(e, dict)
            or not isinstance(e.get("path"), str)
            or not e["path"]
        ):
            return bad("patch", "edits", f"edit {i}: an object with a `path`")
        n = e.get("hunks")
        if not isinstance(n, int) or isinstance(n, bool) or n < 1:
            return bad(
                "patch", "edits", f"edit {i} ({e['path']!r}): `hunks` (int >= 1)"
            )
        if any(p["path"] == e["path"] for p in plan):
            return bad(
                "patch",
                "edits",
                "each path at most once — put all of a file's hunks in one edit",
            )
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
            "patch",
            "edits",
            f"hunk counts that sum to the blob's blocks — edits claim {cursor}, "
            f"the patch holds {len(hunks)}",
        )
    # Every edit's document is held from the first read to the last rename —
    # sorted key order, so overlapping transactions cannot deadlock.
    async with _held(fb, *(e["path"] for e in plan)):
        return await _patch_txn(kernel, fb, plan, payload)


async def _patch_txn(kernel, fb: str, plan: list[dict], payload: dict):
    """The locked half of `patch`: verify every hunk against the CURRENT
    pre-image (the `if_match` check happens here, under the lock), checkpoint
    all, apply all, self-clean or keep the pre-images."""
    # ── VERIFY ALL BEFORE ANY: compute every file's new content in memory ──
    for e in plan:
        raw, err = await _read_all(kernel, fb, e["path"])
        if err is not None:
            return err
        if e["if_match"] is not None and _version(raw) != e["if_match"]:
            return {
                "error": "version mismatch — the document changed since you read it",
                "reason": "version_mismatch",
                "path": e["path"],
                "expected": e["if_match"],
                "actual": _version(raw),
            }
        try:
            working = raw.decode("utf-8")
        except UnicodeDecodeError:
            return bad("patch", "edits", f"{e['path']!r}: a utf-8 text file")
        for idx, (search, replace) in enumerate(e["hunks"]):
            hits = working.count(search)
            if hits != 1:
                reason = "no_match" if hits == 0 else "ambiguous"
                detail = (
                    "search text not found"
                    if hits == 0
                    else f"search text matches {hits} places"
                )
                return {
                    "error": "a hunk did not apply — nothing written",
                    "reason": reason,
                    "failed": [{"path": e["path"], "index": idx, "reason": detail}],
                }
            working = working.replace(search, replace, 1)
        e["new"] = working.encode("utf-8")
    if payload.get("dry_run"):
        return {
            "dry_run": True,
            "edits": [{"path": e["path"], "hunks": len(e["hunks"])} for e in plan],
        }
    # ── CHECKPOINT ALL, then APPLY ALL; a mid-apply failure restores ──
    # everything already written from its pre-image. Pre-images are ALWAYS
    # taken (they ARE the rollback); `checkpoint:true` only KEEPS them.
    ckpts: list[tuple[str, str]] = []
    for e in plan:
        ckpt_id, err = await _take_checkpoint(kernel, fb, e["path"])
        if err is not None:
            return err
        ckpts.append((e["path"], ckpt_id))
    written: list[tuple[str, str]] = []
    for e, (_, ckpt_id) in zip(plan, ckpts):
        err = await _write_atomic(kernel, fb, e["path"], e["new"])
        if err is not None:
            # ROLL BACK what already landed — the pre-images were committed
            # before the first write, so every restore has a source.
            for path, ck in written:
                await _restore_from(kernel, fb, path, ck)
            err = dict(err) if isinstance(err, dict) else {"error": str(err)}
            err["restored"] = [p for p, _ in written]
            err["failed_path"] = e["path"]
            return err
        written.append((e["path"], ckpt_id))
    out = {
        "patched": True,
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
    if payload.get("checkpoint"):
        out["ckpt_ids"] = [{"path": p, "ckpt_id": c} for p, c in ckpts]
    else:
        for path, ck in ckpts:
            await _drop_quiet(kernel, fb, path, ck)
    return out


async def _restore_from(kernel, fb: str, path: str, ckpt_id: str) -> None:
    """Rollback arm of the patch transaction: put the pre-image back (or delete
    a file that did not exist). Best-effort by design — it runs INSIDE a failure
    path that is already being reported."""
    src = _ckpt_path(path, ckpt_id)
    absent = (
        await kernel.send(
            [
                {
                    "target": fb,
                    "payload": {
                        "type": "read_stream",
                        "path": src + ".absent",
                        "offset": 0,
                        "length": 1,
                    },
                }
            ]
        )
    )[0]["result"]
    if isinstance(absent, dict) and not absent.get("error"):
        await kernel.send([{"target": fb, "payload": {"type": "delete", "path": path}}])
        return
    data, err = await _read_all(kernel, fb, src)
    if err is None:
        await _write_atomic(kernel, fb, path, data)


async def _drop_quiet(kernel, fb: str, path: str, ckpt_id: str) -> None:
    """Self-cleaning arm: a COMMITTED transaction discards its pre-images (the
    caller opted out of keeping them with `checkpoint` unset)."""
    src = _ckpt_path(path, ckpt_id)
    await kernel.send([{"target": fb, "payload": {"type": "delete", "path": src}}])
    await kernel.send(
        [{"target": fb, "payload": {"type": "delete", "path": src + ".absent"}}]
    )


# ─── checkpoints (on demand; pre-images as plain files) ─────────


async def _checkpoint(id, payload, kernel):
    """args: path:str (req), on:str? — commit a pre-image now, mutation-free; {ckpt_id, path, version}."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("checkpoint")
    path = payload.get("path", "")
    if not path:
        return missing("checkpoint", "path")
    data, err = await _read_all(kernel, fb, path)
    if err is not None:
        return err
    ckpt_id, err = await _take_checkpoint(kernel, fb, path)
    if err is not None:
        return err
    return {"ckpt_id": ckpt_id, "path": path, "version": _version(data)}


async def _checkpoints(id, payload, kernel):
    """args: path:str (req), on:str? — the pre-images held for a document; {path, ckpts:[{id, size}]}."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("checkpoints")
    path = payload.get("path", "")
    if not path:
        return missing("checkpoints", "path")
    r = (
        await kernel.send(
            [
                {
                    "target": fb,
                    "payload": {
                        "type": "list",
                        "path": f"{CKPT_DIR}/{path.replace('/', '__')}",
                    },
                }
            ]
        )
    )[0]["result"]
    if not isinstance(r, dict) or r.get("error"):
        # no dir yet = no checkpoints, honestly empty
        return {"path": path, "ckpts": []}
    ckpts = [
        {
            "id": int(f["name"]) if f["name"].isdigit() else f["name"],
            "size": f.get("size"),
        }
        for f in r.get("files") or []
        if f.get("type") == "file" and not f["name"].endswith(".absent")
    ]
    return {"path": path, "ckpts": ckpts}


async def _restore(id, payload, kernel):
    """args: path:str (req), ckpt_id:str (req), on:str?, if_match:str? — atomic restore of the pre-image (or delete, if it was absent); {restored:true, path, version|deleted}."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("restore")
    path = payload.get("path", "")
    if not path:
        return missing("restore", "path")
    ckpt_id = payload.get("ckpt_id", "")
    if not ckpt_id:
        return missing("restore", "ckpt_id")
    async with _held(fb, path):
        if err := await _if_match_guard(kernel, fb, path, payload.get("if_match")):
            return err
        src = _ckpt_path(path, ckpt_id)
        # An `.absent` twin means the pre-image was NO FILE — restore deletes.
        absent = (
            await kernel.send(
                [
                    {
                        "target": fb,
                        "payload": {
                            "type": "read_stream",
                            "path": src + ".absent",
                            "offset": 0,
                            "length": 1,
                        },
                    }
                ]
            )
        )[0]["result"]
        if isinstance(absent, dict) and not absent.get("error"):
            r = (
                await kernel.send(
                    [{"target": fb, "payload": {"type": "delete", "path": path}}]
                )
            )[0]["result"]
            if not isinstance(r, dict) or r.get("error"):
                return r if isinstance(r, dict) else {"error": str(r)}
            return {"restored": True, "path": path, "deleted": True}
        data, err = await _read_all(kernel, fb, src)
        if err is not None:
            if err.get("reason") == "not_found":
                return refusal(
                    NOT_FOUND, f"restore: no checkpoint {ckpt_id!r} for {path!r}"
                )
            return err
        if err := await _write_atomic(kernel, fb, path, data):
            return err
    return {"restored": True, "path": path, "version": _version(data)}


async def _drop(id, payload, kernel):
    """args: path:str (req), ckpt_id:str (req), on:str? — discard one pre-image (retention is the caller's); {dropped:true}."""
    fb = _target(id, payload, kernel)
    if not fb:
        return _unwired("drop")
    path = payload.get("path", "")
    ckpt_id = payload.get("ckpt_id", "")
    if not path:
        return missing("drop", "path")
    if not ckpt_id:
        return missing("drop", "ckpt_id")
    src = _ckpt_path(path, ckpt_id)
    r = (
        await kernel.send([{"target": fb, "payload": {"type": "delete", "path": src}}])
    )[0]["result"]
    if not isinstance(r, dict) or r.get("error"):
        return r if isinstance(r, dict) else {"error": str(r)}
    # best-effort: the `.absent` twin, if any (its absence is not an error)
    await kernel.send(
        [{"target": fb, "payload": {"type": "delete", "path": src + ".absent"}}]
    )
    return {"dropped": True, "ckpt_id": ckpt_id, "path": path}


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "read": _read,
    "write": _write,
    "patch": _patch,
    "append": _append,
    "checkpoint": _checkpoint,
    "checkpoints": _checkpoints,
    "restore": _restore,
    "drop": _drop,
}


def validate_meta(meta: dict, record: dict) -> str | None:
    """Refuse a write this bundle cannot honour: `target_id`, when present, must
    be a non-empty agent-id string (the default surface this agent operates on)."""
    if "target_id" in meta:
        t = meta["target_id"]
        if not isinstance(t, str) or not t:
            return "target_id must be a non-empty file_bridge agent id"
    return None


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"text_file: unknown type {t!r}", verb=t)
    return await fn(id, payload, kernel)
