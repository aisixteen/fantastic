"""
Code runner — execute Python code via subprocess. One-shot, stateless.

Output format: {"outputs": [...], "success": bool}.
"""

import asyncio
import logging
import signal
import sys
from typing import Any

logger = logging.getLogger(__name__)


class CodeRunner:
    """Execute Python code via subprocess. One-shot, stateless."""

    def __init__(self, project_dir: str):
        self._project_dir = project_dir
        self._processes: dict[str, asyncio.subprocess.Process] = {}

    async def execute(
        self, agent_id: str, code: str, timeout: float = 60, cwd: str | None = None
    ) -> dict[str, Any]:
        """Run code as `python -c code`, capture stdout/stderr."""
        proc = await asyncio.create_subprocess_exec(
            sys.executable,
            "-c",
            code,
            cwd=cwd or self._project_dir,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        self._processes[agent_id] = proc
        try:
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout)
        except asyncio.TimeoutError:
            proc.kill()
            await proc.communicate()
            return {
                "outputs": [
                    {
                        "output_type": "error",
                        "ename": "TimeoutError",
                        "evalue": "Execution timed out",
                        "traceback": ["TimeoutError: Execution timed out"],
                    }
                ],
                "success": False,
                "error": None,
            }
        finally:
            self._processes.pop(agent_id, None)

        outputs: list[dict[str, Any]] = []
        if stdout:
            outputs.append(
                {
                    "output_type": "stream",
                    "name": "stdout",
                    "text": stdout.decode(),
                }
            )
        if stderr and proc.returncode != 0:
            outputs.append(
                {
                    "output_type": "error",
                    "ename": "RuntimeError",
                    "evalue": stderr.decode(),
                    "traceback": [stderr.decode()],
                }
            )
        elif stderr:
            outputs.append(
                {
                    "output_type": "stream",
                    "name": "stderr",
                    "text": stderr.decode(),
                }
            )

        return {
            "outputs": outputs,
            "success": proc.returncode == 0,
            "error": None,
        }

    async def interrupt(self, agent_id: str) -> bool:
        """Send SIGINT to a running process."""
        proc = self._processes.get(agent_id)
        if proc and proc.returncode is None:
            proc.send_signal(signal.SIGINT)
            return True
        return False

    async def stop_all(self) -> None:
        """Kill all running processes."""
        for proc in self._processes.values():
            if proc.returncode is None:
                proc.kill()
        self._processes.clear()
