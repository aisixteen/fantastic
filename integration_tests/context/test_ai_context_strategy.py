"""LIVE Context-Protocol validation — one interchangeable backend harness over the two
strategies (`compact` · `truncate`), plus a model-REACTION test.

This is the KEY deliverable of the context work: prove END-TO-END, against a REAL LLM, that

  1. overflow actually FIRES once the conversation outgrows the budget (context_status),
  2. the model KEEPS answering after projection — i.e. the projected message list (incl.
     the prepended context-notice) is a shape the real backend accepts (the tool-pairing
     invariant matters most on nvidia / OpenAI-wire, which rejects an orphaned `role:tool`),
  3. the DURABLE record stays WHOLE — every driven turn is still in the mounted chat
     yaml_state (a strategy projects the MODEL CONTEXT, it never trims the store), and
  4. the model RE-ACTS to a compaction notice — pages a dropped fact back via `recall` or
     persists it to its memory agent (`test_model_reacts_to_compaction_notice`). This is
     the 'probably → measured' close: the reaction is read off `context_status.last_reaction`.

The exact summarized/dropped arithmetic is already pinned by the deterministic unit tests
(`ai_core/tests/test_context.py`); here we assert the live, structured truth surfaces
(`context_status` + `history` + the `mem` store), never the model's prose (except the final
codename recall).

Backend-configurable — the SAME test runs on either backend by one env switch:

    FANTASTIC_TEST_BACKEND = nvidia (DEFAULT, NVAPI, cloud — fast + the stricter
                             OpenAI/tool-pairing wire) | ollama (local; gemma4:12b is
                             ~60 s/turn so it is opt-in only, NEVER the default)
    FANTASTIC_TEST_MODEL    = backend-specific model id
    FANTASTIC_CTX_WINDOW    = the forced small window (default 3000 — must clear the
                             system-block floor or the seam hits the too_small failsafe)
    FANTASTIC_CTX_TURNS     = how many chunky turns to drive (default 10)

    cd integration_tests && uv run pytest context/test_ai_context_strategy.py -s   # nvidia
    FANTASTIC_TEST_BACKEND=ollama … uv run pytest …                                # opt-in, slow

Anthropic is intentionally NOT a backend here (out of scope for this validation).
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

_BACKEND = os.environ.get("FANTASTIC_TEST_BACKEND", "nvidia").lower()
_TURN_TIMEOUT = 240.0

# A small window + a tiny output reserve → a budget that a handful of chunky turns
# overflow, while still leaving the system block + one live turn room (no failsafe).
_CTX_WINDOW = int(os.environ.get("FANTASTIC_CTX_WINDOW", "3000"))
_OUTPUT_RESERVE = 256
_RECENT_N = 4
_TURNS = int(os.environ.get("FANTASTIC_CTX_TURNS", "10"))

_DEFAULT_MODEL = {
    "ollama": "llama3.2",
    "nvidia": "nvidia/nemotron-3-super-120b-a12b",
}
_MODEL = os.environ.get("FANTASTIC_TEST_MODEL", _DEFAULT_MODEL.get(_BACKEND, ""))

_HANDLER = {
    "ollama": "ollama_backend.tools",
    "nvidia": "nvidia_nim_backend.tools",
}


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


def _nvapi() -> str | None:
    return _env_or_dotenv("NVAPI", "NVIDIA_API_KEY")


def _skip_reason() -> str | None:
    if _BACKEND not in ("ollama", "nvidia"):
        return f"FANTASTIC_TEST_BACKEND={_BACKEND!r} unsupported here (ollama|nvidia only)"
    if _BACKEND == "nvidia" and not _nvapi():
        return "NVAPI absent — nvidia context-strategy test (paid)"
    return None  # ollama: attempt against the local server


pytestmark = pytest.mark.skipif(_skip_reason() is not None, reason=_skip_reason() or "")

# Each turn is a chunky, self-contained prompt that does NOT invite tool use (so the
# turns accumulate context predictably rather than spawning tool detours). The marker
# `[[turn N]]` lets the durable-record assertion prove every turn survived in the store.
_TOPICS = [
    "the water cycle — evaporation, condensation, precipitation, and collection",
    "how a bicycle stays upright while moving, in terms of angular momentum",
    "the difference between weather and climate, with two concrete examples",
    "why bread rises — the role of yeast, gluten, and carbon dioxide",
    "how rainbows form from refraction and reflection inside raindrops",
    "the basics of how a lever multiplies force, with the fulcrum analogy",
    "why the sky is blue during the day but red at sunset (Rayleigh scattering)",
    "how a battery stores and releases energy through a chemical reaction",
    "the way honeybees communicate the location of food via the waggle dance",
    "how noise-cancelling headphones use destructive interference of sound",
    "why metals feel colder than wood at the same temperature (conduction)",
    "how a sailboat can sail against the wind by tacking",
]


def _seed_ai(binary, wd, strategy: str) -> None:
    """Seed the AI agent with a forced-small window + the strategy under test. History
    auto-mounts (a `chat` yaml_state through the loader) — no file_bridge_id needed for
    chat; nvidia still gets one (below) for its api_key sidecar."""
    meta = {
        "handler_module": _HANDLER[_BACKEND],
        "agent_id": "ai",
        "model": _MODEL,
        "context_window": _CTX_WINDOW,
        "output_reserve": _OUTPUT_RESERVE,
        "context_strategy": strategy,
        "recent_n": _RECENT_N,
    }
    if _BACKEND == "nvidia":
        # nvidia reads its api_key sidecar via file_bridge_id (chat history does NOT).
        meta["file_bridge_id"] = "llm_files"
    seed_create(binary, wd, **meta)


async def _say(port: int, ai: str, text: str, client_id: str) -> dict:
    return await asyncio.wait_for(
        ws_call(port, ai, "send", text=text, client_id=client_id),
        timeout=_TURN_TIMEOUT,
    )


async def _context_status(port: int, ai: str) -> dict:
    return await asyncio.wait_for(ws_call(port, ai, "context_status"), timeout=30.0)


async def _history(port: int, ai: str, client_id: str) -> list[dict]:
    r = await asyncio.wait_for(ws_call(port, ai, "history", client_id=client_id), timeout=30.0)
    return r.get("messages") or []


@pytest.mark.parametrize("strategy", ["compact", "truncate"])
async def test_strategy_fires_and_keeps_durable_record(
    python_binary, python_kernel, parity_tmp, free_port, strategy
):
    wd = parity_tmp(f"ctx_strategy_{_BACKEND}_{strategy}") / "host"
    wd.mkdir(parents=True, exist_ok=True)
    env_src = _REPO / ".env"
    if env_src.exists():
        shutil.copyfile(env_src, wd / ".env")

    port = free_port()
    seed_web(python_binary, wd, port)
    seed_web_ws(python_binary, wd)
    # The `.fantastic` store — the loader DISCOVERS it (record persistence) AND it backs
    # the AI's auto-mounted chat yaml_state (history persists THROUGH the loader onto it).
    seed_create(
        python_binary,
        wd,
        handler_module="file_bridge.tools",
        agent_id="llm_files",
        root=".fantastic",
        ingress_rule="allow_all",
    )
    _seed_ai(python_binary, wd, strategy)

    await python_kernel(wd, port)
    ai = "ai"
    client = "chat"

    if _BACKEND == "nvidia":
        await asyncio.wait_for(ws_call(port, ai, "set_api_key", api_key=_nvapi()), timeout=30.0)

    print(
        f"\n[backend={_BACKEND} model={_MODEL} strategy={strategy} "
        f"window={_CTX_WINDOW} reserve={_OUTPUT_RESERVE} turns={_TURNS}]"
    )

    # Sanity: the budget posture is what we configured, BEFORE any overflow.
    st0 = await _context_status(port, ai)
    assert st0["context_window"] == _CTX_WINDOW, st0
    assert st0["strategy"] == strategy, st0
    assert st0["budget"] == max(_CTX_WINDOW - _OUTPUT_RESERVE, 256), st0
    print(f"[budget] {st0['budget']} tok; last_projection={st0['last_projection']}")

    # Drive chunky turns until well past the budget. Each turn is tagged so we can prove
    # it survived in the durable store even after the model-context was projected.
    markers: list[str] = []
    for i in range(_TURNS):
        topic = _TOPICS[i % len(_TOPICS)]
        marker = f"[[turn {i}]]"
        markers.append(marker)
        text = (
            f"{marker} Please explain {topic}. "
            "Answer in two or three full sentences, plainly, no lists. "
            "Keep going in your own words even if it feels repetitive."
        )
        r = await _say(port, ai, text, client)
        assert "error" not in r, (
            f"turn {i} errored — projection produced a shape the {_BACKEND} backend "
            f"rejected, or the failsafe tripped: {r}"
        )

    st = await _context_status(port, ai)
    lp = st.get("last_projection") or {}
    print(f"[final] last_projection={lp}")

    # 1. Overflow FIRED — the conversation outgrew the budget and the strategy ran.
    assert lp.get("fired") is True, (
        f"{strategy}: overflow never fired over {_TURNS} turns at window {_CTX_WINDOW}; "
        f"status={st}. Raise FANTASTIC_CTX_TURNS or lower FANTASTIC_CTX_WINDOW."
    )
    assert lp.get("strategy") == strategy, lp
    assert lp.get("dropped_turns", 0) > 0, f"{strategy}: nothing dropped — {lp}"

    # 2. Strategy-specific projection shape (the live counterpart of the unit tests).
    if strategy == "compact":
        assert lp.get("summarized") is True, f"{strategy} must summarize — {lp}"
    else:  # truncate never calls the summarizer
        assert lp.get("summarized") is False, f"truncate must not summarize — {lp}"
        assert lp.get("kept_turns", 0) >= 1, lp

    # 3. DURABLE record intact — EVERY driven turn is still in the chat yaml_state, even
    #    though the model only ever saw a projected subset. A strategy shapes context,
    #    never the store.
    msgs = await _history(port, ai, client)
    dump = json.dumps(msgs)
    missing = [m for m in markers if m not in dump]
    user_turns = [m for m in msgs if m.get("role") == "user"]
    print(f"[durable] {len(msgs)} messages, {len(user_turns)} user turns; missing={missing}")
    assert not missing, (
        f"{strategy}: durable record lost turns {missing} — the strategy trimmed the "
        f"STORE, not just the projection (it must never touch the store)."
    )
    assert len(user_turns) >= _TURNS, (
        f"{strategy}: expected >= {_TURNS} user turns persisted, got {len(user_turns)}"
    )


async def _mem_dump(port: int, mem: str) -> str:
    r = await asyncio.wait_for(ws_call(port, mem, "read"), timeout=30.0)
    return json.dumps(r).lower()


async def test_model_reacts_to_compaction_notice(
    python_binary, python_kernel, parity_tmp, free_port
):
    """Close the 'probably' gap: prove the model RE-ACTS to a compaction notice, not just
    that compaction fires. Uses `truncate` (NO summary) and plants a distinctive fact in a
    MIDDLE turn — so after compaction it is GONE from the model's live view entirely, and
    the ONLY way to answer is a protocol reaction: `recall` it back, or persist it to the
    memory agent. We assert on `context_status.last_reaction` (the derived 'ack') + the
    durable store, not prose."""
    strategy = "truncate"
    wd = parity_tmp(f"ctx_react_{_BACKEND}") / "host"
    wd.mkdir(parents=True, exist_ok=True)
    env_src = _REPO / ".env"
    if env_src.exists():
        shutil.copyfile(env_src, wd / ".env")

    port = free_port()
    seed_web(python_binary, wd, port)
    seed_web_ws(python_binary, wd)
    seed_create(
        python_binary,
        wd,
        handler_module="file_bridge.tools",
        agent_id="llm_files",
        root=".fantastic",
        ingress_rule="allow_all",
    )
    _seed_ai(python_binary, wd, strategy)
    # A memory agent in the menu — NO file_bridge_id (persists THROUGH the loader). Gives
    # the model the persist reaction path (the notice names it); it's discovered emergently.
    seed_create(python_binary, wd, handler_module="yaml_state.tools", agent_id="mem", mode="mem")

    await python_kernel(wd, port)
    ai, mem, client = "ai", "mem", "chat"
    if _BACKEND == "nvidia":
        await asyncio.wait_for(ws_call(port, ai, "set_api_key", api_key=_nvapi()), timeout=30.0)

    print(f"\n[reaction backend={_BACKEND} model={_MODEL} strategy={strategy}]")

    # Turn 0 = first (truncate KEEPS the first turn) — neutral framing.
    await _say(port, ai, "Hi! Let's talk through some everyday science.", client)
    # Turn 1 = the planted fact. truncate keeps first + last recent_n, so this MIDDLE turn
    # is dropped from the live view once enough turns accrue — forcing a reaction to recover.
    await _say(
        port,
        ai,
        "Important: my project codename is HALCYON — please keep that in mind.",
        client,
    )
    # Drive chunky filler turns to push turn 1 out of the live window (compaction fires).
    for i in range(_TURNS):
        topic = _TOPICS[i % len(_TOPICS)]
        await _say(
            port,
            ai,
            f"[[fill {i}]] Please explain {topic}. Two or three full sentences, plainly.",
            client,
        )

    lp = (await _context_status(port, ai)).get("last_projection") or {}
    print(f"[final] last_projection={lp}")
    assert lp.get("fired") is True, (
        f"compaction never fired over {_TURNS + 2} turns; raise FANTASTIC_CTX_TURNS or "
        f"lower FANTASTIC_CTX_WINDOW. status={lp}"
    )

    # The decisive turn — answering REQUIRES the dropped fact. No leading hint about how.
    ans = await _say(port, ai, "What is my project codename?", client)
    final = (ans.get("final") or ans.get("response") or "").lower()

    st = await _context_status(port, ai)
    reaction = st.get("last_reaction") or {}
    dump = await _mem_dump(port, mem)
    print(f"[reaction] last_reaction={reaction}  mem_has_halcyon={'halcyon' in dump}")
    print(f"[answer] {final!r}")

    # PRIMARY (hard): the model took a protocol-taught ACTION in response to the notice —
    # paged history back via `recall`, or persisted to its memory agent. This is the
    # measured 'probably → reacted': the model REACHES for the protocol, not just hopes.
    assert reaction.get("recalled") or reaction.get("persisted") or ("halcyon" in dump), (
        "model did NOT react to the compaction notice (neither recalled the dropped turn "
        f"nor persisted to memory). last_reaction={reaction} mem={dump[:200]}"
    )

    # MECHANISM (hard): compaction is LOSSLESS on demand — calling `recall` ourselves pages
    # the dropped planted turn straight back from the durable store, regardless of how the
    # model phrased its own answer. Proves the recovery path the notice advertises is real.
    rc = await asyncio.wait_for(
        ws_call(port, ai, "recall", query="codename", client_id=client), timeout=30.0
    )
    recalled_dump = json.dumps(rc.get("messages") or []).lower()
    assert "halcyon" in recalled_dump, (
        f"recall did not page the dropped fact back — losslessness broken. recall={rc}"
    )

    # OBSERVATION (soft): whether the model's OWN prose recovered the fact is emergent /
    # model-quality dependent (a weak model may react yet still answer poorly) — we print
    # it but do not gate on it. The protocol's job is to make the fact RECOVERABLE (asserted
    # above) and to get the model to REACH for it (asserted above), not to grade phrasing.
    if "halcyon" not in final:
        print(f"[note] model reacted but its prose missed the codename: {final!r}")
