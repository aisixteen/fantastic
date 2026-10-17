"""backends/provider_mocks — the reusable provider mocks (`e2e/mocks`), proven
against the kernel's own backends. NO MODEL.

One mock per provider, and on the PACKAGED host of `RUNTIME` (python | rust |
swift), every call through `web_rest`, one AI agent per provider whose record
names the mock's `url` and an env-backed credential ref pinned to its host. The
record leaves to the server what the backend reads from it (vllm: model,
max_tokens, context_window, kv_slots; ollama: model; anthropic: max_tokens,
context_window), so each probe path is exercised. Per provider:

  1. a turn answers the mock's text, and the server-read fields land on the record;
  2. the key arrives on EVERY request, in the provider's header;
  3. an HTTP 500 → the turn fails loud, naming the status; the next turn works;
  4. a refused key (the mock's 401) → the turn fails loud;
  5. the key never lands in the ledger;
  6. the turn's `status` `done` carries the usage the mock reported —
     `prompt_tokens`, `completion_tokens` (not estimated), `gen_seconds`,
     `context_window` — watched over the ws face.
"""

from __future__ import annotations

import json
import os
import time
import asyncio
import secrets
import shutil
import sys
import tempfile
import threading
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.host.ws import WSWatcher
from helpers.verdict import verdict
from mocks.anthropic import AnthropicMock
from mocks.ollama import OllamaMock
from mocks.responses import ResponsesMock
from mocks.vllm import VllmMock

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")
KEY = "mock-e2e-" + secrets.token_hex(12)
KEY_VAR = "E2E_MOCK_KEY"

#: provider → (mock class, handler module, record fields beyond endpoint/credentials,
#: fields the backend reads from the server)
PROVIDERS = {
    "vllm": (VllmMock, "vllm_backend.tools", {"kv_overflow": "evict"},
             {"model": "mock-vllm", "max_tokens": 262144, "context_window": 262144,
              "kv_slots": 4}),
    "ollama": (OllamaMock, "ollama_backend.tools", {"context_window": 32768},
               {"model": "mock-ollama"}),
    "responses": (ResponsesMock, "responses_backend.tools",
                  {"model": "mock-responses", "context_window": 32768}, {}),
    "anthropic": (AnthropicMock, "anthropic_backend.tools", {"model": "mock-anthropic"},
                  {"max_tokens": 262144, "context_window": 262144}),
}


def watch(port: int, aid: str) -> tuple[list, threading.Event]:
    """Collect `aid`'s events over the ws face in a thread until `stop` is set."""
    seen: list = []
    stop = threading.Event()
    ready = threading.Event()

    async def run():
        w = WSWatcher(port, aid)
        await w.connect()
        ready.set()

        async def pump():
            async for ev in w.events():
                seen.append(ev)

        # the stream is read by ONE task, never under a timeout: a timed-out wait
        # cancels the generator's `__anext__`, and a cancelled async generator is
        # closed — every later event was lost after the first quiet 0.2 s
        reader = asyncio.ensure_future(pump())
        try:
            while not stop.is_set() and not reader.done():
                await asyncio.sleep(0.05)
        finally:
            reader.cancel()
            await w.close()

    threading.Thread(target=lambda: asyncio.run(run()), daemon=True).start()
    ready.wait(10)
    return seen, stop


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    os.environ[KEY_VAR] = KEY  # the STORE reads it (env backend), never a record
    mocks = {p: PROVIDERS[p][0](model=f"mock-{p}").start() for p in PROVIDERS}
    work = Path(tempfile.mkdtemp(prefix="e2e-provider-mocks-"))
    ids = [f"ai_{p}" for p in PROVIDERS]
    hk = HostKernel(spec, work, reach=ids).boot()
    root = hk.root_id
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    def update(aid: str, **fields) -> dict:
        return hk.send(root, {"type": "update_agent", "id": aid, **fields})

    def callable_by(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        rec = next(a for a in r.get("agents") or [] if a.get("id") == aid)
        return dict(rec.get("callable_by") or {})

    def record(aid: str) -> dict:
        r = hk.send("kernel", {"type": "list_agents"})
        return next((a for a in r.get("agents") or [] if a.get("id") == aid), {})

    try:
        r = update("creds",
                   refs={p: {"host": m.host, "env": {"api_key": KEY_VAR}} for p, m in mocks.items()},
                   callable_by={**callable_by("creds"), **{a: ["get"] for a in ids}})
        need(r.get("updated") is True, f"creds: one ref per mock: {str(r)[:200]}")
        r = update("memory", callable_by={**callable_by("memory"), **{a: "*" for a in ids}})
        need(r.get("updated") is True, f"memory accepts the AIs: {str(r)[:200]}")

        for p, (_cls, module, fields, read) in PROVIDERS.items():
            m, aid = mocks[p], f"ai_{p}"
            print(f"── {p}  {m.url}", flush=True)
            m.require_key(KEY)
            rec = hk.create(parent_id="rest", handler_module=module, id=aid,
                            endpoint=m.url, credentials={"store": "creds", "ref": p},
                            memory_store_id="memory",
                            can_call={"$self": "*", "memory": "*", "creds": ["get"],
                                      root: ["reflect"]},
                            callable_by={"$self": "*", "$parent": "*", "rest": "*", "ws": "*"},
                            **fields)
            need(rec.get("id") == aid, f"{p}: create: {str(rec)[:200]}")

            # 1 · a turn answers the mock's text; the server-read fields land
            word = f"MOCK-{p.upper()}-{secrets.token_hex(3)}"
            m.reply(word)
            m.prompt_tokens, m.completion_tokens = 1234, 56
            events, stop = watch(hk.port, aid)
            # a browser-style client: its events reach the agent's watchers
            out = hk.send(aid, {"type": "send", "text": "say the word", "client_id": "tab1"})
            # the done status follows the reply on its own stream: waited for, not
            # a fixed sleep (a loaded runner delivers it later than half a second)
            end = time.monotonic() + 10
            while time.monotonic() < end and not any(
                e.get("type") == "status" and e.get("phase") == "done" for e in list(events)
            ):
                time.sleep(0.05)
            stop.set()
            need(word in str(out.get("response")) and not out.get("error"),
                 f"{p}: the turn answers the mock's text: {str(out)[:200]}")
            got = record(aid)
            need(all(got.get(k) == v for k, v in read.items()),
                 f"{p}: read from the server: {({k: got.get(k) for k in read})}")
            done = [
                e.get("detail") or {}
                for e in events
                if e.get("type") == "status" and e.get("phase") == "done"
            ]
            d = next((x for x in done if x.get("reason") == "ok"), {})
            need(
                d.get("prompt_tokens") == 1234
                and d.get("completion_tokens") == 56
                and "prompt_tokens_estimated" not in d
                and "completion_tokens_estimated" not in d
                and isinstance(d.get("gen_seconds"), (int, float))
                and d.get("context_window") == read.get("context_window", fields.get("context_window")),
                f"{p}: the done status carries the reported usage: {d}"
                + ("" if d else f" — every done seen: {done} · {len(events)} events"),
            )
            probes = sorted({r['path'] for r in m.requests if r['kind'] == 'probe'})
            print(f"     probes: {probes}  turns: {len(m.turns())}", flush=True)

            # 2 · the key on every request
            need(m.requests and all(k == KEY for k in m.keys()),  # noqa: SIM118 (a mock method)
                 f"{p}: the key on every one of {len(m.requests)} requests")
            need(not [r for r in m.requests if r["kind"] == "other"],
                 f"{p}: no request to a path the mock does not serve: "
                 f"{[r['path'] for r in m.requests if r['kind'] == 'other']}")

            # 3 · an HTTP 500 on every try fails the turn loud (a backend may retry
            #     a 5xx: the tries are counted, not assumed); the next turn works
            before = len(m.turns())
            m.error(500, times=None)
            out = hk.send(aid, {"type": "send", "text": "again"})
            m.error(None)
            need(bool(out.get("error")) and "500" in json.dumps(out),
                 f"{p}: HTTP 500 → the turn fails naming it "
                 f"({len(m.turns()) - before} tries): {str(out)[:240]}")
            m.reply("BACK")
            out = hk.send(aid, {"type": "send", "text": "again"})
            need("BACK" in str(out.get("response")) and not out.get("error"),
                 f"{p}: the next turn works: {str(out)[:160]}")

            # 4 · a refused key fails the turn loud
            m.require_key("not-" + KEY)
            out = hk.send(aid, {"type": "send", "text": "again"})
            need(bool(out.get("error")) and "401" in json.dumps(out),
                 f"{p}: a refused key (401) → the turn fails: {str(out)[:240]}")
            m.require_key(KEY)

        # 5 · the key never in the ledger
        ledger = hk.send("kernel", {"type": "ledger"})
        need(KEY not in json.dumps(ledger, default=str), "the key never lands in the ledger")
    finally:
        hk.close()
        for m in mocks.values():
            m.stop()
        shutil.rmtree(work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
