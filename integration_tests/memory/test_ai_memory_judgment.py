"""Behavioral memory test — does an AI backend use a DURABLE yaml_state memory
WHEN NEEDED and NOT EXCESSIVELY?

This is the pinnacle of the substrate's "memory is just another agent" claim:
an AI is TOLD a `yaml_state` memory agent exists (its id + the send-howto) but
decides ITSELF when to save / recall / forget. We assert on the yaml_state
STORE (RAM/disk is truth), never on the model's free-text prose — except the
one recall turn, where a fresh, history-less client means a correct answer can
ONLY have come from memory.

Scenario (single live daemon, driven over WS):

  1. salient -> the AI should SAVE a lasting fact            (store gains "Ada")
  2. trivia  -> the AI should NOT save a throwaway compute   (key-count flat)
  3. recall  -> FRESH client, no transcript: answer needs memory  (reply has "Ada")
  4. update  -> the AI should UPDATE the stored fact         (store gains "Lovelace")
  5. forget  -> the AI should DELETE it                      (store loses "Ada"/"Lovelace")

So the metric is JUDGMENT — precision (don't store trivia) AND recall (retrieve
when needed) — not merely "can it persist once."

PAID + RARE: drives the in-kernel `anthropic_backend` (real tokens). Skipped
unless ANTHROPIC_KEY (env or repo `.env`) is present, so it never runs in CI's
free deterministic suite. Run it manually:

    cd integration_tests && uv run pytest memory/ -s

FINDING (documented, not fixed here): the Python backends (ollama/anthropic/nim)
carry conversational history ONLY as the per-client chat transcript keyed by
`file_bridge_id`. They do NOT auto-mount or auto-inject `yaml_state` into the
prompt. The Swift Apple-FM backend already does (mountMemoryAgents +
always-inject). This test exercises the on-demand tool-call path — the AI
explicitly sends `{"type":"set"|"read"|"delete", ...}` to the `mem` agent — and
confirms that path works correctly on the Python/Anthropic backend.
"""

from __future__ import annotations

import asyncio
import json
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

# Capable tool-use model, cheaper than opus. Override with FANTASTIC_TEST_MODEL.
_MODEL = __import__("os").environ.get("FANTASTIC_TEST_MODEL", "claude-sonnet-4-6")
# Generous timeout per turn: an AI turn includes multiple tool-call round-trips;
# the backend's own HTTP deadline is 180 s, so we cap the WS wait at 240 s.
_TURN_TIMEOUT = 240.0


def _anthropic_key() -> str | None:
    import os

    k = os.environ.get("ANTHROPIC_KEY") or os.environ.get("ANTHROPIC_API_KEY")
    if k:
        return k
    envf = _REPO / ".env"
    if envf.exists():
        for raw in envf.read_text().splitlines():
            line = raw.strip()
            for name in ("ANTHROPIC_KEY=", "ANTHROPIC_API_KEY="):
                if line.startswith(name):
                    return line[len(name) :].strip().strip('"').strip("'")
    return None


pytestmark = pytest.mark.skipif(
    _anthropic_key() is None,
    reason="ANTHROPIC_KEY absent — paid/rare AI memory-judgment test",
)


def _sys_prompt(mem_id: str) -> str:
    # A `system_prompt` override REPLACES the auto-built agent menu, so we must
    # name the memory agent's id + the send-howto here. We TELL it memory
    # exists (matching "say to it we have yaml_state"); WHEN to use it is its
    # call — that judgment is exactly what the scenario measures.
    return (
        f"You are a helpful assistant with a DURABLE MEMORY agent (id: {mem_id}). "
        "Reach it with your send tool:\n"
        f'  remember: send("{mem_id}", {{"type":"set","key":"<short.key>","value":"<value>"}})\n'
        f'  recall:   send("{mem_id}", {{"type":"read"}})\n'
        f'  list:     send("{mem_id}", {{"type":"keys"}})\n'
        f'  forget:   send("{mem_id}", {{"type":"delete","key":"<key>"}})\n'
        "Use memory JUDICIOUSLY: store SALIENT, lasting facts about the user "
        "(their name, durable preferences) and recall them when relevant. Do NOT "
        "store trivia, greetings, or one-off arithmetic. Keep keys short and "
        "stable so you can update or delete them later. Always answer the user "
        "normally as well."
    )


async def _say(port: int, ai: str, text: str, client_id: str, mem: str) -> dict:
    """One full AI turn over WS; waits until the model's reply lands
    (after any tool-calls it chooses to make).

    `client_id` is forwarded to the `file_bridge_id` transcript store so
    separate client ids yield separate conversation histories — the
    recall test (turn 3) exploits this by using a fresh `client_id` that
    has no prior transcript, forcing the AI to read from `yaml_state`.
    """
    return await asyncio.wait_for(
        ws_call(
            port,
            ai,
            "send",
            text=text,
            client_id=client_id,
            system_prompt=_sys_prompt(mem),
        ),
        timeout=_TURN_TIMEOUT,
    )


async def _mem_dump(port: int, mem: str) -> str:
    """Read the full yaml_state store and return it as a lower-cased JSON
    string for substring assertions."""
    r = await asyncio.wait_for(ws_call(port, mem, "read"), timeout=30.0)
    return json.dumps(r).lower()


async def _mem_keys(port: int, mem: str) -> list[str]:
    """Return the list of keys currently held in the yaml_state store."""
    r = await asyncio.wait_for(ws_call(port, mem, "keys"), timeout=30.0)
    ks = r.get("keys")
    return ks if isinstance(ks, list) else []


async def test_ai_uses_memory_when_needed_not_excessively(
    python_binary, python_kernel, parity_tmp, free_port
):
    wd = parity_tmp("ai_memory_judgment") / "host"
    wd.mkdir(parents=True, exist_ok=True)
    # The daemon runs with cwd=wd; copy the repo .env so its cwd-relative
    # dotenv load finds ANTHROPIC_KEY (mirrors boot_bare_host.sh).
    env_src = _REPO / ".env"
    if env_src.exists():
        shutil.copyfile(env_src, wd / ".env")

    port = free_port()

    # Seed WS surface + file-history store + AI (anthropic) + yaml_state memory.
    # Agent ids 'ai' and 'mem' are concrete/literal — NOT the "kernel" routing
    # alias. Using literal ids here is correct: the test explicitly targets
    # these named agents, not the runtime root.
    seed_web(python_binary, wd, port)
    seed_web_ws(python_binary, wd)
    seed_create(
        python_binary,
        wd,
        handler_module="file_bridge.tools",
        agent_id="llm_files",
        root=".fantastic",
        ingress_rule="allow_all",  # the fs edge seals by default - open the backing
    )
    seed_create(
        python_binary,
        wd,
        handler_module="anthropic_backend.tools",
        agent_id="ai",
        file_bridge_id="llm_files",
        model=_MODEL,
    )
    seed_create(
        python_binary,
        wd,
        handler_module="yaml_state.tools",
        agent_id="mem",
        mode="mem",
        # yaml_state persists THROUGH a gated file_bridge (deny-all by default) — wire
        # the same opened provider the AI uses; read/write are symmetric through it.
        file_bridge_id="llm_files",
    )

    await python_kernel(wd, port)
    ai, mem = "ai", "mem"

    # Turn 1 — SALIENT: the AI should SAVE a lasting fact to memory.
    await _say(
        port, ai, "Hi! I'm Ada, and from now on I'd like all answers in metric units.", "chat", mem
    )
    dump = await _mem_dump(port, mem)
    print(f"\n[after save]  store={dump}")
    assert "ada" in dump, f"expected the AI to remember 'Ada'; store={dump}"
    keys_after_save = await _mem_keys(port, mem)

    # Turn 2 — TRIVIA: the AI should NOT store throwaway arithmetic (non-excessive).
    await _say(port, ai, "Quick one: what is 2 + 2?", "chat", mem)
    keys_after_trivia = await _mem_keys(port, mem)
    print(f"[after trivia] keys {keys_after_save} -> {keys_after_trivia}")
    assert len(keys_after_trivia) <= len(keys_after_save), (
        f"AI stored trivia (keys grew {keys_after_save} -> {keys_after_trivia})"
    )

    # Turn 3 — RECALL on a FRESH client (no transcript):
    # `client_id="fresh"` means the file-transcript store for this client is
    # empty — the AI has no conversation history to draw on. A correct answer
    # ("Ada", "metric") can ONLY come from an explicit read of `yaml_state`.
    t3 = await _say(port, ai, "What's my name, and which units do I prefer?", "fresh", mem)
    resp3 = (t3.get("response") or "").lower()
    print(f"[recall reply] {resp3!r}")
    assert "ada" in resp3, (
        f"fresh-context recall failed — the AI did not retrieve the saved name "
        f"from memory; reply={resp3!r}"
    )

    # Turn 4 — UPDATE: the AI should overwrite the stored name with the full form.
    await _say(port, ai, "Actually, please use my full name: Ada Lovelace.", "chat", mem)
    dump4 = await _mem_dump(port, mem)
    print(f"[after update] store={dump4}")
    assert "lovelace" in dump4, f"update not stored; store={dump4}"

    # Turn 5 — FORGET: the AI should DELETE the name entry entirely.
    await _say(port, ai, "Please forget my name entirely.", "chat", mem)
    dump5 = await _mem_dump(port, mem)
    print(f"[after forget] store={dump5}")
    assert "lovelace" not in dump5 and "ada" not in dump5, (
        f"name not forgotten — memory still holds it; store={dump5}"
    )
