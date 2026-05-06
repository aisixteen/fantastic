"""python_runtime — subprocess Python exec.

Each `exec` is its own process: `python -c <code>`. Stateless across
calls (no shared globals or KV cache). Per-agent in-flight tracking
enables interrupt/stop.

Replaces the old `execute_python` MCP tool from the prior codebase.
Lives as its own agent so cwd, timeout, interpreter, and (future)
sandboxing are addressable per instance.

Interpreter resolution (highest priority first):
  1. payload.python      — explicit interpreter path on the call
  2. payload.venv        — venv dir on the call; <venv>/bin/python
  3. record.python       — explicit interpreter on the agent record
  4. record.venv         — venv dir on the agent record
  5. sys.executable      — kernel's own interpreter (default)

`venv` is the project-local uv-style venv (or any layout shipping
`bin/python` / `bin/python3` / `Scripts/python.exe`). `python` is
the absolute path to a specific interpreter, useful for conda envs
or system installs. Both are ~/-expansion-aware.

This decouples each agent's runtime from the kernel: an ML project
points at its own conda env with torch/yaml/etc.; a fresh project
gets an isolated `uv venv .venv && uv pip install …` pinned env;
the kernel itself stays slim.
"""

from __future__ import annotations

import asyncio
import signal as signal_mod
import sys
from pathlib import Path


# Per-agent set of live subprocess.Process — used for interrupt / stop.
_procs: dict[str, set[asyncio.subprocess.Process]] = {}


def _resolve_python(rec: dict, payload: dict | None = None) -> str:
    """Resolve which Python interpreter a given exec should use.

    Tries `payload.python` → `payload.venv` → `record.python` →
    `record.venv` → sys.executable. A `venv` value resolves to
    `<venv>/bin/python` (Unix) or `<venv>/Scripts/python.exe`
    (Windows-style). Missing override entries fall through; only an
    EXISTING explicit path overrides — a `venv` that points nowhere
    silently degrades to sys.executable so a typo doesn't break
    every exec.
    """
    payload = payload or {}

    # 1. payload.python (per-call override)
    p = payload.get("python")
    if p:
        return str(Path(p).expanduser())

    # 2. payload.venv (per-call override)
    pv = payload.get("venv")
    if pv:
        cand = _venv_python(pv)
        if cand:
            return cand

    # 3. record.python
    rp = rec.get("python")
    if rp:
        return str(Path(rp).expanduser())

    # 4. record.venv
    rv = rec.get("venv")
    if rv:
        cand = _venv_python(rv)
        if cand:
            return cand

    # 5. default — kernel's own interpreter
    return sys.executable


def _venv_python(venv_path: str) -> str | None:
    """Locate a Python interpreter inside a venv-style directory.
    Returns None when nothing usable is found (caller falls back)."""
    base = Path(venv_path).expanduser()
    for rel in ("bin/python", "bin/python3", "Scripts/python.exe", "Scripts/python"):
        cand = base / rel
        if cand.exists():
            return str(cand)
    return None


# ─── verbs ──────────────────────────────────────────────────────


async def _reflect(id, payload, kernel):
    """Identity + cwd + interpreter + count of in-flight subprocesses. No args."""
    rec = kernel.get(id) or {}
    live = _procs.get(id, set())
    return {
        "id": id,
        "sentence": "Python subprocess runner.",
        "cwd": rec.get("cwd") or "<process default>",
        "python": _resolve_python(rec),
        "venv": rec.get("venv"),
        "in_flight": sum(1 for p in live if p.returncode is None),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }


async def _exec(id, payload, kernel):
    """args: code:str (req), timeout:float? (default 60), cwd:str? (overrides agent cwd), python:str? (interpreter path override), venv:str? (venv-dir override; uses <venv>/bin/python). Resolution order — payload.python > payload.venv > record.python > record.venv > sys.executable. Spawns `<interp> -c code`, captures pipes. Returns {stdout, stderr, exit_code, timed_out:bool}."""
    code = payload.get("code", "")
    if not isinstance(code, str) or not code:
        return {"error": "python_runtime: code (str) required"}
    timeout = float(payload.get("timeout", 60))
    rec = kernel.get(id) or {}
    cwd = payload.get("cwd") or rec.get("cwd") or None
    interp = _resolve_python(rec, payload)

    proc = await asyncio.create_subprocess_exec(
        interp,
        "-c",
        code,
        cwd=cwd,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    _procs.setdefault(id, set()).add(proc)
    timed_out = False
    try:
        try:
            stdout, stderr = await asyncio.wait_for(proc.communicate(), timeout=timeout)
        except asyncio.TimeoutError:
            timed_out = True
            proc.kill()
            try:
                stdout, stderr = await proc.communicate()
            except Exception:
                stdout, stderr = b"", b""
    finally:
        _procs.get(id, set()).discard(proc)

    return {
        "stdout": stdout.decode("utf-8", errors="replace"),
        "stderr": stderr.decode("utf-8", errors="replace"),
        "exit_code": proc.returncode if proc.returncode is not None else -1,
        "timed_out": timed_out,
    }


async def _interrupt(id, payload, kernel):
    """No args. Sends SIGINT to all in-flight subprocesses for this agent. Returns {interrupted:int}."""
    n = 0
    for p in list(_procs.get(id, set())):
        if p.returncode is None:
            try:
                p.send_signal(signal_mod.SIGINT)
                n += 1
            except OSError:
                pass
    return {"interrupted": n}


async def _stop(id, payload, kernel):
    """No args. SIGKILLs all in-flight subprocesses for this agent. Returns {killed:int}."""
    n = 0
    for p in list(_procs.get(id, set())):
        if p.returncode is None:
            try:
                p.kill()
                n += 1
            except OSError:
                pass
    return {"killed": n}


async def _boot(id, payload, kernel):
    """No-op. python_runtime is stateless per-call."""
    return None


# ─── dispatch ───────────────────────────────────────────────────


VERBS = {
    "reflect": _reflect,
    "exec": _exec,
    "interrupt": _interrupt,
    "stop": _stop,
    "boot": _boot,
}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return {"error": f"python_runtime: unknown type {t!r}"}
    return await fn(id, payload, kernel)
