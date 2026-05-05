"""terminal bundle — PTY shell session as an agent.

State is in-memory only (one PTY per agent). Output is emitted as
`{type:"output", data:"..."}` events on the agent's own inbox so a
browser tab watching the agent receives the stream live.

Verbs:
  reflect   -> {sentence, command, running, scrollback_bytes, cols, rows}
  boot      -> spawn the PTY if not running (called by kernel boot fanout)
  write     args: data                  -> write bytes to PTY stdin
  shell     args: cmd, timeout?         -> run via done-token, return output
  resize    args: cols, rows            -> SIGWINCH
  output    args: max_bytes?            -> {output: <scrollback>}
  restart                                -> kill + respawn with same params
  signal    args: signal? (default 2)   -> os.kill(pid, signal)
  stop                                   -> kill the PTY
"""

from __future__ import annotations

import asyncio
import collections
import fcntl
import logging
import os
import pty
import secrets
import signal as signal_mod
import struct
import termios

logger = logging.getLogger(__name__)

MAX_SCROLLBACK = 256 * 1024
DEFAULT_COLS = 200
DEFAULT_ROWS = 50

# Per-agent runtime state (NOT persisted)
_procs: dict[str, dict] = {}

# Pending done-tokens per agent: {agent_id: {token: asyncio.Event}}
_pending_tokens: dict[str, dict[str, asyncio.Event]] = {}


def _detect_shell() -> str:
    sh = os.environ.get("SHELL")
    if sh and os.path.isfile(sh):
        return sh
    for cand in ("/bin/zsh", "/bin/bash", "/bin/sh"):
        if os.path.isfile(cand):
            return cand
    return "/bin/sh"


def _set_winsize(fd: int, cols: int, rows: int) -> None:
    try:
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    except OSError:
        pass


def _append_scrollback(state: dict, data: str) -> None:
    buf: collections.deque[str] = state["scrollback"]
    buf.append(data)
    state["scrollback_bytes"] += len(data.encode("utf-8", errors="replace"))
    while state["scrollback_bytes"] > MAX_SCROLLBACK and buf:
        old = buf.popleft()
        state["scrollback_bytes"] -= len(old.encode("utf-8", errors="replace"))


def _on_readable(agent_id: str, kernel) -> None:
    state = _procs.get(agent_id)
    if not state:
        return
    fd = state["fd"]
    try:
        data = os.read(fd, 4096)
    except (OSError, BlockingIOError):
        return
    if not data:
        # EOF — child exited
        _detach_reader(agent_id)
        asyncio.create_task(kernel.emit(agent_id, {"type": "closed"}))
        _cleanup(agent_id)
        return
    text = data.decode("utf-8", errors="replace")
    _append_scrollback(state, text)
    # Signal any pending done-tokens. We look for `<token>\r\n` —
    # which the printf produces, but the command echo line does NOT
    # (the echo has `<token>'` from the quote, not <token>\r\n).
    if agent_id in _pending_tokens:
        full = _scrollback_text(agent_id)
        for tok, ev in list(_pending_tokens[agent_id].items()):
            if (tok + "\r\n") in full or (tok + "\n") in full:
                ev.set()
    asyncio.create_task(kernel.emit(agent_id, {"type": "output", "data": text}))


def _attach_reader(agent_id: str, kernel) -> None:
    state = _procs.get(agent_id)
    if not state or state.get("reader_attached"):
        return
    loop = asyncio.get_event_loop()
    fd = state["fd"]
    os.set_blocking(fd, False)
    loop.add_reader(fd, _on_readable, agent_id, kernel)
    state["reader_attached"] = True


def _detach_reader(agent_id: str) -> None:
    state = _procs.get(agent_id)
    if not state or not state.get("reader_attached"):
        return
    try:
        loop = asyncio.get_event_loop()
        loop.remove_reader(state["fd"])
    except (RuntimeError, OSError, ValueError):
        pass
    state["reader_attached"] = False


def _spawn(agent_id: str, kernel) -> None:
    if agent_id in _procs:
        return
    rec = kernel.get(agent_id) or {}
    cmd = rec.get("command") or _detect_shell()
    args = rec.get("args") or []
    cwd = rec.get("cwd") or os.getcwd()
    cols = int(rec.get("cols") or DEFAULT_COLS)
    rows = int(rec.get("rows") or DEFAULT_ROWS)

    pid, fd = pty.fork()
    if pid == 0:
        # child
        try:
            os.chdir(cwd)
        except OSError:
            pass
        env = os.environ.copy()
        env["TERM"] = "xterm-256color"
        os.execvpe(cmd, [cmd, *args], env)
    # parent
    _set_winsize(fd, cols, rows)
    state = {
        "pid": pid,
        "fd": fd,
        "cmd": cmd,
        "args": args,
        "cwd": cwd,
        "cols": cols,
        "rows": rows,
        "scrollback": collections.deque(),
        "scrollback_bytes": 0,
    }
    _procs[agent_id] = state
    _attach_reader(agent_id, kernel)


def _cleanup(agent_id: str) -> None:
    _detach_reader(agent_id)
    state = _procs.pop(agent_id, None)
    if not state:
        return
    try:
        os.close(state["fd"])
    except OSError:
        pass
    try:
        os.kill(state["pid"], signal_mod.SIGKILL)
    except OSError:
        pass


def _scrollback_text(agent_id: str) -> str:
    s = _procs.get(agent_id)
    if not s:
        return ""
    return "".join(s["scrollback"])


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + PTY state. No args. `running`/`scrollback_bytes` are process-local — read via the live serve to get truth."""
    state = _procs.get(id)
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "PTY shell session.",
        "command": (state or {}).get("cmd") or rec.get("command") or _detect_shell(),
        "running": id in _procs,
        "cols": (state or {}).get("cols", rec.get("cols", DEFAULT_COLS)),
        "rows": (state or {}).get("rows", rec.get("rows", DEFAULT_ROWS)),
        "scrollback_bytes": (state or {}).get("scrollback_bytes", 0),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "output": "{type:'output', data:str} — every PTY read chunk; UTF-8-decoded bytes",
            "closed": "{type:'closed'} — child process exited (EOF on the PTY fd)",
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Spawns the PTY child if not already running. Returns {running:true}."""
    _spawn(id, kernel)
    return {"running": True}


async def _write(id, payload, kernel):
    """args: data:str (req). Writes UTF-8 bytes to PTY stdin (no synchronous wait). Returns {written:int}."""
    state = _procs.get(id)
    if not state:
        return {"error": "not running"}
    data = payload.get("data", "")
    try:
        os.write(state["fd"], data.encode("utf-8"))
    except OSError as e:
        return {"error": str(e)}
    return {"written": len(data)}


async def _resize(id, payload, kernel):
    """args: cols:int, rows:int. Fires SIGWINCH so TUI apps redraw. Returns {resized, cols, rows}."""
    state = _procs.get(id)
    if not state:
        return {"error": "not running"}
    cols = int(payload.get("cols", DEFAULT_COLS))
    rows = int(payload.get("rows", DEFAULT_ROWS))
    state["cols"], state["rows"] = cols, rows
    _set_winsize(state["fd"], cols, rows)
    return {"resized": True, "cols": cols, "rows": rows}


async def _output(id, payload, kernel):
    """args: max_bytes:int? (default MAX_SCROLLBACK=256K). Returns {output:str} — tail of the PTY scrollback."""
    max_bytes = int(payload.get("max_bytes", MAX_SCROLLBACK))
    text = _scrollback_text(id)
    if max_bytes < len(text.encode("utf-8")):
        text = text[-max_bytes:]
    return {"output": text}


async def _restart(id, payload, kernel):
    """No args. SIGKILLs the PTY child and respawns with the same command/cwd/cols/rows. Returns {restarted:true}."""
    _cleanup(id)
    _spawn(id, kernel)
    return {"restarted": True}


async def _signal(id, payload, kernel):
    """args: signal:int? (default SIGINT=2). os.kill(pid, signal). Returns {signal:int} or {error}."""
    state = _procs.get(id)
    if not state:
        return {"error": "not running"}
    sig = int(payload.get("signal", signal_mod.SIGINT))
    try:
        os.kill(state["pid"], sig)
    except OSError as e:
        return {"error": str(e)}
    return {"signal": sig}


async def _stop(id, payload, kernel):
    """No args. Closes the PTY fd and SIGKILLs the child. Returns {stopped:true}."""
    _cleanup(id)
    return {"stopped": True}


async def _shutdown(id, payload, kernel):
    """No args. Lifecycle hook called by core.delete_agent before record removal — closes the PTY fd and SIGKILLs the child so the subprocess doesn't outlive its agent record (orphan PTYs would keep emitting output to a dead inbox, leaking sprites in telemetry views). Returns {shutdown:true}."""
    _cleanup(id)
    return {"shutdown": True}


async def _shell(id, payload, kernel):
    """args: cmd:str (req), timeout:float? (default 30s). Synchronous run via done-token; returns {cmd, output, completed:bool, error?}."""
    state = _procs.get(id)
    if not state:
        return {"error": "shell: not running; call boot first"}
    cmd = payload.get("cmd", "")
    if not cmd:
        return {"error": "shell: cmd required"}
    timeout = float(payload.get("timeout", 30.0))

    token = f"__DONE_{secrets.token_hex(8)}__"
    pending = _pending_tokens.setdefault(id, {})
    event = asyncio.Event()
    pending[token] = event
    try:
        before_text = _scrollback_text(id)
        line = f"{cmd}; printf '\\n%s\\n' '{token}'\n"
        try:
            os.write(state["fd"], line.encode("utf-8"))
        except OSError as e:
            return {"error": str(e)}
        try:
            await asyncio.wait_for(event.wait(), timeout=timeout)
            completed = True
        except asyncio.TimeoutError:
            completed = False
            # Cmd is still running — send Ctrl-C so the PTY shell is
            # ready for the next call. Otherwise any subsequent shell
            # verb queues behind this one.
            try:
                os.write(state["fd"], b"\x03")
            except OSError:
                pass
            # Best-effort: wait briefly for the token to land after
            # the interrupt; if the cmd did print it, capture it.
            try:
                await asyncio.wait_for(event.wait(), timeout=0.5)
            except asyncio.TimeoutError:
                pass
        after_text = _scrollback_text(id)
        delta = (
            after_text[len(before_text) :]
            if after_text.startswith(before_text)
            else after_text
        )
        # The token appears TWICE: once in the echoed command line
        # (followed by `'`), once as printf's output (followed by
        # \r\n). We match the second one and slice before it.
        needle_crlf = token + "\r\n"
        needle_lf = token + "\n"
        tok_idx = delta.find(needle_crlf)
        if tok_idx < 0:
            tok_idx = delta.find(needle_lf)
        if tok_idx >= 0:
            # Walk back to the \n that starts the printf's leading newline.
            line_start = delta.rfind("\n", 0, tok_idx)
            output = delta[: line_start if line_start >= 0 else tok_idx].rstrip("\r\n")
        else:
            output = delta
        if not completed:
            return {
                "cmd": cmd,
                "output": output,
                "completed": False,
                "error": "timeout",
            }
        return {"cmd": cmd, "output": output, "completed": True}
    finally:
        pending.pop(token, None)
        if not pending:
            _pending_tokens.pop(id, None)


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "write": _write,
    "shell": _shell,
    "resize": _resize,
    "output": _output,
    "restart": _restart,
    "signal": _signal,
    "stop": _stop,
    "shutdown": _shutdown,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"terminal: unknown type {t!r}"}
    return await fn(id, payload, kernel)
