"""README-ONLY memory DISCOVERY — does an AI, given ONLY the system's self-description
(the auto-built agent menu + reflect/readme), DISCOVER that a durable memory agent exists
and USE it with judgment — AFTER the persistence refactor (yaml_state persists THROUGH the
loader, nothing to wire)?

This is the gate before context-management: the upcoming MemGPT-style memory offload
ASSUMES emergent memory works. The refactor rewrote yaml_state's self-description (no more
`file_bridge_id`; now "persists through the loader, just set/read"), so we prove the NEW
readme still teaches the model the memory model.

Difference from `test_ai_memory_judgment.py`: that test HARD-CODES the memory recipe in a
`system_prompt` override (naming the mem id + the exact send calls). Here we pass NO
`system_prompt` — the model gets the DEFAULT assembled prompt (primer + the live agent
MENU, which carries `mem`'s reflect sentence + verbs + readme + the send-howto). The model
must discover `mem` and learn `set`/`read` ITSELF. Capability from self-description — the
north-star.

Also: `mem` (yaml_state) is seeded with NO `file_bridge_id` — it persists THROUGH the loader
(the refactor). The asserts are on the yaml_state STORE (truth), never the model's prose,
except the fresh-client recall turn.

Backend-configurable for the "how does it feel / is the window enough" experiment:

    FANTASTIC_TEST_BACKEND = anthropic (default) | ollama | nvidia
    FANTASTIC_TEST_MODEL    = backend-specific model id
    FANTASTIC_NUM_CTX       = ollama num_ctx (e.g. 32768, 65536)

    cd integration_tests && uv run pytest memory/test_ai_memory_discovery.py -s
    FANTASTIC_TEST_BACKEND=ollama FANTASTIC_NUM_CTX=32768 \
        FANTASTIC_TEST_MODEL=llama3.2 uv run pytest memory/test_ai_memory_discovery.py -s
"""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import sys
from pathlib import Path

import pytest

_HERE = Path(__file__).resolve().parent
_INTEG = _HERE.parent
_REPO = _INTEG.parent
if str(_INTEG) not in sys.path:
    sys.path.insert(0, str(_INTEG))

from helpers.seeding import seed_create, seed_web, seed_web_ws  # noqa: E402
from helpers.ws import ws_call  # noqa: E402

_BACKEND = os.environ.get("FANTASTIC_TEST_BACKEND", "anthropic").lower()
_TURN_TIMEOUT = 240.0

_DEFAULT_MODEL = {
    "anthropic": "claude-sonnet-4-6",
    "ollama": "llama3.2",
    "nvidia": "nvidia/nemotron-3-super-120b-a12b",
}
_MODEL = os.environ.get("FANTASTIC_TEST_MODEL", _DEFAULT_MODEL.get(_BACKEND, ""))


def _env_or_dotenv(*names: str) -> str | None:
    for n in names:
        v = os.environ.get(n)
        if v:
            return v
    envf = _REPO / ".env"
    if envf.exists():
        for raw in envf.read_text().splitlines():
            line = raw.strip()
            for n in names:
                if line.startswith(f"{n}="):
                    return line[len(n) + 1 :].strip().strip('"').strip("'")
    return None


def _anthropic_key() -> str | None:
    return _env_or_dotenv("ANTHROPIC_KEY", "ANTHROPIC_API_KEY")


def _nvapi() -> str | None:
    return _env_or_dotenv("NVAPI", "NVIDIA_API_KEY")


def _skip_reason() -> str | None:
    if _BACKEND == "anthropic" and not _anthropic_key():
        return "ANTHROPIC_KEY absent — paid/rare AI memory-discovery test"
    if _BACKEND == "nvidia" and not _nvapi():
        return "NVAPI absent — nvidia memory-discovery test"
    if _BACKEND not in ("anthropic", "ollama", "nvidia"):
        return f"unknown FANTASTIC_TEST_BACKEND={_BACKEND!r}"
    return None  # ollama: attempt against the local server


pytestmark = pytest.mark.skipif(_skip_reason() is not None, reason=_skip_reason() or "")

_HANDLER = {
    "anthropic": "anthropic_backend.tools",
    "ollama": "ollama_backend.tools",
    "nvidia": "nvidia_nim_backend.tools",
}


def _seed_ai(binary, wd) -> None:
    """Seed the AI agent for the selected backend. All wire `file_bridge_id` for their
    own CHAT transcript (ai_core is out of the persistence refactor); only `mem` drops it."""
    meta = {
        "handler_module": _HANDLER[_BACKEND],
        "agent_id": "ai",
        "file_bridge_id": "llm_files",
        "model": _MODEL,
    }
    if _BACKEND == "ollama":
        nc = os.environ.get("FANTASTIC_NUM_CTX")
        if nc:
            meta["num_ctx"] = int(nc)
    seed_create(binary, wd, **meta)


async def _say(port: int, ai: str, text: str, client_id: str) -> dict:
    """One full AI turn over WS — NO system_prompt override, so the model sees the
    DEFAULT assembled prompt (primer + live agent menu + send-howto) and must discover
    the memory agent itself."""
    return await asyncio.wait_for(
        ws_call(port, ai, "send", text=text, client_id=client_id),
        timeout=_TURN_TIMEOUT,
    )


async def _mem_dump(port: int, mem: str) -> str:
    r = await asyncio.wait_for(ws_call(port, mem, "read"), timeout=30.0)
    return json.dumps(r).lower()


async def _mem_keys(port: int, mem: str) -> list[str]:
    r = await asyncio.wait_for(ws_call(port, mem, "keys"), timeout=30.0)
    ks = r.get("keys")
    return ks if isinstance(ks, list) else []


async def test_ai_discovers_and_uses_memory_from_readmes(
    python_binary, python_kernel, parity_tmp, free_port
):
    wd = parity_tmp(f"ai_memory_discovery_{_BACKEND}") / "host"
    wd.mkdir(parents=True, exist_ok=True)
    env_src = _REPO / ".env"
    if env_src.exists():
        shutil.copyfile(env_src, wd / ".env")

    port = free_port()

    seed_web(python_binary, wd, port)
    seed_web_ws(python_binary, wd)
    # The `.fantastic` store — the loader DISCOVERS it (record persistence) AND it backs
    # the AI's own chat transcript. yaml_state persists THROUGH the loader onto it too.
    seed_create(
        python_binary,
        wd,
        handler_module="file_bridge.tools",
        agent_id="llm_files",
        root=".fantastic",
        ingress_rule="allow_all",
    )
    _seed_ai(python_binary, wd)
    # The memory agent — NO file_bridge_id. It persists THROUGH the loader (the refactor).
    seed_create(
        python_binary,
        wd,
        handler_module="yaml_state.tools",
        agent_id="mem",
        mode="mem",
    )

    await python_kernel(wd, port)
    ai, mem = "ai", "mem"

    if _BACKEND == "nvidia":
        await asyncio.wait_for(
            ws_call(port, ai, "set_api_key", api_key=_nvapi()), timeout=30.0
        )

    print(f"\n[backend={_BACKEND} model={_MODEL} num_ctx={os.environ.get('FANTASTIC_NUM_CTX')}]")

    # Turn 1 — SALIENT: the model must discover `mem` from the menu and SAVE a fact.
    await _say(
        port, ai,
        "Hi! I'm Ada, and from now on I'd like all answers in metric units.", "chat",
    )
    dump = await _mem_dump(port, mem)
    print(f"[after save]   store={dump}")
    assert "ada" in dump, f"AI did not discover+use memory to save 'Ada'; store={dump}"
    keys_after_save = await _mem_keys(port, mem)

    # Turn 2 — TRIVIA: should NOT store throwaway arithmetic.
    await _say(port, ai, "Quick one: what is 2 + 2?", "chat")
    keys_after_trivia = await _mem_keys(port, mem)
    print(f"[after trivia] keys {keys_after_save} -> {keys_after_trivia}")
    assert len(keys_after_trivia) <= len(keys_after_save), (
        f"AI stored trivia (keys grew {keys_after_save} -> {keys_after_trivia})"
    )

    # Turn 3 — RECALL on a FRESH client (no transcript): a correct answer can ONLY
    # come from reading `mem`.
    t3 = await _say(port, ai, "What's my name, and which units do I prefer?", "fresh")
    resp3 = (t3.get("response") or "").lower()
    print(f"[recall reply] {resp3!r}")
    assert "ada" in resp3, f"fresh-context recall failed; reply={resp3!r}"

    # Turn 4 — UPDATE.
    await _say(port, ai, "Actually, please use my full name: Ada Lovelace.", "chat")
    dump4 = await _mem_dump(port, mem)
    print(f"[after update] store={dump4}")
    assert "lovelace" in dump4, f"update not stored; store={dump4}"

    # Turn 5 — FORGET.
    await _say(port, ai, "Please forget my name entirely.", "chat")
    dump5 = await _mem_dump(port, mem)
    print(f"[after forget] store={dump5}")
    # Robust forget check: the distinctive surname proves the NAME entry is gone — a
    # kept preference value may incidentally mention "ada" (e.g. "user (ada) prefers…").
    assert "lovelace" not in dump5, f"name not forgotten; store={dump5}"
