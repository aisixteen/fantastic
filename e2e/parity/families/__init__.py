"""One family = one boot (or one set of processes) and one output subtree.

A family is a plain module with `NAME` and `run(ctx)`. `run.py` gives it a
`Ctx`: the runtime name, its own fixed workdir, a `Recorder` and the port map.
It may raise — `run.py` records the traceback as a harness error and the capture
exits non-zero, because a family that gave up halfway has produced a SHORTER
manifest, and a short manifest silently agrees with everything it never asked.
"""

from __future__ import annotations

import json
import time
from dataclasses import dataclass, field
from pathlib import Path

#: Baseline grant tokens. Keys in a grant map are literal ids or `$tokens`;
#: `"*"` is a legal VALUE (every verb) and never a legal KEY (it matches no id).
BASE = {"$self": "*", "$parent": "*", "$children": "*"}
#: The ingress rule an agent needs before any leg may call it.
OPEN = {"ingress_rule": "allow_all_ingress"}
#: THE MEMORY STORE `spec.CaptureKernel.seed` writes: one ram_store for the
#: kernel's rows and every AI agent's conversation.
MEMORY = "memory"


def ai(**meta) -> dict:
    """An AI agent's record: its conversation store NAMED and GRANTED.

    `memory_store_id` has no default — an AI agent without one fails its first
    store call — and the store is reached like any agent, so its `can_call`
    names it. The rest of the grant is the family's own (the kernel default,
    `BASE`, when the family states none)."""
    can = {**(meta.pop("can_call", None) or BASE), MEMORY: "*"}
    return {**meta, "memory_store_id": MEMORY, "can_call": can}


@dataclass
class Ctx:
    runtime: str
    rec: object
    ports: object
    work: Path
    link: Path
    slow: bool = False
    meta: dict = field(default_factory=dict)

    def boot(self, **kw):
        """A `CaptureKernel` on this family's workdir, booted."""
        from spec import CaptureKernel

        k = CaptureKernel(self.runtime, self.link, **kw)
        k.boot()
        self.meta.setdefault("root_id", k.root_id)
        self.meta.setdefault("seed_branch", k.seed_branch)
        self.meta.setdefault("port", k.port)
        return k


def call(k, target: str, payload: dict, timeout: float = 60):
    """A send whose transport failure is DATA.

    A refusal is an answer and lands as one. A socket that never answered is
    not: it becomes `__harness_error__`, which fails the capture rather than
    being written where a reply belongs.
    """
    try:
        return k.send(target, payload, timeout=timeout)
    except Exception as e:  # noqa: BLE001 - recorded, then the family fails
        return {"__harness_error__": repr(e)}


def quiesce(stub, k, *, agent: str = "ai", max_s: float = 120, still: float = 4.0,
            rec=None, label: str = "") -> dict:
    """Wait until the model stub has been idle `still` seconds AND the kernel's
    `ps` shows no task for `agent`.

    Two conditions, not one. A stub that has gone quiet can simply be between
    turns, and a `ps` that is empty for an instant can be a turn that has not
    been queued yet; waiting on both is what made the round-2 chains reproducible
    instead of length-dependent.
    """
    t0 = time.monotonic()
    last_n, last_change, polls = len(stub.requests), time.monotonic(), 0
    while time.monotonic() - t0 < max_s:
        time.sleep(0.5)
        n = len(stub.requests)
        if n != last_n:
            last_n, last_change = n, time.monotonic()
            continue
        if time.monotonic() - last_change < still:
            continue
        ps = call(k, "kernel", {"type": "ps"}, timeout=15)
        polls += 1
        tasks = ps.get("tasks") if isinstance(ps, dict) else None
        busy = [t for t in (tasks or []) if isinstance(t, dict) and t.get("agent") == agent]
        if not busy:
            return {"label": label, "requests": n, "waited_s": round(time.monotonic() - t0, 2),
                    "ps_polls": polls}
        last_change = time.monotonic()
    out = {"label": label, "requests": len(stub.requests),
           "waited_s": round(time.monotonic() - t0, 2), "timeout": True}
    if rec is not None:
        rec.note(kind="quiesce_timeout", **out)
    return out


def save_kernel_log(rec, k, rel: str = "kernel_log.txt") -> None:
    """The kernel's own stdout+stderr, RAW.

    Several items are read only here (the cli bundle's intro lines A9.5–A9.7,
    the ledger's own complaints A11.1/C46, the bridge's `skipping malformed
    frame`). It is bytes, not text: a runtime that emits invalid utf-8 on its way
    down is a difference, and a lossy decode erases it.
    """
    try:
        rec.raw(rel, Path(k.klog_path).read_bytes())
    except OSError as e:
        rec.note(kind="kernel_log_unreadable", error=repr(e))


def disk_walk(base: Path) -> list[dict]:
    """Harness-side ground truth for a persisted tree: every file, sorted, with
    its length and sha. Taken WITHOUT the kernel, so a store verb that lies about
    its own directory has something to be wrong against."""
    import hashlib
    import os

    out = []
    if not base.exists():
        return out
    for dp, dns, fns in os.walk(base):
        dns.sort()
        for fn in sorted(fns):
            fp = Path(dp) / fn
            try:
                b = fp.read_bytes()
            except OSError as e:
                out.append({"path": str(fp.relative_to(base)), "error": repr(e)})
                continue
            out.append({"path": str(fp.relative_to(base)), "bytes_len": len(b),
                        "sha256": hashlib.sha256(b).hexdigest()})
    return out


def cursor_of(reply, prefer: str):
    """`(key, value)` of a continuation cursor. `prefer` is python's key; any
    other non-null key ending in `next` is recorded as what the runtime used, so
    a renamed cursor shows up as a NAME difference instead of silently ending a
    page walk after one page."""
    if not isinstance(reply, dict):
        return None, None
    if reply.get(prefer) not in (None, ""):
        return prefer, reply[prefer]
    for key, v in reply.items():
        if key != prefer and key.endswith("next") and v not in (None, ""):
            return key, v
    return None, None


def page_walk(k, rec, aid: str, base_payload: dict, prefer: str, rel: str,
              max_pages: int = 60) -> None:
    pages, seen, payload = [], set(), dict(base_payload)
    stop = "max_pages"
    for _ in range(max_pages):
        r = call(k, aid, payload)
        key, cur = cursor_of(r, prefer)
        pages.append({"payload": dict(payload), "reply": r, "cursor_key": key, "cursor": cur})
        if cur is None:
            stop = "no_cursor"
            break
        if json.dumps(cur, default=str) in seen:
            stop = "cursor_repeated"
            break
        seen.add(json.dumps(cur, default=str))
        payload = {**base_payload, "after": cur}
    rec.json(rel, {"base_payload": base_payload, "stop": stop, "page_count": len(pages),
                   "pages": pages})
