"""terminal bundle — PTY shell session as an agent.

State is in-memory only (one PTY per agent). Output is emitted as
`{type:"output", data:"..."}` events on the agent's own inbox so a
browser tab watching the agent receives the stream live.

Verbs:
  reflect   -> {sentence, command, running, scrollback_bytes, cols, rows,
                paused, unacked}
  boot      -> spawn the PTY if not running (called by kernel boot fanout)
  write     args: data                  -> write bytes to PTY stdin
  paste_image args: data:bytes, mime?   -> save image, type its path into PTY
  shell     args: cmd, timeout?         -> run via done-token, return output
  resize    args: cols, rows            -> SIGWINCH
  output    args: max_bytes?            -> {output: <scrollback>}
  ack       args: chars                 -> flow-control ack from a streamer
  restart                                -> kill + respawn with same params
  signal    args: signal? (default 2)   -> os.kill(pid, signal)
  stop                                   -> kill the PTY

Flow control (mirrors VSCode's integrated terminal): the streaming
output path is backpressured. `_on_readable` counts every char it
emits; once more than HIGH_WATERMARK chars sit unacknowledged the PTY
reader is detached ("paused") so the kernel PTY buffer fills and the
shell naturally throttles. A streaming consumer (terminal_webapp) acks
each chunk AFTER xterm has parsed it via the `ack` verb; once the
backlog drains below LOW_WATERMARK the reader re-attaches. Without it
a flood of output (a pasted script that runs, `cat bigfile`) piles
unbounded emit tasks onto the loop and the tab locks up. The
synchronous `shell` verb drains via scrollback, not the emit stream —
it is exempt (no pause while a done-token is pending; flow state is
cleared around each call).
"""

from __future__ import annotations

import asyncio
import codecs
import collections
import fcntl
import logging
import os
import pty
import secrets
import shutil
import signal as signal_mod
import struct
import tempfile
import termios

logger = logging.getLogger(__name__)

MAX_SCROLLBACK = 256 * 1024
DEFAULT_COLS = 200
DEFAULT_ROWS = 50

# Flow control — VSCode's FlowControlConstants, ported. Pause the PTY
# reader once this many emitted chars sit unacknowledged by the
# streaming consumer; resume once the consumer's acks drain the
# backlog below the low-water mark.
HIGH_WATERMARK = 100_000
LOW_WATERMARK = 5_000

# Image paste — the formats Claude Code accepts, keyed by MIME, and
# its per-image size cap. A browser xterm can't hand a server-side
# CLI an image from the browser clipboard, so `paste_image` bridges
# it: save the bytes, type the path (mimics a file drag-drop).
_IMAGE_EXT = {
    "image/png": "png",
    "image/jpeg": "jpg",
    "image/gif": "gif",
    "image/webp": "webp",
}
MAX_PASTE_IMAGE = 5 * 1024 * 1024

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
    # Incremental UTF-8 decode — `os.read` slices the stream at fixed
    # byte boundaries, so a multi-byte char (box-drawing glyphs in any
    # TUI redraw are 3 bytes) routinely straddles two reads. A naive
    # per-chunk `decode` turns the split char into 2-3 replacement
    # chars: that's the `<?>` litter AND, because one cell becomes
    # three, the column-shift "line breaks" on resize. The incremental
    # decoder buffers the partial tail until the next read completes
    # it — exactly what node-pty does for VSCode's terminal.
    text = state["decoder"].decode(data)
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
    # Flow control: account the emitted chars and pause the reader if
    # the streaming consumer is falling behind. A `shell` call in
    # flight drains via scrollback (not the emit stream) and never
    # acks — don't pause under it.
    state["unacked"] += len(text)
    if (
        not _pending_tokens.get(agent_id)
        and not state["paused"]
        and state["unacked"] > HIGH_WATERMARK
    ):
        state["paused"] = True
        _detach_reader(agent_id)


async def _write_all(fd: int, buf: bytes) -> int:
    """Write the FULL buffer to the non-blocking PTY fd. `os.write` on
    a non-blocking fd short-writes — it returns however many bytes the
    PTY input buffer could take and the caller is responsible for the
    rest — and raises `BlockingIOError` when the buffer is full. A
    single `os.write()` that ignores the count silently drops the tail
    of anything bigger than the buffer: that's exactly how a paste
    ends up truncated mid-escape-sequence (the bracketed-paste `\\e[201~`
    end marker dropped), leaving the shell stuck in paste mode and the
    terminal apparently dead. Loop until every byte lands; await
    fd-writable when the buffer is full."""
    loop = asyncio.get_event_loop()
    written = 0
    while written < len(buf):
        try:
            written += os.write(fd, buf[written:])
        except BlockingIOError:
            ev = asyncio.Event()
            loop.add_writer(fd, ev.set)
            try:
                await ev.wait()
            finally:
                loop.remove_writer(fd)
    return written


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


def _clear_flow_control(agent_id: str, kernel) -> None:
    """Reset flow-control accounting and force-resume the reader. A
    fresh streaming consumer (terminal_webapp mounting → `boot`) or a
    synchronous `shell` call has no relationship to a previous
    consumer's unacked backlog — stale counts would otherwise leave
    the reader paused with nobody left to ack it. VSCode's
    `clearUnacknowledgedChars`, ported."""
    state = _procs.get(agent_id)
    if not state:
        return
    state["unacked"] = 0
    if state["paused"]:
        state["paused"] = False
        _attach_reader(agent_id, kernel)


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
        # Incremental UTF-8 decoder — buffers a partial multi-byte
        # char across `os.read` boundaries (see _on_readable). One
        # per PTY lifetime; the byte stream is continuous within a
        # session.
        "decoder": codecs.getincrementaldecoder("utf-8")(errors="replace"),
        # Flow control
        "unacked": 0,
        "paused": False,
        # Serializes writes to the PTY fd. Two concurrent `_write_all`
        # loops on one non-blocking fd would race on `add_writer` AND
        # interleave bytes — fatal for a bracketed-paste sequence.
        "write_lock": asyncio.Lock(),
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
    # Reap so the child isn't left as a zombie (`kill -0 <pid>` keeps
    # reporting zombies as alive otherwise). Best-effort.
    try:
        os.waitpid(state["pid"], 0)
    except (OSError, ChildProcessError):
        pass
    # Drop the per-agent pasted-image scratch dir, if one was created.
    paste_dir = state.get("paste_dir")
    if paste_dir:
        shutil.rmtree(paste_dir, ignore_errors=True)


def _scrollback_text(agent_id: str) -> str:
    s = _procs.get(agent_id)
    if not s:
        return ""
    return "".join(s["scrollback"])


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + PTY state. No args. `running`/`scrollback_bytes`/`paused`/`unacked` are process-local — read via the live serve to get truth."""
    state = _procs.get(id)
    rec = kernel.get(id) or {}
    return {
        "id": id,
        "sentence": "PTY shell session.",
        "command": (state or {}).get("cmd") or rec.get("command") or _detect_shell(),
        "running": id in _procs,
        "pid": (state or {}).get("pid"),
        "cols": (state or {}).get("cols", rec.get("cols", DEFAULT_COLS)),
        "rows": (state or {}).get("rows", rec.get("rows", DEFAULT_ROWS)),
        "scrollback_bytes": (state or {}).get("scrollback_bytes", 0),
        "paused": (state or {}).get("paused", False),
        "unacked": (state or {}).get("unacked", 0),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
        "emits": {
            "output": "{type:'output', data:str} — every PTY read chunk; UTF-8-decoded bytes",
            "closed": "{type:'closed'} — child process exited (EOF on the PTY fd)",
        },
    }


async def _boot(id, payload, kernel):
    """Idempotent. Spawns the PTY child if not already running; clears stale flow-control state so a freshly-mounted streamer starts clean. Returns {running:true}."""
    _spawn(id, kernel)
    _clear_flow_control(id, kernel)
    return {"running": True}


async def _write(id, payload, kernel):
    """args: data:str (req). Writes UTF-8 bytes to PTY stdin IN FULL — loops over the non-blocking fd so large pastes aren't truncated; serialized per-agent so concurrent writes can't interleave a bracketed-paste sequence. Returns {written:int}."""
    state = _procs.get(id)
    if not state:
        return {"error": "not running"}
    data = payload.get("data", "")
    try:
        async with state["write_lock"]:
            written = await _write_all(state["fd"], data.encode("utf-8"))
    except OSError as e:
        return {"error": str(e)}
    return {"written": written}


async def _ack(id, payload, kernel):
    """args: chars:int. Flow-control ack from a streaming consumer (terminal_webapp acks each chunk after xterm parses it). Decrements the unacked-char count; re-attaches the PTY reader once it drops below LOW_WATERMARK. Returns {unacked:int, paused:bool}."""
    state = _procs.get(id)
    if not state:
        return {"error": "not running"}
    chars = int(payload.get("chars", 0))
    state["unacked"] = max(0, state["unacked"] - chars)
    if state["paused"] and state["unacked"] < LOW_WATERMARK:
        state["paused"] = False
        _attach_reader(id, kernel)
    return {"unacked": state["unacked"], "paused": state["paused"]}


async def _paste_image(id, payload, kernel):
    """args: data:bytes (req), mime:str? (default image/png). Saves a pasted image to a per-agent scratch file and types its absolute path into the PTY. Bridges image paste for a CLI (e.g. claude) running in a browser xterm — the server can't reach the browser clipboard, so path injection mimics a file drag-drop. Returns {path:str, bytes:int}."""
    state = _procs.get(id)
    if not state:
        return {"error": "not running"}
    data = payload.get("data")
    if not isinstance(data, (bytes, bytearray)):
        return {"error": "paste_image: data must be bytes"}
    if len(data) > MAX_PASTE_IMAGE:
        return {"error": f"paste_image: {len(data)} bytes exceeds the 5 MB cap"}
    mime = (payload.get("mime") or "image/png").lower()
    ext = _IMAGE_EXT.get(mime)
    if ext is None:
        return {"error": f"paste_image: unsupported image type {mime!r}"}
    # Per-agent scratch dir, created lazily — most terminals never
    # paste an image, so don't mint a temp dir for every spawn.
    paste_dir = state.get("paste_dir")
    if not paste_dir:
        paste_dir = tempfile.mkdtemp(prefix=f"fantastic_paste_{id}_")
        state["paste_dir"] = paste_dir
    path = os.path.join(paste_dir, f"paste_{secrets.token_hex(4)}.{ext}")
    try:
        with open(path, "wb") as f:
            f.write(data)
    except OSError as e:
        return {"error": str(e)}
    # Type the path at the PTY cursor with a trailing space (no
    # newline — pasting an image must not submit; it mirrors a
    # drag-drop, leaving the user to type their prompt and hit enter).
    try:
        async with state["write_lock"]:
            await _write_all(state["fd"], (path + " ").encode("utf-8"))
    except OSError as e:
        return {"error": str(e)}
    return {"path": path, "bytes": len(data)}


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


async def on_delete(agent):
    """Cascade hook — invoked by the substrate during cascade-delete
    BEFORE the agent's disk artifact is removed. Closes the PTY fd
    and SIGKILLs the child so the subprocess doesn't outlive its
    agent record (orphan PTYs would keep emitting output to a dead
    inbox, leaking sprites in telemetry views)."""
    _cleanup(agent.id)


async def _shell(id, payload, kernel):
    """args: cmd:str (req), timeout:float? (default 30s). Synchronous run via done-token; returns {cmd, output, completed:bool, error?}."""
    state = _procs.get(id)
    if not state:
        return {"error": "shell: not running; call boot first"}
    cmd = payload.get("cmd", "")
    if not cmd:
        return {"error": "shell: cmd required"}
    timeout = float(payload.get("timeout", 30.0))

    # `shell` drains via scrollback, not the emit stream — it can't be
    # left at the mercy of a streamer's stale flow-control pause (a
    # detached reader means the done-token never lands → timeout).
    # Clear it on the way in, and again on the way out so this call's
    # own output doesn't strand the reader paused for the next streamer.
    _clear_flow_control(id, kernel)
    token = f"__DONE_{secrets.token_hex(8)}__"
    pending = _pending_tokens.setdefault(id, {})
    event = asyncio.Event()
    pending[token] = event
    try:
        before_text = _scrollback_text(id)
        line = f"{cmd}; printf '\\n%s\\n' '{token}'\n"
        try:
            async with state["write_lock"]:
                await _write_all(state["fd"], line.encode("utf-8"))
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
        # This call's output never went through the ack stream — drop
        # the backlog so it can't pause the reader for the next streamer.
        _clear_flow_control(id, kernel)


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "write": _write,
    "ack": _ack,
    "paste_image": _paste_image,
    "shell": _shell,
    "resize": _resize,
    "output": _output,
    "restart": _restart,
    "signal": _signal,
    "stop": _stop,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"terminal: unknown type {t!r}"}
    return await fn(id, payload, kernel)
