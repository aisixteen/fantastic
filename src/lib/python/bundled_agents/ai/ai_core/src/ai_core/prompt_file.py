"""The system block from a FILE (owner, 2026-10-08): `system_prompt_file: {bridge, path}`.

The agent reads the file THROUGH that file_bridge, AS ITSELF — every read and watch
is its own gated send, so its `can_call` names the bridge (`read_stream`,
`watch_path`, `unwatch_path`, `watch`) and the bridge's `callable_by` names it. It
holds the path under the bridge's disk watcher (`watch_path`) and watches the bridge
through a private inbox: a `changed` event for the path marks the text STALE, and
the next turn reads it again. An editor's save is live on the next turn — no
restart, and no read on a turn nothing changed.

Armed on the first turn that needs it (and again after a kernel restart — all of
this is RAM); re-armed when the record names another file; dropped when the field
is cleared or the agent deleted. A file that cannot be read leaves the turn on the
block below it in the precedence, and the reason is the agent's
`system_prompt_file_error` (reflect) until a read succeeds.
"""

from __future__ import annotations

import posixpath

#: The refusal for a `system_prompt_file` the record cannot use.
REFUSAL = (
    "system_prompt_file must be {bridge: <file_bridge id>, path: <path under its "
    "root>} — null clears it"
)

# agent id → {ref, text, stale, error, inbox, task}
_FILES: dict[str, dict] = {}


def ref_of(rec: dict) -> tuple[str, str] | None:
    """`(bridge, path)` the record names, or None."""
    v = rec.get("system_prompt_file")
    if (
        isinstance(v, dict)
        and isinstance(v.get("bridge"), str)
        and v["bridge"].strip()
        and isinstance(v.get("path"), str)
        and v["path"].strip()
    ):
        return v["bridge"], v["path"]
    return None


def shape_error(v) -> str | None:
    """Why a written `system_prompt_file` is unusable, or None (null clears it)."""
    if v is None:
        return None
    if (
        not isinstance(v, dict)
        or set(v) != {"bridge", "path"}
        or ref_of({"system_prompt_file": v}) is None
    ):
        return REFUSAL
    return None


def error_of(self_id: str) -> str | None:
    """Why the file is not this agent's block right now, or None."""
    st = _FILES.get(self_id)
    return st["error"] if st else None


async def text(self_id: str, kernel) -> str | None:
    """The file's text for this turn, or None (no file named, or unreadable). Runs
    in the agent's own turn, so every send here is the agent's."""
    ref = ref_of(kernel.get(self_id) or {})
    st = _FILES.get(self_id)
    if ref is None:
        if st is not None:
            await drop(self_id, kernel)
        return None
    if st is None or st["ref"] != ref:
        if st is not None:
            await drop(self_id, kernel)
        st = await _arm(self_id, kernel, ref)
    elif (
        st["task"] is None
    ):  # arming failed on an earlier turn — the same one path, again
        st = await _arm(self_id, kernel, ref)
    if st["stale"] and st["task"] is not None:
        st["text"], st["error"] = await _read(kernel, *ref)
        st["stale"] = False
    return st["text"]


async def _arm(self_id: str, kernel, ref: tuple[str, str]) -> dict:
    bridge, path = ref
    inbox = f"{self_id}/prompt_file"
    st = {
        "ref": ref,
        "text": None,
        "stale": True,
        "error": None,
        "inbox": inbox,
        "task": None,
    }
    _FILES[self_id] = st
    out = (
        await kernel.send(
            [{"target": bridge, "payload": {"type": "watch_path", "path": path}}]
        )
    )[0]
    res = out.get("result")
    if (
        out.get("status") != "completed"
        or not isinstance(res, dict)
        or res.get("error")
    ):
        st["error"] = f"{bridge} watch_path {path}: {_why(out)}"
        return st
    ctx = kernel.ctx
    q = ctx.ensure_inbox(inbox)
    denial = kernel.watch(bridge, inbox)
    if denial is not None:
        ctx.inboxes.pop(inbox, None)
        st["error"] = f"{bridge} watch: {denial.get('error')}"
        await _unwatch_path(kernel, bridge, path)
        return st
    norm = posixpath.normpath(path)

    async def drain() -> None:
        while True:
            ev = await q.get()
            if (
                isinstance(ev, dict)
                and ev.get("type") == "changed"
                and posixpath.normpath(str(ev.get("path"))) == norm
            ):
                st["stale"] = True

    tid = ctx.spawn_task(self_id, "prompt_file", drain())
    st["task"] = ctx.tasks[tid]["_task"]
    return st


async def _read(kernel, bridge: str, path: str) -> tuple[str | None, str | None]:
    """The whole file through `read_stream`, as utf-8 → (text, None) or (None, why)."""
    chunks: list[bytes] = []
    offset = 0
    while True:
        out = (
            await kernel.send(
                [
                    {
                        "target": bridge,
                        "payload": {
                            "type": "read_stream",
                            "path": path,
                            "offset": offset,
                        },
                    }
                ]
            )
        )[0]
        res = out.get("result")
        if (
            out.get("status") != "completed"
            or not isinstance(res, dict)
            or res.get("error")
        ):
            return None, f"{bridge} read_stream {path}: {_why(out)}"
        chunks.append(bytes(res.get("bytes") or b""))
        if res.get("eof"):
            break
        offset = res["next_offset"]
    try:
        return b"".join(chunks).decode("utf-8"), None
    except UnicodeDecodeError as e:
        return None, f"{bridge} {path}: not utf-8 ({e})"


async def drop(self_id: str, kernel) -> None:
    """Stop watching: the drain task, the watch, the inbox, the bridge's path hold."""
    st = _FILES.pop(self_id, None)
    if st is None:
        return
    bridge, path = st["ref"]
    if st["task"] is not None:
        st["task"].cancel()
        kernel.unwatch(bridge, st["inbox"])
        kernel.ctx.inboxes.pop(st["inbox"], None)
        await _unwatch_path(kernel, bridge, path)


async def _unwatch_path(kernel, bridge: str, path: str) -> None:
    await kernel.send(
        [{"target": bridge, "payload": {"type": "unwatch_path", "path": path}}]
    )


def _why(out: dict) -> str:
    res = out.get("result")
    if isinstance(res, dict) and res.get("error"):
        return str(res["error"])
    return str(out.get("status"))
