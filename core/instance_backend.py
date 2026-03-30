"""
Abstract instance backend — local + SSH transport for launching Fantastic instances.

LocalBackend: spawns a subprocess on the local machine.
SSHBackend: launches via SSH tunnel, relying on ~/.ssh/config host aliases.

Architecture:
  Launcher                             Remote (gpu-box)
  ┌─────────────┐    SSH tunnel       ┌──────────────┐
  │ launch(...)  │                     │              │
  │  1. ssh: find free remote port     │ → port found │
  │  2. ssh -L local:localhost:remote  │ → fantastic   │
  │  3. health_check via tunnel        │ ← via tunnel │
  │  Result: url=http://127.0.0.1:local│              │
  └─────────────┘                     └──────────────┘

SSH subprocess stays alive = tunnel stays alive.
process.terminate() = tunnel closes + remote gets SIGHUP.
"""

import asyncio
import logging
import os
import signal
import socket
import sys
from abc import ABC, abstractmethod
from dataclasses import dataclass, field

logger = logging.getLogger(__name__)


async def _drain_pipe(stream: asyncio.StreamReader | None) -> None:
    """Read and discard all data from a pipe to prevent buffer deadlock."""
    if stream is None:
        return
    try:
        while True:
            chunk = await stream.read(65536)
            if not isinstance(chunk, (bytes, bytearray)) or not chunk:
                break
    except Exception:
        pass


@dataclass
class LaunchResult:
    """Result of a backend launch operation."""

    process: asyncio.subprocess.Process
    pid: int
    port: int  # port accessible from the launcher
    url: str  # URL accessible from the launcher
    extra: dict = field(default_factory=dict)  # backend-specific metadata


class InstanceBackend(ABC):
    """Abstract base for instance launch/stop backends."""

    @property
    @abstractmethod
    def backend_type(self) -> str: ...

    @abstractmethod
    async def launch(
        self,
        project_dir: str,
        port: int = 0,
        cli: str = "",
    ) -> LaunchResult: ...

    @abstractmethod
    async def stop(self, pid: int) -> None:
        """Stop an instance by PID."""
        ...

    # ─── Shared helpers ────────────────────────────────────────────────

    def find_free_local_port(self, start: int = 49200, max_tries: int = 100) -> int:
        """Find a free TCP port on localhost."""
        for port in range(start, start + max_tries):
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
                try:
                    s.bind(("127.0.0.1", port))
                    return port
                except OSError:
                    continue
        raise RuntimeError(
            f"No free port found in range {start}-{start + max_tries - 1}"
        )

    async def health_check(self, url: str, timeout: float = 15.0) -> bool:
        """Poll url until 200 response or timeout."""
        import httpx

        deadline = asyncio.get_event_loop().time() + timeout
        while asyncio.get_event_loop().time() < deadline:
            try:
                async with httpx.AsyncClient() as client:
                    resp = await client.get(url, timeout=2)
                    if resp.status_code == 200:
                        return True
            except Exception:
                pass
            await asyncio.sleep(0.5)
        return False


class LocalBackend(InstanceBackend):
    """Launch instances as local subprocesses."""

    @property
    def backend_type(self) -> str:
        return "local"

    async def launch(
        self,
        project_dir: str,
        port: int = 0,
        cli: str = "",
    ) -> LaunchResult:
        abs_dir = os.path.abspath(project_dir)
        if not os.path.isdir(abs_dir):
            raise RuntimeError(f"Directory does not exist: {abs_dir}")

        if port == 0:
            port = self.find_free_local_port()

        cmd = [
            sys.executable,
            "-m",
            "core.cli",
            "--port",
            str(port),
            "--host",
            "127.0.0.1",
            "--project-dir",
            abs_dir,
            "serve",
        ]
        if cli:
            cmd += ["--cli"] + cli.split()

        proc = await asyncio.create_subprocess_exec(
            *cmd,
            start_new_session=True,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        # Wait for health check
        url = f"http://127.0.0.1:{port}/api/state"
        for _ in range(30):
            await asyncio.sleep(0.5)
            if proc.returncode is not None:
                stderr = ""
                if proc.stderr:
                    stderr = (await proc.stderr.read()).decode(errors="replace")
                raise RuntimeError(
                    f"Process exited with code {proc.returncode}: {stderr[:500]}"
                )
            if await self.health_check(url, timeout=1.0):
                # Drain pipes to prevent buffer deadlock
                asyncio.create_task(_drain_pipe(proc.stdout))
                asyncio.create_task(_drain_pipe(proc.stderr))
                return LaunchResult(
                    process=proc,
                    pid=proc.pid,
                    port=port,
                    url=f"http://127.0.0.1:{port}",
                )

        proc.terminate()
        raise RuntimeError(f"Instance on port {port} did not become ready within 15s")

    async def stop(self, pid: int) -> None:
        """Stop a local instance by killing its process group."""
        try:
            pgid = os.getpgid(pid)
            os.killpg(pgid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            return
        # Wait up to 3s, then SIGKILL
        for _ in range(30):
            await asyncio.sleep(0.1)
            try:
                os.kill(pid, 0)
            except (ProcessLookupError, OSError):
                return
        try:
            pgid = os.getpgid(pid)
            os.killpg(pgid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            pass


class SSHBackend(InstanceBackend):
    """Launch instances on a remote host via SSH tunnel."""

    def __init__(self, ssh_host: str, remote_cmd: str = "fantastic"):
        self._ssh_host = ssh_host
        self._remote_cmd = remote_cmd

    @property
    def backend_type(self) -> str:
        return "ssh"

    def _remote_python_cmd(self) -> str:
        """Derive remote Python path from remote_cmd path.

        /home/user/miniconda3/envs/fantastic/bin/fantastic → .../bin/python3
        Bare 'fantastic' → 'python3'
        """
        from pathlib import PurePosixPath

        p = PurePosixPath(self._remote_cmd.split()[0])
        if p.parent != PurePosixPath("."):
            return str(p.parent / "python3")
        return "python3"

    async def _find_free_remote_port(self) -> int:
        """Find a free port on the remote host via SSH."""
        # chr(0)*0 produces "" without quote chars — avoids nested quoting hell
        snippet = "import socket;s=socket.socket();s.bind((chr(0)*0,0));print(s.getsockname()[1]);s.close()"
        python_cmd = self._remote_python_cmd()
        stdout, stderr, rc = await self._ssh_exec(f"{python_cmd} -c '{snippet}'")
        if rc != 0:
            raise RuntimeError(f"SSH port-finding failed on {self._ssh_host}: {stderr}")
        return int(stdout)

    async def _ssh_exec(self, cmd: str, timeout: int = 15) -> tuple[str, str, int]:
        """Run a command on the remote host via SSH. Returns (stdout, stderr, returncode)."""
        escaped = cmd.replace("'", "'\\''")
        full_cmd = f"ssh {self._ssh_host} '{escaped}'"
        proc = await asyncio.create_subprocess_shell(
            full_cmd,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        return stdout.decode().strip(), stderr.decode().strip(), proc.returncode

    async def _check_remote_server(self, project_dir: str) -> dict | None:
        """Check if a server is already running on the remote for this project dir.

        Returns {"pid": int, "port": int} if alive, None otherwise.
        """
        python_cmd = self._remote_python_cmd()
        check_script = (
            "import json, os, signal; "
            f"p = '{project_dir}/.fantastic/config.json'; "
            "d = json.load(open(p)) if os.path.exists(p) else {}; "
            "pid = d.get('pid', 0); port = d.get('port', 0); "
            "alive = False; "
            "exec('try:\\n os.kill(pid, 0); alive = True\\nexcept: pass') if pid else None; "
            "print(f'{pid}:{port}:{alive}')"
        )
        try:
            stdout, _, rc = await self._ssh_exec(f"{python_cmd} -c '{check_script}'")
            if rc != 0 or not stdout:
                return None
            parts = stdout.split(":")
            if len(parts) == 3 and parts[2] == "True":
                return {"pid": int(parts[0]), "port": int(parts[1])}
        except Exception:
            pass
        return None

    async def launch(
        self,
        project_dir: str,
        port: int = 0,
        cli: str = "",
    ) -> LaunchResult:
        # Step 0: Check if server already running on remote
        existing = await self._check_remote_server(project_dir)
        if existing:
            remote_port = existing["port"]
            local_port = port if port != 0 else self.find_free_local_port()
            # Just set up tunnel to existing server
            ssh_cmd = (
                f"ssh -L {local_port}:localhost:{remote_port}"
                f" -o ExitOnForwardFailure=yes"
                f" -o ServerAliveInterval=15"
                f" -o ServerAliveCountMax=3"
                f" -N"
                f" {self._ssh_host}"
            )
            proc = await asyncio.create_subprocess_shell(
                ssh_cmd,
                start_new_session=True,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            url = f"http://127.0.0.1:{local_port}"
            if not await self.health_check(url):
                proc.terminate()
                raise RuntimeError(
                    f"Remote server on {self._ssh_host} (pid {existing['pid']}, port {remote_port}) "
                    f"is alive but tunnel health check failed"
                )
            return LaunchResult(
                process=proc,
                pid=existing["pid"],
                port=local_port,
                url=url,
                extra={
                    "ssh_host": self._ssh_host,
                    "remote_port": remote_port,
                    "local_port": local_port,
                    "reused": True,
                    "tunnel_pid": proc.pid,
                },
            )

        # Step 1: Find free port on remote
        remote_port = await self._find_free_remote_port()

        # Step 2: Find free local port for the tunnel
        local_port = port if port != 0 else self.find_free_local_port()

        # Build remote command: cd into project dir, then run fantastic
        remote_cmd_parts = [
            self._remote_cmd,
            "--port",
            str(remote_port),
            "--host",
            "127.0.0.1",
        ]
        if cli:
            remote_cmd_parts += ["--cli"] + cli.split()
        remote_cmd_str = f"cd {project_dir} && " + " ".join(remote_cmd_parts)

        # Step 3: SSH with port forwarding (full paths, no shell wrapping needed)
        escaped_cmd = remote_cmd_str.replace("'", "'\\''")
        ssh_cmd = (
            f"ssh -L {local_port}:localhost:{remote_port}"
            f" -o ExitOnForwardFailure=yes"
            f" -o ServerAliveInterval=15"
            f" -o ServerAliveCountMax=3"
            f" {self._ssh_host}"
            f" '{escaped_cmd}'"
        )
        proc = await asyncio.create_subprocess_shell(
            ssh_cmd,
            start_new_session=True,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )

        # Step 4: Health check via tunnel (longer timeout for SSH)
        url = f"http://127.0.0.1:{local_port}/api/state"
        for _ in range(60):
            await asyncio.sleep(0.5)
            if proc.returncode is not None:
                stderr = ""
                if proc.stderr:
                    stderr = (await proc.stderr.read()).decode(errors="replace")
                raise RuntimeError(
                    f"SSH process exited with code {proc.returncode}: {stderr[:500]}"
                )
            if await self.health_check(url, timeout=1.0):
                # Drain pipes to prevent buffer deadlock
                asyncio.create_task(_drain_pipe(proc.stdout))
                asyncio.create_task(_drain_pipe(proc.stderr))
                return LaunchResult(
                    process=proc,
                    pid=proc.pid,
                    port=local_port,
                    url=f"http://127.0.0.1:{local_port}",
                    extra={
                        "ssh_host": self._ssh_host,
                        "remote_port": remote_port,
                        "local_port": local_port,
                    },
                )

        proc.terminate()
        raise RuntimeError(
            f"SSH instance on {self._ssh_host}:{remote_port} "
            f"(tunnel {local_port}) did not become ready within 30s"
        )

    async def stop(self, pid: int) -> None:
        """Stop SSH tunnel by killing the tunnel process (remote gets SIGHUP)."""
        try:
            os.kill(pid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            return
        for _ in range(50):
            await asyncio.sleep(0.1)
            try:
                os.kill(pid, 0)
            except (ProcessLookupError, OSError):
                return
        try:
            os.kill(pid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            pass

    async def stop_remote(self, remote_pid: int) -> None:
        """Kill the remote server process via SSH."""
        try:
            await self._ssh_exec(f"kill {remote_pid}", timeout=10)
        except Exception:
            pass


def get_backend(
    ssh_host: str | None = None,
    remote_cmd: str = "fantastic",
) -> InstanceBackend:
    """Factory: returns LocalBackend or SSHBackend based on ssh_host."""
    if ssh_host:
        return SSHBackend(ssh_host=ssh_host, remote_cmd=remote_cmd)
    return LocalBackend()
