"""kernel/chat_provider_split — an `ai_chat.tools` chat and its provider APART. NO MODEL.

Scripted OpenAI-compatible servers (`e2e/mocks/vllm.py`) play the model behind
`vllm_backend` PROVIDERS; the chat holds the conversation and completes through the provider
its `provider_id` names (`complete`, `watch`). Rust lane (`runtimes: [rust]`), driven over
`web_rest` on the packaged host.

  1. local split: the chat answers `send` (the scripted server sees one request); its record
     names the provider and holds no `endpoint`; its `history` holds the transcript; the
     provider's holds none;
  2. swap: `update_agent` the chat's `provider_id` to a second provider (another server): the
     next pass reaches the second server, and that request carries the earlier rows;
  3. lines: a chat with no line on its provider is refused by the provider's name (`cannot
     watch provider …`) and the server sees no request from it; a chat HOLDING the line
     completes and the provider's record is byte-equal before and after (the target can
     neither widen nor block); a chat holding the line under an ancestor that does not (the
     cap) is refused by the provider's name and the server sees nothing;
  4. across the bridge (MUTED while `MUTED_4` is set, #763): the provider on a SECOND kernel that
     dials the first through a `ws_bridge` (as E118): the chat names it `<guid>|<id>`, the second
     kernel's bridge is the leg that serves the call there, and the pass completes through it.

Every line is the caller's own `can`; nothing is written on a target.
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
from helpers.lines import ALL, PROVIDER
from helpers.verdict import verdict
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
AI, PROV, PROV2 = "chat", "prov_a", "prov_b"
NO_GRANT, HOLDS_LINE, CAPPED, MID = "chat_no_grant", "chat_holds_line", "chat_capped", "mid"
REMOTE_AI, REMOTE_PROV, BRIDGE = "chat_remote", "prov_far", "br"
MODEL_A, MODEL_B, MODEL_FAR = "mock-a", "mock-b", "mock-far"
COMPLETE = PROVIDER
DONE = '\n<call name="done"></call>'

#: Part 4 is muted: a line naming a peer (`<guid>|<id>`) is refused by the `can` parser until the
#: peer design lands (#763). Parts 1-3 run and assert. Un-mute by setting this to "".
MUTED_4 = "#763: cross-kernel peers wait for the peer design"

bad: list[str] = []


def need(cond: bool, what: str) -> bool:
    print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
    if not cond:
        bad.append(what)
    return cond


def short(v, n: int = 240) -> str:
    return str(v)[:n]


def record(hk: HostKernel, aid: str) -> dict:
    """`aid`'s record as `rest` sees it (reflect; whole inside rest's tree)."""
    r = hk.send(aid, {"type": "reflect"})
    return r if isinstance(r, dict) else {}


def provider(hk: HostKernel, pid: str, mock: VllmMock, model: str) -> None:
    rec = hk.create(parent_id="rest", handler_module="vllm_backend.tools", id=pid,
                    endpoint=mock.url, model=model, context_window=262144,
                    kv_slots=4, kv_overflow="evict", memory_store_id="memory",
                    can={"memory": ALL})
    need(rec.get("id") == pid, f"create provider {pid}: {short(rec, 160)}")


def chat(hk: HostKernel, cid: str, provider_id: str, model: str, can: dict,
         parent: str = "rest") -> None:
    rec = hk.create(parent_id=parent, handler_module="ai_chat.tools", id=cid,
                    provider_id=provider_id, model=model, max_tokens=8192,
                    context_window=262144, memory_store_id="memory",
                    can={"memory": ALL, **can})
    need(rec.get("id") == cid, f"create chat {cid}: {short(rec, 160)}")


def history_text(hk: HostKernel, aid: str) -> str:
    return json.dumps(hk.send(aid, {"type": "history", "limit": 500}), default=str)


def idle(hk: HostKernel, aid: str, bound: float = 60) -> bool:
    end = time.monotonic() + bound
    while time.monotonic() < end:
        if hk.quiet(aid):
            return True
        time.sleep(0.05)
    return False


def passes_at_least(m: VllmMock, n: int, bound: float = 60) -> list[dict]:
    end = time.monotonic() + bound
    while time.monotonic() < end:
        if len(m.passes()) >= n:
            break
        time.sleep(0.05)
    return m.passes()


def messages(one_pass: dict) -> str:
    return json.dumps(one_pass["body"].get("messages") or [])


def refused_by_name(hk: HostKernel, cid: str, pid: str, bound: float = 30) -> str:
    """Send a message to chat `cid`; the text, once it names the refusal of provider `pid`."""
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
    far = HostKernel(spec, pwork).boot()
    try:
        # the far kernel dials this one: this kernel then reaches its agents as `<guid>|<id>`
        rec = far.create(parent_id="rest", handler_module="ws_bridge.tools", id=BRIDGE,
                         host="127.0.0.1", port=hk.port, peer_id="ws")
        need(rec.get("id") == BRIDGE, f"create {BRIDGE} → :{hk.port}: {short(rec, 160)}")
        up = far.send(BRIDGE, {"type": "reflect"})
        need(up.get("connected") is True, f"the far bridge is up: {short(up, 160)}")
        guid = far.send(far.root_id, {"type": "reflect", "tree": "none"}).get("guid")
        need(bool(guid), f"the far kernel's guid: {guid}")
        # the bridge that serves this kernel's calls there holds the line on the far provider
        provider(far, REMOTE_PROV, mfar, MODEL_FAR)
        r = far.send(far.root_id, {"type": "update_agent", "id": BRIDGE,
                                   "can": {REMOTE_PROV: COMPLETE}})
        need(r.get("updated") is True, f"the far bridge may call its provider: {short(r, 160)}")
        far_id = f"{guid}|{REMOTE_PROV}"
        chat(hk, REMOTE_AI, far_id, MODEL_FAR, {far_id: COMPLETE})
        mfar.reply("Answer from the far kernel." + DONE)
        out = hk.send(REMOTE_AI, {"type": "send", "text": "Ask the far provider."})
        need(not out.get("error"), f"the chat accepts a send: {short(out, 160)}")
        passes_f = passes_at_least(mfar, 1)
        need(idle(hk, REMOTE_AI), "the remote-provider pass settles")
        need(len(passes_f) == 1, f"the far kernel's server saw the request: {len(passes_f)}")
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
    hk = HostKernel(spec, work).boot()
    try:
        # ── 1 · the local split ──
        print("\n── 1 · local split ──", flush=True)
        provider(hk, PROV, ma, MODEL_A)
        chat(hk, AI, PROV, MODEL_A, {PROV: COMPLETE})
        ma.reply("First answer." + DONE)
        out = hk.send(AI, {"type": "send", "text": "Say the first thing."})
        need(not out.get("error"), f"the chat accepts a send: {short(out, 160)}")
        passes = passes_at_least(ma, 1)
        need(idle(hk, AI), "the chat's pass settles")
        need(len(passes) == 1, f"the provider's server saw one request: {len(passes)}")
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
        provider(hk, PROV2, mb, MODEL_B)
        cc = dict(record(hk, AI).get("can") or {})
        r = hk.send(hk.root_id, {"type": "update_agent", "id": AI, "provider_id": PROV2,
                                 "model": MODEL_B, "can": {**cc, PROV2: COMPLETE}})
        need(r.get("updated") is True, f"update_agent: the chat's provider_id → {PROV2}: {short(r, 160)}")
        mb.reply("Second answer." + DONE)
        out = hk.send(AI, {"type": "send", "text": "Now the second thing."})
        need(not out.get("error"), f"the chat accepts a send: {short(out, 160)}")
        passes_b = passes_at_least(mb, 1)
        need(idle(hk, AI), "the swapped chat's pass settles")
        need(len(passes_b) == 1 and len(ma.passes()) == 1,
             f"the next pass went to the second server only: b={len(passes_b)} a={len(ma.passes())}")
        sent = messages(passes_b[0]) if passes_b else ""
        need(MODEL_B == (passes_b[0]["body"].get("model") if passes_b else None),
             "the request names the chat's model")
        need("First answer." in sent and "Say the first thing." in sent,
             "the request to the new provider carries the rows of the earlier pass")
        need("Second answer." in history_text(hk, AI), "the chat's history holds the second answer")

        # ── 3 · the lines: the caller's own, and the cap ──
        print("\n── 3 · lines ──", flush=True)
        # a chat with no line on its provider
        chat(hk, NO_GRANT, PROV, MODEL_A, {})
        seen = refused_by_name(hk, NO_GRANT, PROV)
        need(f"cannot watch provider `{PROV}`" in seen,
             f"a chat with no line on its provider is refused by the provider's name: {seen[:200]}")
        need(len(ma.passes()) == 1, f"it never reached the server: {len(ma.passes())} request(s) in all")
        # (a) a chat holding the line completes; the provider's record is untouched
        prov_before = json.dumps(hk.stored(PROV), sort_keys=True)
        chat(hk, HOLDS_LINE, PROV, MODEL_A, {PROV: COMPLETE})
        ma.reply("Third answer." + DONE)
        out = hk.send(HOLDS_LINE, {"type": "send", "text": "The third thing."})
        need(not out.get("error"), f"the chat holding the line accepts a send: {short(out, 160)}")
        passes_at_least(ma, 2)
        need(idle(hk, HOLDS_LINE), "its pass settles")
        need(len(ma.passes()) == 2 and "Third answer." in history_text(hk, HOLDS_LINE),
             f"a chat holding the line completes: {len(ma.passes())} request(s) in all")
        need(json.dumps(hk.stored(PROV), sort_keys=True) == prov_before and prov_before != "{}",
             "the provider's record is byte-equal before and after (it can neither widen nor block)")
        # (b) the cap: the line held under an ancestor whose tree does not hold the provider.
        # `mid` holds the `memory` line only, so the cap cuts the provider alone.
        rec = hk.create(parent_id="rest", handler_module="user_probe.tools", id=MID,
                        can={"memory": ALL})
        need(rec.get("id") == MID, f"create {MID} (no line on {PROV}): {short(rec, 160)}")
        chat(hk, CAPPED, PROV, MODEL_A, {PROV: COMPLETE}, parent=MID)
        seen = refused_by_name(hk, CAPPED, PROV)
        need(f"cannot watch provider `{PROV}`" in seen,
             f"the cap: {MID} holds no line on {PROV}, so its child's call is refused by the "
             f"provider's name: {seen[:200]}")
        need(len(ma.passes()) == 2, f"the capped chat never reached the server: {len(ma.passes())} in all")

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
