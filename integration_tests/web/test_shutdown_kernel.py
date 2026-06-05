"""`shutdown_kernel` verb integration test — python + rust kernels.

Proves the remotely-callable graceful self-shutdown that makes kernel
lifecycle backend-agnostic (the app stops a kernel without knowing
whether it's a container or a bare process):

  send → kernel {"type":"shutdown_kernel"}
    1. acks {"type":"shutdown_kernel","ok":true} FIRST (flushed to the caller),
    2. releases .fantastic/lock.json, drains, stops the listeners,
    3. exits code 0 → the port goes down.

Reachable over BOTH transports (web_rest POST + web_ws), on BOTH hosts
(python canonical + rust), identical wire shape. Privileged + root-only:
addressed at a CHILD agent it returns an error and the kernel keeps
running (the gating assertion in the REST cases).

These run under the default `local` target (built binaries). They also
run under `FANTASTIC_TARGET=container` — the KernelProc surface is
identical, so the same assertions validate the shipped image.
"""

from __future__ import annotations

import asyncio
import time

import httpx
import pytest

from helpers.kernel_proc import KernelProc
from helpers.seeding import seed_web, seed_web_rest, seed_web_ws
from helpers.ws import ws_call


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
        f"{kp.label}: kernel did not exit within {timeout}s after shutdown_kernel\n"
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
    raise AssertionError(f"port {port} still accepting after shutdown_kernel")


def _assert_lock_present(kp: KernelProc) -> None:
    # Establish the lock EXISTED while up, so the post-shutdown
    # `not exists()` is a real before/after transition, not a vacuous pass.
    lock = kp.workdir / ".fantastic" / "lock.json"
    assert lock.exists(), f"{kp.label}: lock.json missing while the daemon is up: {lock}"


def _assert_lock_released(kp: KernelProc) -> None:
    lock = kp.workdir / ".fantastic" / "lock.json"
    assert not lock.exists(), f"{kp.label}: lock.json not released after shutdown: {lock}"


async def _assert_still_serving(kp: KernelProc, port: int) -> None:
    """Transport-agnostic liveness check used AFTER a refused child-addressed
    shutdown_kernel — proves the gate did not stop the kernel. Works for both
    local (proc) and container targets (where kp.proc is None)."""
    await asyncio.sleep(0.3)
    async with httpx.AsyncClient(timeout=2.0) as client:
        live = await client.get(f"http://127.0.0.1:{port}/")
    assert live.status_code < 500, (
        f"{kp.label}: kernel stopped serving after a refused child shutdown_kernel "
        f"(status {live.status_code})"
    )
    if kp.proc is not None:
        assert kp.proc.poll() is None, (
            f"{kp.label}: process exited on a refused child shutdown_kernel"
        )
    if kp.container:
        assert kp._container_running() is True, (
            f"{kp.label}: container stopped on a refused child shutdown_kernel"
        )


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
    rest_id = seed_web_rest(binary, workdir)
    kp = await kernel_factory(workdir, port)

    async with httpx.AsyncClient(timeout=5.0) as client:
        # Gating: addressed at a CHILD agent (`web`) it must refuse, and the
        # kernel must keep running (no shutdown triggered). The error REPLY
        # shape is identical across runtimes ({"error": ...}); the HTTP
        # status that web_rest maps it to differs (python 200-with-error-body,
        # rust 400), a pre-existing transport detail — assert on the body.
        rneg = await client.post(
            f"http://127.0.0.1:{port}/{rest_id}/web",
            json={"type": "shutdown_kernel"},
        )
        assert rneg.status_code in (200, 400), f"gating POST status {rneg.status_code}: {rneg.text}"
        neg = rneg.json()
        assert "error" in neg and "root control surface" in neg["error"], (
            f"shutdown_kernel on a child should error, got: {neg}"
        )
        # The refused call must NOT have stopped the kernel (transport-agnostic).
        await _assert_still_serving(kp, port)

        # Lock is present while up — so the post-shutdown release is a real
        # before/after transition, not a vacuous pass.
        _assert_lock_present(kp)

        # Real shutdown: addressed at the root via the `kernel` alias.
        r = await client.post(
            f"http://127.0.0.1:{port}/{rest_id}/kernel",
            json={"type": "shutdown_kernel"},
        )
        assert r.status_code == 200, f"shutdown_kernel status {r.status_code}: {r.text}"
        ack = r.json()
        assert ack.get("type") == "shutdown_kernel" and ack.get("ok") is True, (
            f"unexpected ack: {ack}"
        )

    code = await _await_exit(kp)
    _assert_clean_exit(kp, code)
    _assert_lock_released(kp)
    await _await_port_dead(port)


async def _shutdown_via_ws(binary, kernel_factory, parity_tmp, free_port, tag: str) -> None:
    base = parity_tmp(f"shutdown_ws_{tag}")
    workdir = base / "W"
    workdir.mkdir(parents=True)
    port = free_port()

    seed_web(binary, workdir, port)
    seed_web_ws(binary, workdir)
    kp = await kernel_factory(workdir, port)

    # Gating over WS too (the readmes claim the gate holds over BOTH
    # transports): a child-addressed shutdown_kernel must be refused and the
    # kernel must stay up.
    neg = await ws_call(port, "web", "shutdown_kernel")
    assert "error" in neg and "root control surface" in neg["error"], (
        f"WS shutdown_kernel on a child should error, got: {neg}"
    )
    await _assert_still_serving(kp, port)

    _assert_lock_present(kp)

    # `kernel` alias resolves to the root; the ack frame must come back
    # before the socket is torn down.
    ack = await ws_call(port, "kernel", "shutdown_kernel")
    assert ack.get("type") == "shutdown_kernel" and ack.get("ok") is True, (
        f"unexpected ws ack: {ack}"
    )

    code = await _await_exit(kp)
    _assert_clean_exit(kp, code)
    _assert_lock_released(kp)
    await _await_port_dead(port)


@pytest.mark.asyncio
async def test_shutdown_kernel_rest_python(python_binary, python_kernel, parity_tmp, free_port):
    await _shutdown_via_rest(python_binary, python_kernel, parity_tmp, free_port, "py")


@pytest.mark.asyncio
async def test_shutdown_kernel_ws_python(python_binary, python_kernel, parity_tmp, free_port):
    await _shutdown_via_ws(python_binary, python_kernel, parity_tmp, free_port, "py")


@pytest.mark.asyncio
async def test_shutdown_kernel_rest_rust(rust_binary, rust_kernel, parity_tmp, free_port):
    await _shutdown_via_rest(rust_binary, rust_kernel, parity_tmp, free_port, "rs")


@pytest.mark.asyncio
async def test_shutdown_kernel_ws_rust(rust_binary, rust_kernel, parity_tmp, free_port):
    await _shutdown_via_ws(rust_binary, rust_kernel, parity_tmp, free_port, "rs")
