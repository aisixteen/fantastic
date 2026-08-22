"""shims — the wire clients, as DATA.

This bridge publishes a WIRE and nothing else. A shim is a blob a request may ASK for:

    {type:'start', code:'…', shim:'python'}   → the blob is prepended to the program
    {type:'start', code:'…'}                  → raw wire; the program speaks it itself

That is the whole reason `exec_bridge` is generic where a per-language executor is not.
Prepending a language's client unconditionally is precisely what binds an entire executor
to one language. Here, adding a language is adding an entry to `SHIMS` — a blob — not a
code path, not a bundle, not a branch in the spawner.

## Why prepending is not the only option

A shim is prepended as SOURCE, which works for interpreters that execute a script top to
bottom from a `-c`/`-e` argument. That is not universal, and the bridge does not pretend
it is: an interpreter whose argv template writes a file, or a language needing a header
before any statement, is served by a program that reads `{type:'shim', name:…}` and
arranges its own wiring. The published contract is the WIRE; the shim is a convenience
over it, and `shim:` being optional is what keeps the wire primary.

## The wire (what a program must speak if it names no shim)

* `FANTASTIC_CTRL_FD` — an inherited socket fd, already open.
* `FANTASTIC_SELF_ID` — the OWNER agent's id (the tool's public id, not the job id).
* `FANTASTIC_ARGS`   — this call's `args`, as JSON.
* Framing, both directions: `[4-byte big-endian length | JSON body]`.
* Child → parent: `{op:"send", rid, target, payload}` · `{op:"emit", target?, payload}` ·
  `{op:"watch", src}` · `{op:"unwatch", src}` · `{op:"onmessage"}` · `{op:"offmessage"}`
* Parent → child: `{op:"reply", rid, data}` · `{op:"event", src, payload}` ·
  `{op:"inbox", payload}`
* `rid` correlates a `send` with its `reply`; the parent echoes it back untouched.
* EOF on the fd means the kernel is GONE — exit, or become an orphan nothing can reach.
"""

from __future__ import annotations

# The python shim IS the connector — ONE wire client for one protocol. It moved here
# when the per-language executor was deleted (2026-08-07); the rust/swift byte-equality
# tests now point at THIS file.
from exec_bridge._connector import CONNECTOR_SRC as _PYTHON_SHIM

# name → source prepended to the program. One entry per LANGUAGE, not per interpreter:
# every CPython on an allowlist shares this client.
SHIMS: dict[str, str] = {"python": _PYTHON_SHIM}


def names() -> list[str]:
    """The shim names this bridge can serve — what `reflect` advertises."""
    return sorted(SHIMS)


def apply(shim: str | None, program: str) -> str:
    """`program`, with the named shim prepended. `None` ⇒ untouched (raw wire).

    An unknown name is a REFUSAL, not a silent pass-through to the raw wire: a program
    that asked for `kernel` and did not get it fails later, somewhere else, with a
    `NameError` that names nothing useful. Refusing here, with the legal set, puts the
    error next to the mistake.
    """
    if shim is None or not str(shim).strip():
        return program
    src = SHIMS.get(str(shim))
    if src is None:
        raise KeyError(f"unknown shim {shim!r} — this bridge serves {names()}")
    return src + "\n" + program
