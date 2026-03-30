"""
ProcessRunner — manages optional subprocess per agent. PTY mode.

Each agent can have at most one running process.
Output is dispatched via a callback so the server can broadcast to WS clients.
An in-memory scrollback buffer is kept per process so frontends can reconnect after reload.
"""

import asyncio
import atexit
import collections
import logging
import os
import pty
import signal
import struct
import fcntl
import termios
from pathlib import Path
from typing import Any, Callable, Awaitable

logger = logging.getLogger(__name__)

# Track all child PIDs globally so we can kill them on exit
_child_pids: set[int] = set()


def _kill_all_children():
    """Kill all child processes. Called on exit/signal."""
    for pid in list(_child_pids):
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    _child_pids.clear()


# Register cleanup for normal exit
atexit.register(_kill_all_children)


# Register cleanup for SIGTERM/SIGINT (uvicorn reload sends SIGTERM)
def _signal_handler(signum, frame):
    _kill_all_children()
    # Re-raise so the process actually exits
    signal.signal(signum, signal.SIG_DFL)
    os.kill(os.getpid(), signum)


signal.signal(signal.SIGTERM, _signal_handler)
signal.signal(signal.SIGINT, _signal_handler)

# Max bytes to keep in scrollback buffer per process
MAX_SCROLLBACK = 256 * 1024  # 256 KB


class ProcessRunner:
    """Manages optional subprocess per agent. PTY mode (shell sessions)."""

    def __init__(
        self,
        on_output: Callable[[str, str], Awaitable[None]] | None = None,
        agents_dir: Path | None = None,
    ):
        """
        Args:
            on_output: async callback(agent_id, data) called when process produces output
            agents_dir: path to .fantastic/agents/ directory for persisting scrollback to disk
        """
        self._processes: dict[str, dict[str, Any]] = {}
        self._on_output = on_output
        self._agents_dir = agents_dir
        # In-memory scrollback buffer per process (deque of strings, capped by byte size)
        self._scrollback: dict[str, collections.deque[str]] = {}
        self._scrollback_bytes: dict[str, int] = {}
        # Dirty tracking for periodic disk flush
        self._scrollback_dirty: set[str] = set()
        self._flush_task: asyncio.Task | None = None

    @staticmethod
    def _detect_shell() -> str:
        """Detect the best available shell for the current system."""
        # 1. SHELL env var (user's configured shell)
        shell = os.environ.get("SHELL")
        if shell and os.path.isfile(shell):
            return shell
        # 2. Current user's login shell from passwd
        import pwd

        try:
            pw_shell = pwd.getpwuid(os.getuid()).pw_shell
            if pw_shell and os.path.isfile(pw_shell):
                return pw_shell
        except (KeyError, ImportError):
            pass
        # 3. Fallback: first available common shell
        for sh in ("/bin/zsh", "/bin/bash", "/bin/sh"):
            if os.path.isfile(sh):
                return sh
        return "/bin/sh"

    def exists(self, agent_id: str) -> bool:
        """Check if a process exists and is running."""
        return agent_id in self._processes

    def get_scrollback(self, agent_id: str) -> str:
        """Get the full in-memory scrollback buffer for a process."""
        buf = self._scrollback.get(agent_id)
        if buf:
            return "".join(buf)
        return ""

    def _append_scrollback(self, agent_id: str, data: str) -> None:
        """Append data to the in-memory scrollback buffer, evicting old data if needed."""
        if agent_id not in self._scrollback:
            self._scrollback[agent_id] = collections.deque()
            self._scrollback_bytes[agent_id] = 0

        buf = self._scrollback[agent_id]
        data_bytes = len(data.encode("utf-8", errors="replace"))
        buf.append(data)
        self._scrollback_bytes[agent_id] += data_bytes

        # Evict oldest chunks if over limit
        while self._scrollback_bytes[agent_id] > MAX_SCROLLBACK and buf:
            old = buf.popleft()
            self._scrollback_bytes[agent_id] -= len(
                old.encode("utf-8", errors="replace")
            )

        self._scrollback_dirty.add(agent_id)

    def clear_scrollback(self, agent_id: str) -> None:
        """Clear the in-memory scrollback buffer for a process."""
        self._scrollback.pop(agent_id, None)
        self._scrollback_bytes.pop(agent_id, None)

    def seed_scrollback(self, agent_id: str, data: str) -> None:
        """Seed in-memory scrollback with data (e.g. loaded from disk)."""
        if not data:
            return
        self._scrollback[agent_id] = collections.deque([data])
        self._scrollback_bytes[agent_id] = len(data.encode("utf-8", errors="replace"))

    # ─── Disk persistence ─────────────────────────────────────

    def _flush_scrollback(self, agent_id: str) -> None:
        """Write scrollback buffer to .fantastic/agents/{id}/process.log."""
        if not self._agents_dir:
            return
        buf = self._scrollback.get(agent_id)
        if not buf:
            return
        log_path = self._agents_dir / agent_id / "process.log"
        if not log_path.parent.exists():
            return
        try:
            log_path.write_text("".join(buf), encoding="utf-8", errors="replace")
        except OSError as e:
            logger.debug(f"Failed to flush scrollback for {agent_id}: {e}")

    def _flush_dirty(self) -> None:
        """Flush all dirty scrollback buffers to disk."""
        for tid in list(self._scrollback_dirty):
            self._flush_scrollback(tid)
        self._scrollback_dirty.clear()

    async def _flush_loop(self) -> None:
        """Background loop that flushes dirty scrollback every 5 seconds."""
        try:
            while True:
                await asyncio.sleep(5)
                self._flush_dirty()
        except asyncio.CancelledError:
            self._flush_dirty()  # Final flush on cancellation

    async def start_flush_loop(self) -> None:
        """Start the background flush loop."""
        if self._agents_dir and not self._flush_task:
            self._flush_task = asyncio.create_task(self._flush_loop())

    def load_scrollback_from_disk(self, agent_id: str) -> str:
        """Load scrollback from .fantastic/agents/{id}/process.log if it exists."""
        if not self._agents_dir:
            return ""
        log_path = self._agents_dir / agent_id / "process.log"
        if not log_path.exists():
            return ""
        try:
            return log_path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            return ""

    # ─── Process lifecycle ─────────────────────────────────────

    async def create(
        self,
        agent_id: str,
        cols: int = 80,
        rows: int = 24,
        cwd: str | None = None,
        command: str | None = None,
        args: list[str] | None = None,
        env_extra: dict[str, str] | None = None,
        on_exit: Callable[[str], Awaitable[None]] | None = None,
        welcome_command: str | None = None,
    ) -> None:
        """
        Create a process. If command is given, spawn that program instead of the default shell.
        args are passed as argv (command is always argv[0]).
        env_extra merges into the process environment.
        on_exit is called when the process exits on its own (not when closed via close()).
        If process already exists, this is a no-op (use get_scrollback to replay).
        """
        if agent_id in self._processes:
            return

        master_fd, slave_fd = pty.openpty()

        # Set initial size
        winsize = struct.pack("HHHH", rows, cols, 0, 0)
        fcntl.ioctl(slave_fd, termios.TIOCSWINSZ, winsize)

        prog = command or self._detect_shell()
        argv = [prog] + (args or ([] if command else ["-l"]))
        env = os.environ.copy()
        env["TERM"] = "xterm-256color"
        if env_extra:
            env.update(env_extra)

        pid = os.fork()
        if pid == 0:
            # Child process
            os.close(master_fd)
            os.setsid()
            fcntl.ioctl(slave_fd, termios.TIOCSCTTY, 0)
            os.dup2(slave_fd, 0)
            os.dup2(slave_fd, 1)
            os.dup2(slave_fd, 2)
            if slave_fd > 2:
                os.close(slave_fd)
            if cwd:
                os.chdir(cwd)
            os.execvpe(prog, argv, env)
        else:
            # Parent process
            os.close(slave_fd)

            # IMPORTANT: master_fd MUST be non-blocking. The read loop uses
            # select() + os.read() in an executor thread. Without O_NONBLOCK
            # the write side also becomes non-blocking, so _write_all() must
            # handle BlockingIOError (EAGAIN). Do not remove this flag —
            # it will deadlock the event loop on large reads.
            flags = fcntl.fcntl(master_fd, fcntl.F_GETFL)
            fcntl.fcntl(master_fd, fcntl.F_SETFL, flags | os.O_NONBLOCK)

            _child_pids.add(pid)
            reader_task = asyncio.create_task(self._read_loop(agent_id, master_fd))
            self._processes[agent_id] = {
                "master_fd": master_fd,
                "pid": pid,
                "reader_task": reader_task,
                "cols": cols,
                "rows": rows,
                "on_exit": on_exit,
                # Stored for restart
                "command": command,
                "args": args,
                "env_extra": env_extra,
                "cwd": cwd,
                "welcome_command": welcome_command,
            }
            logger.info(f"Process {agent_id} created (pid={pid}, cmd={prog})")

            # Write welcome command to shell after startup
            if welcome_command:
                await asyncio.sleep(0.15)
                await self.write(agent_id, welcome_command + "\n")

    async def _read_loop(self, agent_id: str, master_fd: int) -> None:
        loop = asyncio.get_event_loop()
        try:
            while True:
                data = await loop.run_in_executor(None, self._blocking_read, master_fd)
                if data is None:
                    break
                if data:
                    self._append_scrollback(agent_id, data)
                if data and self._on_output:
                    await self._on_output(agent_id, data)
        except asyncio.CancelledError:
            return  # Cancelled via close() — skip on_exit
        except Exception as e:
            logger.debug(f"Process {agent_id} read loop ended: {e}")

        # Process exited on its own — clean up and fire on_exit callback
        proc = self._processes.pop(agent_id, None)
        if not proc:
            return
        try:
            os.close(proc["master_fd"])
        except OSError:
            pass
        _child_pids.discard(proc.get("pid", 0))
        on_exit = proc.get("on_exit")
        if on_exit:
            try:
                await on_exit(agent_id)
            except Exception as e:
                logger.error(f"Process {agent_id} on_exit error: {e}")

    def _blocking_read(self, master_fd: int) -> str | None:
        """Blocking read from master fd. Returns None on EOF."""
        import select

        try:
            r, _, _ = select.select([master_fd], [], [], 0.1)
            if r:
                data = os.read(master_fd, 4096)
                if not data:
                    return None
                return data.decode("utf-8", errors="replace")
            return ""  # timeout, no data yet
        except OSError:
            return None

    @staticmethod
    def _write_all(fd: int, data: bytes) -> None:
        """Write all bytes to fd, handling partial writes and non-blocking EAGAIN."""
        import time

        mv = memoryview(data)
        while mv:
            try:
                n = os.write(fd, mv)
                mv = mv[n:]
            except BlockingIOError:
                # PTY buffer full — wait for child to consume
                time.sleep(0.01)
            except OSError:
                break

    async def write(self, agent_id: str, data: str) -> None:
        proc = self._processes.get(agent_id)
        if not proc:
            return
        try:
            raw = data.encode("utf-8")
            fd = proc["master_fd"]
            loop = asyncio.get_running_loop()
            await loop.run_in_executor(None, self._write_all, fd, raw)
        except OSError as e:
            logger.error(f"Process {agent_id} write error: {e}")

    async def resize(self, agent_id: str, cols: int, rows: int) -> None:
        proc = self._processes.get(agent_id)
        if not proc:
            return
        try:
            winsize = struct.pack("HHHH", rows, cols, 0, 0)
            fcntl.ioctl(proc["master_fd"], termios.TIOCSWINSZ, winsize)
            proc["cols"] = cols
            proc["rows"] = rows
        except OSError as e:
            logger.error(f"Process {agent_id} resize error: {e}")

    def get_dimensions(self, agent_id: str) -> tuple[int, int] | None:
        """Get current (cols, rows) for a process, or None if not found."""
        proc = self._processes.get(agent_id)
        if not proc:
            return None
        return (proc.get("cols", 80), proc.get("rows", 24))

    async def restart(self, agent_id: str) -> None:
        """Restart a process with the same parameters."""
        proc = self._processes.get(agent_id)
        if not proc:
            raise ValueError(f"Process {agent_id} not found")
        # Copy params before close() removes the entry
        command = proc.get("command")
        args = proc.get("args")
        env_extra = proc.get("env_extra")
        cwd = proc.get("cwd")
        cols = proc.get("cols", 80)
        rows = proc.get("rows", 24)
        on_exit = proc.get("on_exit")
        welcome_command = proc.get("welcome_command")
        await self.close(agent_id)
        await self.create(
            agent_id,
            cols=cols,
            rows=rows,
            cwd=cwd,
            command=command,
            args=args,
            env_extra=env_extra,
            on_exit=on_exit,
            welcome_command=welcome_command,
        )
        logger.info(f"Process {agent_id} restarted")

    def send_signal(self, agent_id: str, sig: int) -> None:
        """Send a signal to a process."""
        proc = self._processes.get(agent_id)
        if not proc:
            raise ValueError(f"Process {agent_id} not found")
        os.kill(proc["pid"], sig)
        logger.info(f"Sent signal {sig} to process {agent_id} (pid={proc['pid']})")

    async def close(self, agent_id: str) -> None:
        proc = self._processes.pop(agent_id, None)
        if not proc:
            # Still clean up scrollback
            self._flush_scrollback(agent_id)
            self._scrollback_dirty.discard(agent_id)
            self._scrollback.pop(agent_id, None)
            self._scrollback_bytes.pop(agent_id, None)
            return
        proc["reader_task"].cancel()
        try:
            os.close(proc["master_fd"])
        except OSError:
            pass
        try:
            os.kill(proc["pid"], signal.SIGTERM)
        except OSError:
            pass
        _child_pids.discard(proc["pid"])
        self._flush_scrollback(agent_id)
        self._scrollback_dirty.discard(agent_id)
        self._scrollback.pop(agent_id, None)
        self._scrollback_bytes.pop(agent_id, None)
        logger.info(f"Process {agent_id} closed")

    async def close_all(self) -> None:
        if self._flush_task:
            self._flush_task.cancel()
            try:
                await self._flush_task
            except asyncio.CancelledError:
                pass
            self._flush_task = None
        for cid in list(self._processes.keys()):
            await self.close(cid)
