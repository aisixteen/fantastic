"""Container smoke — the shipped image loads the DE-LOOPED AI backends + the
script executor, and a durable authored agent (a "mind") boots and is
addressable.

This is the container-side proof of the de-loop work: every LLM backend became a
context-managed conversational completion with the tool loop REMOVED (verbs:
send·history·recall·context_status·…, and NO `refresh_menu`), while the agentic
loop is now emergent — an authored agent on the ONE script executor
(`exec_bridge`, `shim:'python'`) drives the tool loop itself.

Runs ONLY under `FANTASTIC_TARGET=container` (skips otherwise). It is PROGRAMMED
but not run in the default local sweep — set the env + build the image
(`sh container/build.sh`) to exercise it. No `podman`/`docker` is invoked at
collection time; the launcher self-gates (skips if the engine/image is absent).
"""

from __future__ import annotations

import os
from pathlib import Path
from typing import Callable

import pytest

from helpers.kernel_proc import KernelProc
from helpers.seeding import seed_create, seed_web, seed_web_ws

pytestmark = pytest.mark.skipif(
    os.environ.get("FANTASTIC_TARGET", "local").strip().lower() != "container",
    reason="container smoke; set FANTASTIC_TARGET=container + build the image to run",
)

# A durable authored agent: on each `ping` inbox turn it echoes back. Proves the
# exec_bridge durable-agent + addressable-inbox path loads and boots in-image.
_ECHO_CODE = (
    "import time\n"
    "def _on(m):\n"
    "    if isinstance(m, dict) and m.get('type') == 'ping':\n"
    "        kernel.emit({'type': 'echo', 'text': m.get('text'),\n"
    "                     'client_id': m.get('client_id')})\n"
    "kernel.on_message(_on)\n"
    "kernel.reflect('kernel')  # FIFO round-trip: on_message registered upstream\n"
    "print('READY', flush=True)\n"
    "while True:\n"
    "    time.sleep(3600)\n"
)


@pytest.mark.asyncio
async def test_container_deloop_backends_and_mind_boot(
    python_binary: Path,
    python_kernel: Callable[..., KernelProc],
    parity_tmp: Callable[[str], Path],
    free_port: Callable[[], int],
) -> None:
    wd = parity_tmp("deloop_smoke")
    port = free_port()
    seed_web(python_binary, wd, port)
    seed_web_ws(python_binary, wd)
    # A de-looped AI backend + a store it could persist history THROUGH.
    seed_create(
        python_binary,
        wd,
        handler_module="file_bridge.tools",
        agent_id="store",
        ingress_rule="allow_all_ingress",
    )
    seed_create(
        python_binary,
        wd,
        handler_module="ollama_backend.tools",
        agent_id="the_ai",
        file_bridge_id="store",
    )
    # A durable authored agent (the "mind" seat) on the ONE script executor. The leg
    # is SEALED by default and has NO interpreter until the operator names one, so an
    # in-image interpreter + `shim=python` + an open ingress are all required to run.
    seed_create(
        python_binary,
        wd,
        handler_module="exec_bridge.tools",
        agent_id="the_mind",
        interpreters='["/opt/fantastic/venv/bin/python"]',
        shim="python",
        ingress_rule="allow_all_ingress",
    )

    kernel = await python_kernel(wd, port)

    # 1) The AI backend loaded DE-LOOPED: context verbs present, tool-loop menu gone.
    ai = await kernel.call("the_ai", "reflect")
    verbs = ai.get("verbs", {})
    assert "recall" in verbs and "context_status" in verbs, (
        f"de-looped context verbs missing: {sorted(verbs)}"
    )
    assert "refresh_menu" not in verbs, "refresh_menu should be gone (loop removed)"
    assert "send" in verbs and "history" in verbs

    # 2) The bundle catalog carries every de-looped backend + the script executor.
    root = await kernel.call("kernel", "reflect", bundles="ids")
    names = set(root.get("bundles", []))
    assert {
        "ollama_backend",
        "nvidia_nim_backend",
        "anthropic_backend",
        "exec_bridge",
    } <= names, f"expected de-looped backends + exec_bridge in catalog, got {sorted(names)}"

    # 3) A durable authored agent boots on the script executor + is addressable:
    #    an unknown verb is queued to its running job's inbox (the emergent-loop
    #    seat — the mind receives turns via on_message).
    started = await kernel.call("the_mind", "start", code=_ECHO_CODE)
    assert started.get("job_id"), f"exec_bridge start should return a job_id: {started}"
    ack = await kernel.call("the_mind", "send", type="ping", text="hi", client_id="c")
    assert ack.get("queued") is True, f"addressable-inbox ack expected: {ack}"
