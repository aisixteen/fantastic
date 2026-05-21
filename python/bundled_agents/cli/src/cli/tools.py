"""cli singleton — terminal renderer.

Receives `token`, `done`, `say`, `error`, `status` payloads. Prints to stdout.
Unknown verbs are silently dropped (cli is a render sink).
"""

from __future__ import annotations

import json
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


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "token": _token,
    "done": _done,
    "say": _say,
    "error": _error,
    "status": _status,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    fn = VERBS.get(payload.get("type"))
    if fn is None:
        return None  # cli is silent on unknown verbs
    return await fn(id, payload, kernel)
