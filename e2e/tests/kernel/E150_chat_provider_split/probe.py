"""kernel/chat_provider_split — an `ai_chat.tools` chat and its provider APART. NO MODEL.

Scripted OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model behind
`vllm_backend` PROVIDERS; the chat holds the conversation and completes through the provider
its `provider_id` names (`complete`, `watch`). Rust lane (`runtimes: [rust]`), driven over
`web_rest` on the packaged host.

  1. local split: the chat answers `send` (the scripted server sees one request); its record
     names the provider and holds no `endpoint`; its `history` holds the transcript; the
     provider's holds none;
  2. swap: `update_agent` the chat's `provider_id` to a second provider (another server): the
     next turn reaches the second server, and that request carries the earlier rows;
  3. grants: a chat without `complete` / `watch` on its provider, and a provider that does not
     admit the chat, are each refused by the provider's name (`cannot watch provider …`), and
     the server sees no request from them;
  4. across the bridge (MUTED while `MUTED_4` is set, #616): the provider on a SECOND kernel that dials the first through a
     `ws_bridge` (as E118): the chat names it `<guid>|<id>`, the second kernel's bridge is the
     leg that serves the call there (the provider admits it), and the turn completes through it.
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
AI, PROV, PROV2 = "chat", "prov_a", "prov_b"
NO_GRANT, NOT_ADMITTED = "chat_no_grant", "chat_not_admitted"
REMOTE_AI, REMOTE_PROV, BRIDGE = "chat_remote", "prov_far", "br"
MODEL_A, MODEL_B, MODEL_FAR = "mock-a", "mock-b", "mock-far"
COMPLETE = ["complete", "watch", "reflect"]
DONE = '\n<call name="done"></call>'

#: Part 4 is muted (owner 2026-11-11): the chat's call across the bridge needs the autogrant redesign,
#: fantastic#616 (the bridge's ingress and grants are not changed meanwhile). Parts 1-3 run and assert.
#: Un-mute by setting this to "".
MUTED_4 = "#616: the call across the bridge waits for the autogrant redesign (edge sealed by policy)"

bad: list[str] = []


def need(cond: bool, what: str) -> bool:
    print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
    if not cond:
        bad.append(what)
    return cond


def short(v, n: int = 240) -> str:
    return str(v)[:n]


def callable_by(hk: HostKernel, aid: str) -> dict:
    r = hk.send("kernel", {"type": "list_agents"})
    rec = next((a for a in r.get("agents") or [] if a.get("id") == aid), {})
    return dict(rec.get("callable_by") or {})


def record(hk: HostKernel, aid: str) -> dict:
    r = hk.send("kernel", {"type": "list_agents"})
    return next((a for a in r.get("agents") or [] if a.get("id") == aid), {})


def admit(hk: HostKernel, target: str, who: str, verbs) -> None:
    r = hk.send(hk.root_id, {"type": "update_agent", "id": target,
                             "callable_by": {**callable_by(hk, target), who: verbs}})
    need(r.get("updated") is True, f"{target} admits {who}: {short(r, 160)}")


def provider(hk: HostKernel, pid: str, mock: VllmMock, model: str, admits: dict) -> None:
    rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=pid,
                    endpoint=mock.url, model=model, max_tokens=8192, context_window=262144,
                    kv_slots=4, kv_overflow="evict", memory_store_id="memory",
                    can_call={"$self": "*", "memory": "*"},
                    callable_by={"$self": "*", "$parent": "*", **admits})
    need(rec.get("id") == pid, f"create provider {pid}: {short(rec, 160)}")


def chat(hk: HostKernel, cid: str, provider_id: str, model: str, can_call: dict) -> None:
    rec = hk.create(parent_id="rest", handler_module="ai_chat.tools", id=cid,
                    provider_id=provider_id, model=model, max_tokens=8192,
                    context_window=262144, memory_store_id="memory",
                    can_call={"$self": "*", "memory": "*", **can_call},
                    callable_by={"$self": "*", "$parent": "*", "rest": "*"})
    need(rec.get("id") == cid, f"create chat {cid}: {short(rec, 160)}")


def history_text(hk: HostKernel, aid: str) -> str:
    return json.dumps(hk.send(aid, {"type": "history", "limit": 500}), default=str)


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


def messages(turn: dict) -> str:
    return json.dumps(turn["body"].get("messages") or [])


def refused_by_name(hk: HostKernel, cid: str, pid: str, bound: float = 30) -> str:
    """Send a turn to chat `cid`; the text, once it names the refusal of provider `pid`."""
    out = hk.send(cid, {"type": "send", "text": "hello there"})
    want = f"cannot watch provider `{pid}`"
    end = time.monotonic() + bound
    seen = json.dumps(out, default=str)
    while want not in seen and time.monotonic() < end:
        time.sleep(0.2)
        seen = (json.dumps(out, default=str) + history_text(hk, cid)
                + json.dumps(hk.send(cid, {"type": "status"}), default=str))
    return seen


def across_the_bridge(spec: dict, hk: HostKernel) -> None:
    """Part 4: the provider on a SECOND kernel, reached through a `ws_bridge`. It boots that kernel
    and its scripted server itself and stops both."""
    mfar = VllmMock(model=MODEL_FAR).start()
    pwork = Path(tempfile.mkdtemp(prefix="e2e-split-far-"))
    far = HostKernel(spec, pwork, reach=[REMOTE_PROV, BRIDGE]).boot()
    try:
        admit(far, "memory", REMOTE_PROV, "*")
        # the far kernel dials this one: this kernel then reaches its agents as `<guid>|<id>`
        rec = far.create(parent_id="rest", handler_module="ws_bridge.tools", id=BRIDGE,
                         host="127.0.0.1", port=hk.port, peer_id="ws",
                         callable_by={"$self": "*", "$parent": "*", "ws": "*"})
        need(rec.get("id") == BRIDGE, f"create {BRIDGE} → :{hk.port}: {short(rec, 160)}")
        up = far.send(BRIDGE, {"type": "reflect"})
        need(up.get("connected") is True, f"the far bridge is up: {short(up, 160)}")
        guid = far.send(far.root_id, {"type": "reflect", "tree": "none"}).get("guid")
        need(bool(guid), f"the far kernel's guid: {guid}")
        # the far provider admits the bridge that serves this kernel's calls there
        provider(far, REMOTE_PROV, mfar, MODEL_FAR, {BRIDGE: COMPLETE})
        bcc = dict(record(far, BRIDGE).get("can_call") or {})
        r = far.send(far.root_id, {"type": "update_agent", "id": BRIDGE,
                                   "can_call": {**bcc, REMOTE_PROV: COMPLETE}})
        need(r.get("updated") is True, f"the far bridge may call its provider: {short(r, 160)}")
        far_id = f"{guid}|{REMOTE_PROV}"
        chat(hk, REMOTE_AI, far_id, MODEL_FAR, {far_id: COMPLETE})
        mfar.reply("Answer from the far kernel." + DONE)
        out = hk.send(REMOTE_AI, {"type": "send", "text": "Ask the far provider."})
        need(not out.get("error"), f"the chat accepts a send: {short(out, 160)}")
        turns_f = turns_at_least(mfar, 1)
        need(idle(hk, REMOTE_AI), "the remote-provider turn settles")
        need(len(turns_f) == 1, f"the far kernel's server saw the request: {len(turns_f)}")
        need("Answer from the far kernel." in history_text(hk, REMOTE_AI),
             "the transcript is on the chat's kernel; the provider's is on the far one")
        need("Answer from the far kernel." not in history_text(far, REMOTE_PROV),
             "the far provider holds none of it")
    finally:
        far.close()
        mfar.stop()
        shutil.rmtree(pwork, ignore_errors=True)


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    ma, mb = (VllmMock(model=m).start() for m in (MODEL_A, MODEL_B))
    work = Path(tempfile.mkdtemp(prefix="e2e-split-"))
    reach = [AI, PROV, PROV2, NO_GRANT, NOT_ADMITTED, REMOTE_AI]
    hk = HostKernel(spec, work, reach=reach).boot()
    try:
        for who in (AI, PROV, PROV2, NO_GRANT, NOT_ADMITTED, REMOTE_AI):
            admit(hk, "memory", who, "*")

        # ── 1 · the local split ──
        print("\n── 1 · local split ──", flush=True)
        provider(hk, PROV, ma, MODEL_A, {AI: COMPLETE, NO_GRANT: COMPLETE})
        chat(hk, AI, PROV, MODEL_A, {PROV: COMPLETE})
        ma.reply("First answer." + DONE)
        out = hk.send(AI, {"type": "send", "text": "Say the first thing."})
        need(not out.get("error"), f"the chat accepts a send: {short(out, 160)}")
        turns = turns_at_least(ma, 1)
        need(idle(hk, AI), "the chat's turn settles")
        need(len(turns) == 1, f"the provider's server saw one request: {len(turns)}")
        rec = record(hk, AI)
        need(rec.get("provider_id") == PROV and rec.get("endpoint") is None,
             f"the chat's record names its provider and holds no endpoint: provider_id="
             f"{rec.get('provider_id')!r} endpoint={rec.get('endpoint')!r}")
        need(record(hk, PROV).get("endpoint") == ma.url and record(hk, PROV).get("provider_id") is None,
             "the provider's record holds the endpoint and names no provider")
        held = history_text(hk, AI)
        need("First answer." in held and "Say the first thing." in held,
             f"the chat's history holds the transcript: {held[:200]}")
        need("First answer." not in history_text(hk, PROV),
             "the provider holds none of the transcript")

        # ── 2 · swap the provider ──
        print("\n── 2 · swap ──", flush=True)
        provider(hk, PROV2, mb, MODEL_B, {AI: COMPLETE})
        cc = dict(record(hk, AI).get("can_call") or {})
        r = hk.send(hk.root_id, {"type": "update_agent", "id": AI, "provider_id": PROV2,
                                 "model": MODEL_B, "can_call": {**cc, PROV2: COMPLETE}})
        need(r.get("updated") is True, f"update_agent: the chat's provider_id → {PROV2}: {short(r, 160)}")
        mb.reply("Second answer." + DONE)
        out = hk.send(AI, {"type": "send", "text": "Now the second thing."})
        need(not out.get("error"), f"the chat accepts a send: {short(out, 160)}")
        turns_b = turns_at_least(mb, 1)
        need(idle(hk, AI), "the swapped chat's turn settles")
        need(len(turns_b) == 1 and len(ma.turns()) == 1,
             f"the next turn went to the second server only: b={len(turns_b)} a={len(ma.turns())}")
        sent = messages(turns_b[0]) if turns_b else ""
        need(MODEL_B == (turns_b[0]["body"].get("model") if turns_b else None),
             "the request names the chat's model")
        need("First answer." in sent and "Say the first thing." in sent,
             "the request to the new provider carries the rows of the earlier turn")
        need("Second answer." in history_text(hk, AI), "the chat's history holds the second answer")

        # ── 3 · the grants, both ends ──
        print("\n── 3 · grants ──", flush=True)
        # a chat the provider admits, with no `complete` / `watch` of its own on it
        chat(hk, NO_GRANT, PROV, MODEL_A, {})
        seen = refused_by_name(hk, NO_GRANT, PROV)
        need(f"cannot watch provider `{PROV}`" in seen,
             f"a chat without the grant is refused by the provider's name: {seen[:200]}")
        # a chat with the grant, on a provider that does not admit it
        chat(hk, NOT_ADMITTED, PROV, MODEL_A, {PROV: COMPLETE})
        seen = refused_by_name(hk, NOT_ADMITTED, PROV)
        need(f"cannot watch provider `{PROV}`" in seen,
             f"a provider that does not admit the chat is refused by name: {seen[:200]}")
        need(len(ma.turns()) == 1, f"neither reached the server: {len(ma.turns())} request(s) in all")

        # ── 4 · the provider on another kernel, across the bridge ──
        print("\n── 4 · across the bridge ──", flush=True)
        if MUTED_4:
            print(f"part 4 muted {MUTED_4}", flush=True)
        else:
            across_the_bridge(spec, hk)
    finally:
        hk.close()
        for m in (ma, mb):
            m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
