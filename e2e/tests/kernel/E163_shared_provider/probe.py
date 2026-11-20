"""kernel/shared_provider — two `ai_chat.tools` chats on ONE provider (#632). NO MODEL.

Scripted OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model behind one
`vllm_backend` PROVIDER. The provider's record is its identity (endpoint, model, window, KV
policy); the request knobs are each chat's and ride its every request. Rust lane
(`runtimes: [rust]`), driven over `web_rest` on the packaged host.

  1. admitted: the creator names the provider for each chat and admits each on the provider's
     `callable_by` (`update_agent`, read-add-write);
  2. own knobs: each chat's request carries its own temperature / max_tokens / top_k, and the
     provider's record holds none;
  3. KV accounting: the provider's reflect shows both conversations live;
  4. lifecycle: a change to the provider's endpoint reaches a chat's next request; with both
     chats deleted the provider stays.
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
PROV, CA, CB = "shared_prov", "chat_a", "chat_b"
MODEL = "mock-shared"
COMPLETE = ["complete", "watch", "reflect"]
DONE = '\n<call name="done"></call>'
KNOBS = ("temperature", "top_p", "top_k", "min_p", "reasoning_effort", "thinking_token_budget",
         "presence_penalty", "frequency_penalty", "repetition_penalty", "no_repeat_ngram_size",
         "seed", "stop", "max_tokens")

bad: list[str] = []


def need(cond: bool, what: str) -> bool:
    print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
    if not cond:
        bad.append(what)
    return cond


def short(v, n: int = 240) -> str:
    return str(v)[:n]


def record(hk: HostKernel, aid: str) -> dict:
    r = hk.send("kernel", {"type": "list_agents"})
    return next((a for a in r.get("agents") or [] if a.get("id") == aid), {})


def admit(hk: HostKernel, target: str, who: str, verbs) -> None:
    """The creator's side of the gate: read the target's `callable_by`, add `who`, write it whole."""
    cb = dict(record(hk, target).get("callable_by") or {})
    r = hk.send(hk.root_id, {"type": "update_agent", "id": target, "callable_by": {**cb, who: verbs}})
    need(r.get("updated") is True, f"{target} admits {who}: {short(r, 160)}")


def chat(hk: HostKernel, cid: str, knobs: dict) -> None:
    """A chat on the provider the creator names, with its own knobs; then the creator admits it."""
    rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=cid,
                    provider_id=PROV, model=MODEL, context_window=262144,
                    memory_store_id="memory", **knobs,
                    can_call={"$self": "*", "memory": "*", PROV: COMPLETE},
                    callable_by={"$self": "*", "$parent": "*", "rest": "*"})
    need(rec.get("id") == cid, f"create chat {cid} on {PROV}: {short(rec, 160)}")
    admit(hk, PROV, cid, COMPLETE)


def idle(hk: HostKernel, aid: str, bound: float = 60) -> bool:
    end = time.monotonic() + bound
    while time.monotonic() < end:
        tasks = hk.send("kernel", {"type": "ps"}).get("tasks") or []
        if not any(t.get("agent") == aid for t in tasks):
            return True
        time.sleep(0.05)
    return False


def turns_at_least(m: VllmMock, n: int, bound: float = 60) -> list[dict]:
    end = time.monotonic() + bound
    while time.monotonic() < end:
        if len(m.turns()) >= n:
            break
        time.sleep(0.05)
    return m.turns()


def turn_of(hk: HostKernel, m: VllmMock, cid: str, text: str, answer: str, n: int) -> dict:
    """One turn of `cid`; the request the server saw (its `n`-th), or {}."""
    m.reply(answer + DONE)
    out = hk.send(cid, {"type": "send", "text": text})
    need(not out.get("error"), f"{cid} accepts a send: {short(out, 160)}")
    seen = turns_at_least(m, n)
    need(idle(hk, cid), f"{cid}'s turn settles")
    need(answer in json.dumps(hk.send(cid, {"type": "history", "limit": 500}), default=str),
         f"{cid}'s history holds its answer")
    return seen[n - 1]["body"] if len(seen) >= n else {}


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    m, m2 = VllmMock(model=MODEL).start(), VllmMock(model=MODEL).start()
    work = Path(tempfile.mkdtemp(prefix="e2e-shared-provider-"))
    hk = HostKernel(spec, work, reach=[PROV, CA, CB]).boot()
    try:
        for who in (PROV, CA, CB):
            admit(hk, "memory", who, "*")
        rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=PROV,
                        endpoint=m.url, model=MODEL, context_window=262144, kv_slots=4,
                        kv_overflow="evict", memory_store_id="memory",
                        can_call={"$self": "*", "memory": "*"},
                        callable_by={"$self": "*", "$parent": "*"})
        need(rec.get("id") == PROV, f"create provider {PROV}: {short(rec, 160)}")

        # ── 1 · two chats named on it, each admitted by its creator ──
        print("\n── 1 · admitted ──", flush=True)
        chat(hk, CA, {"temperature": 0.3, "max_tokens": 8192})
        chat(hk, CB, {"temperature": 0.9, "max_tokens": 4096, "top_k": 7})
        cb = record(hk, PROV).get("callable_by") or {}
        need(cb.get(CA) == COMPLETE and cb.get(CB) == COMPLETE,
             f"the provider admits both chats: {short(cb, 200)}")
        held = [k for k in KNOBS if k in record(hk, PROV)]
        need(not held, f"the provider's record holds no request knob: {held}")

        # ── 2 · each request carries its own chat's knobs ──
        print("\n── 2 · own knobs ──", flush=True)
        a = turn_of(hk, m, CA, "Say the first thing.", "Answer A.", 1)
        b = turn_of(hk, m, CB, "Now something else entirely.", "Answer B.", 2)
        need((a.get("temperature"), a.get("max_tokens")) == (0.3, 8192),
             f"{CA}'s request carries its own knobs: temperature={a.get('temperature')} "
             f"max_tokens={a.get('max_tokens')}")
        need((b.get("temperature"), b.get("max_tokens"), b.get("top_k")) == (0.9, 4096, 7),
             f"{CB}'s request carries its own knobs: temperature={b.get('temperature')} "
             f"max_tokens={b.get('max_tokens')} top_k={b.get('top_k')}")
        need(a.get("top_k") == 20, f"a knob {CA} leaves out is the backend's default: top_k={a.get('top_k')}")
        need(a.get("model") == MODEL and b.get("model") == MODEL, "both requests name the provider's model")

        # ── 3 · the provider's KV table sees both conversations ──
        print("\n── 3 · KV accounting ──", flush=True)
        kv = hk.send(PROV, {"type": "reflect"}).get("kv") or {}
        need(kv.get("live") == 2 and len(kv.get("conversations") or []) == 2,
             f"the provider's KV table holds both chats' conversations: {short(kv, 300)}")

        # ── 4 · lifecycle: a provider change reaches its chats; the provider outlives them ──
        print("\n── 4 · lifecycle ──", flush=True)
        r = hk.send(hk.root_id, {"type": "update_agent", "id": PROV, "endpoint": m2.url})
        need(r.get("updated") is True, f"the provider's endpoint changed: {short(r, 160)}")
        c = turn_of(hk, m2, CA, "Once more.", "Answer C.", 1)
        need(c.get("temperature") == 0.3 and len(m.turns()) == 2,
             f"{CA}'s next request reached the new endpoint with its own knobs: "
             f"temperature={c.get('temperature')} first server={len(m.turns())}")
        for cid in (CA, CB):
            r = hk.send(hk.root_id, {"type": "delete_agent", "id": cid})
            need(r.get("deleted") is True, f"delete {cid}: {short(r, 160)}")
        need(record(hk, PROV).get("id") == PROV, "the provider stays when its last chat goes")
    finally:
        hk.close()
        for mock in (m, m2):
            mock.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
