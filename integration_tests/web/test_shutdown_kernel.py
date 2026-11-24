"""Kernel stop integration test — the rust kernel (python cases skipped, #170).

The process verbs are the operator's alone. Over BOTH transports (web_rest
POST + web_ws), a leg is an agent caller, so:

  1. `kernel_stop` addressed at a CHILD (`web`, on which the leg holds a
     `kernel_stop` line, so the gate passes) is answered `not_here`, and the
     kernel keeps running;
  2. `kernel_stop` addressed at the root is refused
     `{reason: not_allowed, error: "kernel_stop: operator only"}`, and the
     kernel keeps running;
  3. the operator's stop (SIGTERM) releases .fantastic/lock.json, drains,
     stops the listeners and exits code 0 → the port goes down.

These run under the default `local` target (built binaries). They also
run under `FANTASTIC_TARGET=container` — the KernelProc surface is
identical, so the same assertions validate the shipped image.
"""

from __future__ import annotations

import asyncio
import signal
import subprocess
import time

import httpx
import pytest

from helpers.kernel_proc import KernelProc
from helpers.seeding import call, seed_web, seed_web_rest, seed_web_ws
from helpers.ws import ws_call

#: the leg's one line: `kernel_stop` on its parent `web`, so a child-addressed
#: stop passes the gate and is answered by the dispatch (`not_here`)
REACH = {"web": call("kernel_stop")}
OPERATOR_ONLY = {"reason": "not_allowed", "error": "kernel_stop: operator only"}


async def _await_exit(kp: KernelProc, timeout: float = 15.0) -> int | None:
    """Block until the kernel process/container has exited. Returns the
    local exit code (None for a container). Raises on timeout."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if kp.proc is not None:
            if kp.proc.poll() is not None:
                return kp.proc.returncode
        elif kp.container:
            if kp._container_running() is False:
                return None
        await asyncio.sleep(0.1)
    kp._drain_output()
    raise AssertionError(
        f"{kp.label}: kernel did not exit within {timeout}s after SIGTERM\n"
        f"--- stderr ---\n{kp.stderr}"
    )


async def _await_port_dead(port: int, timeout: float = 10.0) -> None:
    """Block until the port stops accepting (the listener is gone)."""
    deadline = time.monotonic() + timeout
    async with httpx.AsyncClient(timeout=1.0) as client:
        while time.monotonic() < deadline:
            try:
                await client.get(f"http://127.0.0.1:{port}/")
            except httpx.TransportError:
                return  # connection refused / reset → listener down
            await asyncio.sleep(0.1)
    raise AssertionError(f"port {port} still accepting after SIGTERM")


def _assert_lock_present(kp: KernelProc) -> None:
    # Establish the lock EXISTED while up, so the post-shutdown
    # `not exists()` is a real before/after transition, not a vacuous pass.
    lock = kp.workdir / ".fantastic" / "lock.json"
    assert lock.exists(), f"{kp.label}: lock.json missing while the daemon is up: {lock}"


def _assert_lock_released(kp: KernelProc) -> None:
    lock = kp.workdir / ".fantastic" / "lock.json"
    assert not lock.exists(), f"{kp.label}: lock.json not released after shutdown: {lock}"


async def _assert_still_serving(kp: KernelProc, port: int) -> None:
    """Transport-agnostic liveness check used AFTER a refused `kernel_stop` —
    proves the refusal did not stop the kernel. Works for both local (proc) and
    container targets (where kp.proc is None)."""
    await asyncio.sleep(0.3)
    async with httpx.AsyncClient(timeout=2.0) as client:
        live = await client.get(f"http://127.0.0.1:{port}/")
    assert live.status_code < 500, (
        f"{kp.label}: kernel stopped serving after a refused kernel_stop "
        f"(status {live.status_code})"
    )
    if kp.proc is not None:
        assert kp.proc.poll() is None, (
            f"{kp.label}: process exited on a refused kernel_stop"
        )
    if kp.container:
        assert kp._container_running() is True, (
            f"{kp.label}: container stopped on a refused kernel_stop"
        )


def _sigterm(kp: KernelProc) -> None:
    """The operator's stop: SIGTERM to the daemon (a container: `kill --signal
    TERM`, so it exits on its own and is not removed before the checks)."""
    if kp.container and kp.engine:
        subprocess.run([kp.engine, "kill", "--signal", "TERM", kp.container], capture_output=True, text=True)
    elif kp.proc is not None:
        kp.proc.send_signal(signal.SIGTERM)


def _assert_not_here(reply: dict) -> None:
    assert reply.get("reason") == "not_here" and reply.get("verb") == "kernel_stop", (
        f"kernel_stop on a child should be not_here, got: {reply}"
    )


def _assert_operator_only(reply: dict) -> None:
    assert {k: reply.get(k) for k in OPERATOR_ONLY} == OPERATOR_ONLY, (
        f"kernel_stop on the root from a leg should be operator only, got: {reply}"
    )


async def _operator_stop(kp: KernelProc, port: int) -> None:
    _sigterm(kp)
    code = await _await_exit(kp)
    _assert_clean_exit(kp, code)
    _assert_lock_released(kp)
    await _await_port_dead(port)


def _assert_clean_exit(kp: KernelProc, code: int | None) -> None:
    # Local: graceful shutdown returns 0. Container: code is None (we only
    # observe Running==false), the exit-0 → container-stop is the signal.
    if kp.proc is not None:
        assert code == 0, f"{kp.label}: expected exit 0, got {code}"


async def _shutdown_via_rest(binary, kernel_factory, parity_tmp, free_port, tag: str) -> None:
    base = parity_tmp(f"shutdown_rest_{tag}")
    workdir = base / "W"
    workdir.mkdir(parents=True)
    port = free_port()

    seed_web(binary, workdir, port)
    rest_id = seed_web_rest(binary, workdir, reach=REACH)
    kp = await kernel_factory(workdir, port)

    async with httpx.AsyncClient(timeout=5.0) as client:
        # Addressed at a CHILD agent (`web`) it is not_here, and the kernel keeps
        # running. The HTTP status web_rest maps a refusal to is a transport
        # detail — assert on the body.
        rneg = await client.post(
            f"http://127.0.0.1:{port}/{rest_id}/web",
            json={"type": "kernel_stop"},
        )
        assert rneg.status_code in (200, 400), f"child POST status {rneg.status_code}: {rneg.text}"
        _assert_not_here(rneg.json())
        await _assert_still_serving(kp, port)

        # Addressed at the root via the `kernel` alias: operator only.
        r = await client.post(
            f"http://127.0.0.1:{port}/{rest_id}/kernel",
            json={"type": "kernel_stop"},
        )
        assert r.status_code in (200, 400, 403), f"root POST status {r.status_code}: {r.text}"
        _assert_operator_only(r.json())
        await _assert_still_serving(kp, port)

    # Lock is present while up — so the post-stop release is a real
    # before/after transition, not a vacuous pass.
    _assert_lock_present(kp)
    await _operator_stop(kp, port)


async def _shutdown_via_ws(binary, kernel_factory, parity_tmp, free_port, tag: str) -> None:
    base = parity_tmp(f"shutdown_ws_{tag}")
    workdir = base / "W"
    workdir.mkdir(parents=True)
    port = free_port()

    seed_web(binary, workdir, port)
    seed_web_ws(binary, workdir, reach=REACH)
    kp = await kernel_factory(workdir, port)

    # The same over WS: a child-addressed stop is not_here, a root stop is
    # operator only, and the kernel stays up after each.
    _assert_not_here(await ws_call(port, "web", "kernel_stop"))
    await _assert_still_serving(kp, port)
    _assert_operator_only(await ws_call(port, "kernel", "kernel_stop"))
    await _assert_still_serving(kp, port)

    _assert_lock_present(kp)
    await _operator_stop(kp, port)


@pytest.mark.skip(reason="#170: Python product keeps the old grant model")
@pytest.mark.asyncio
async def test_shutdown_kernel_rest_python(python_binary, python_kernel, parity_tmp, free_port):
    await _shutdown_via_rest(python_binary, python_kernel, parity_tmp, free_port, "py")


@pytest.mark.skip(reason="#170: Python product keeps the old grant model")
@pytest.mark.asyncio
async def test_shutdown_kernel_ws_python(python_binary, python_kernel, parity_tmp, free_port):
    await _shutdown_via_ws(python_binary, python_kernel, parity_tmp, free_port, "py")


@pytest.mark.asyncio
async def test_shutdown_kernel_rest_rust(rust_binary, rust_kernel, parity_tmp, free_port):
    await _shutdown_via_rest(rust_binary, rust_kernel, parity_tmp, free_port, "rs")


@pytest.mark.asyncio
async def test_shutdown_kernel_ws_rust(rust_binary, rust_kernel, parity_tmp, free_port):
    await _shutdown_via_ws(rust_binary, rust_kernel, parity_tmp, free_port, "rs")
