"""cli singleton — terminal renderer.

Receives `token`, `done`, `say`, `error`, `status` payloads. Prints to stdout.
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
import sys


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + accepted event types. No args."""
    return {
        "id": id,
        "sentence": "Terminal renderer. Prints incoming events to stdout.",
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "accepts": ["token", "done", "say", "error", "status"],
    }


async def _token(id, payload, kernel):
    """args: text:str. Writes to stdout (no newline). Returns None."""
    sys.stdout.write(payload.get("text", ""))
    sys.stdout.flush()
    return None


async def _done(id, payload, kernel):
    """args: none. Emits a single newline; closes a token-stream cleanly. Returns None."""
    print()
    return None


async def _say(id, payload, kernel):
    """args: text:str, source:str?. Prints `  [source] text` on its own line. Returns None."""
    text = payload.get("text", "")
    src = payload.get("source", "")
    prefix = f"  [{src}] " if src else "  "
    print(f"{prefix}{text}")
    return None


async def _error(id, payload, kernel):
    """args: text:str. Prints `  ERROR: text` on its own line. Returns None."""
    print(f"  ERROR: {payload.get('text', '')}")
    return None


async def _status(id, payload, kernel):
    """args: phase:str, source:str?, detail:dict?. Renders one-line phase markers for queued / thinking / tool_calling (entry+exit). `streaming` and `done` produce no output (token + done handlers cover them). Returns None."""
    phase = payload.get("phase", "")
    src = payload.get("source", "")
    detail = payload.get("detail") or {}
    prefix = f"  [{src}]" if src else " "
    if phase == "queued":
        ahead = detail.get("ahead", 0)
        print(f"{prefix} queued ({ahead} ahead)")
    elif phase == "thinking":
        if detail.get("waiting_on") == "rate_limit":
            print(f"{prefix} rate-limited; waiting {detail.get('wait_s', '?')}s")
        else:
            print(f"{prefix} thinking…")
    elif phase == "tool_calling":
        tool = detail.get("tool") or {}
        verb = tool.get("verb", "")
        target = tool.get("target", "")
        if "reply_preview" in tool:
            # exit
            preview = tool.get("reply_preview", "")
            if len(preview) > 80:
                preview = preview[:80] + "…"
            print(f"{prefix} ← {verb}({target})  {preview}")
        else:
            # entry — args summary on the line
            args = tool.get("args") or {}
            args_str = json.dumps(args, default=str)
            if len(args_str) > 80:
                args_str = args_str[:80] + "…"
            print(f"{prefix} → {verb}({target})  {args_str}")
    # `streaming` and `done` are silent — handled by other verbs.
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
    print(f"[fantastic] {_identity(ctx)} — booting…")
    print(
        '  one envelope: send(<id>, {"type":"<verb>", …})'
        "   ·   kernel = root   ·   full map: reflect readme=true"
    )
    print(
        "  PULL  ask → reply        REST POST /<rest>/<id>        ·  this REPL: @<id> <verb> k=v"
    )
    print(
        "  PUSH  async stream/emit  WS /<id>/ws : watch{src} · emit{target,payload} · state_subscribe"
    )
    print(
        "  REACH one call by id, any unit: "
        "compute(python_runtime) · infer(ai) · memory(yaml_state) · shell(terminal_backend)"
    )
    return None


async def _booted(id, payload, kernel):
    """Final PTY push — the kernel's "all booted" signal, sent AFTER the boot
    loop. cli is a DUMB SINK: it does NOT inspect the tree for ports/surfaces.
    Each agent announces its OWN endpoints to cli during its boot (e.g. `web`
    sends a `say` with its listening URL), so this just closes the intro. Live
    coordinates therefore arrive between `intro_booting` and here, or — if no
    renderer / a race — they are in the map above + `reflect readme=true`.
    No args. Returns None."""
    print("[kernel] up — all booted. attach via the map above, or reflect readme=true")
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "token": _token,
    "done": _done,
    "say": _say,
    "error": _error,
    "status": _status,
    "intro_booting": _intro_booting,
    "booted": _booted,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return None  # cli is silent on unknown verbs
    return await fn(id, payload, kernel)
