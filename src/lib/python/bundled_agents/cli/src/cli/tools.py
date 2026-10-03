"""cli singleton — terminal renderer.

Receives `token`, `done`, `say`, `error`, `status`, `context` payloads. Prints to stdout.
Unknown verbs are silently dropped (cli is a render sink).

Renders the PTY intro too, but as a DUMB SINK — it prints what it is told and
NEVER inspects the tree. The kernel/agents push to it:
  - `intro_booting` (kernel → cli, before boot): identity + the pull/push
    control-plane map (port-independent).
  - each agent announces its OWN endpoints during its boot (e.g. `web` sends a
    `say` with its listening URL) — the producer owns the info, not this sink.
  - `booted` (kernel → cli, after the boot loop): the "all booted" close.
`longrun` fires `intro_booting` / `booted` only when stdin is a tty. Best-effort:
no renderer or a race is fine — the full map is in the intro + `reflect
readme=true`. The kernel stays decoupled (it sends verbs; it never imports this).
"""

from __future__ import annotations

import json
import os
import re
import sys

# C0 and C1 control characters (with DEL), minus newline and tab: what a terminal
# INTERPRETS instead of printing — an OSC title or clipboard write, a cursor move,
# a line erase. Agent and model text reaches this sink from callers it does not
# know, so it never reaches the terminal with them (audit BV-11).
_CONTROL = re.compile(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]")


def _write(text) -> None:
    """THE one write to the terminal, its control characters stripped. A non-str
    reaches `write` as is, so it raises exactly what `sys.stdout.write` raises."""
    sys.stdout.write(_CONTROL.sub("", text) if isinstance(text, str) else text)


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + accepted event types. No args."""
    return {
        "id": id,
        "sentence": "Terminal renderer. Prints incoming events to stdout.",
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "accepts": ["token", "done", "say", "error", "status", "context"],
    }


async def _token(id, payload, kernel):
    """args: text:str. Writes to stdout (no newline). Returns None."""
    _write(payload.get("text", ""))
    sys.stdout.flush()
    return None


async def _done(id, payload, kernel):
    """args: none. Emits a single newline; closes a token-stream cleanly. Returns None."""
    _write("\n")
    return None


async def _say(id, payload, kernel):
    """args: text:str, source:str?. Prints `  [source] text` on its own line. Returns None."""
    text = payload.get("text", "")
    src = payload.get("source", "")
    prefix = f"  [{src}] " if src else "  "
    _write(f"{prefix}{text}\n")
    return None


async def _error(id, payload, kernel):
    """args: text:str. Prints `  ERROR: text` on its own line. Returns None."""
    _write(f"  ERROR: {payload.get('text', '')}\n")
    return None


async def _status(id, payload, kernel):
    """args: phase:str, source:str?, detail:dict?. Renders one-line phase markers for queued / thinking / tool_calling (entry+exit). `streaming` and `done` produce no output (token + done handlers cover them). Returns None."""
    phase = payload.get("phase", "")
    src = payload.get("source", "")
    detail = payload.get("detail") or {}
    prefix = f"  [{src}]" if src else " "
    if phase == "queued":
        ahead = detail.get("ahead", 0)
        _write(f"{prefix} queued ({ahead} ahead)\n")
    elif phase == "thinking":
        _write(f"{prefix} thinking…\n")
    elif phase == "tool_calling":
        tool = detail.get("tool") or {}
        verb = tool.get("verb", "")
        target = tool.get("target", "")
        if "reply_preview" in tool:
            # exit
            preview = tool.get("reply_preview", "")
            _write(f"{prefix} ← {verb}({target})  {preview}\n")
        else:
            # entry — args summary on the line
            args = tool.get("args") or {}
            args_str = json.dumps(args, default=str)
            _write(f"{prefix} → {verb}({target})  {args_str}\n")
    # `streaming` and `done` are silent — handled by other verbs.
    return None


async def _context(id, payload, kernel):
    """args: phase:str ('compacted'|'too_small'), source:str?, detail:dict?. Renders a one-line context-protocol notice.
    `⟲ context compacted (strategy, N dropped)` or
    `✗ context too small — <hint>`. Returns None."""
    phase = payload.get("phase", "")
    src = payload.get("source", "")
    detail = payload.get("detail") or {}
    prefix = f"  [{src}]" if src else " "
    if phase == "compacted":
        strat = detail.get("strategy", "?")
        dropped = detail.get("dropped_messages", 0)
        _write(
            f"{prefix} ⟲ context compacted ({strat}, {dropped} dropped; recall to page back)\n"
        )
    elif phase == "too_small":
        _write(f"{prefix} ✗ context too small — {detail.get('hint', '')}\n")
    return None


# ─── two-phase PTY intro (first contact) ────────────────────────


def _identity(ctx) -> str:
    """`python · env=<…> · v<…>? · root=<…> · pid <…>` — the same deployment
    context the root reflect carries, rendered for the terminal."""
    env = os.environ.get("FANTASTIC_ENV", "host")
    ver = os.environ.get("FANTASTIC_VERSION")
    root = ctx.root.id if ctx.root is not None else "kernel_state"
    parts = ["python", f"env={env}"]
    if ver:
        parts.append(ver)
    parts += [f"root={root}", f"pid {os.getpid()}"]
    return " · ".join(parts)


async def _intro_booting(id, payload, kernel):
    """First PTY push, BEFORE boot. Identity + the pull/push control-plane map — port-independent, so it prints instantly. No args. Returns None."""
    ctx = kernel.ctx
    _write(f"[fantastic] {_identity(ctx)} — booting…\n")
    _write(
        '  one envelope: send(<id>, {"type":"<verb>", …})'
        "   ·   kernel = root   ·   full map: reflect readme=true\n"
    )
    _write(
        "  PULL  ask → reply        REST POST /<rest>/<id>        ·  this REPL: @<id> <verb> k=v\n"
    )
    _write(
        "  PUSH  async stream/emit  WS /<id>/ws : watch{src} · emit{target,payload} · state_subscribe\n"
    )
    _write(
        "  REACH one call by id, any unit: "
        "compute(exec_bridge) · infer(ai) · memory(file_bridge) · shell(exec_bridge pty)\n"
    )
    return None


async def _booted(id, payload, kernel):
    """Final PTY push — the kernel's "all booted" signal, sent AFTER the boot loop.
    cli is a DUMB SINK: it does NOT inspect the tree for ports/surfaces.
    Each agent announces its OWN endpoints to cli during its boot (e.g. `web`
    sends a `say` with its listening URL), so this just closes the intro. Live
    coordinates therefore arrive between `intro_booting` and here, or — if no
    renderer / a race — they are in the map above + `reflect readme=true`.
    No args. Returns None."""
    _write(
        "[kernel] up — all booted. attach via the map above, or reflect readme=true\n"
    )
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "token": _token,
    "done": _done,
    "say": _say,
    "error": _error,
    "status": _status,
    "context": _context,
    "intro_booting": _intro_booting,
    "booted": _booted,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return None  # cli is silent on unknown verbs
    return await fn(id, payload, kernel)
