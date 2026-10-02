"""providers — what each backend does when the endpoint misbehaves.

§9.1's last `source` row: "the stub answers every request 200 SSE end_turn", so
no 4xx, no 5xx, no 429, no cut stream, no missing key was ever driven on any
runtime. Each case here is its OWN boot, because several of them are about a
value read once at start (a key that is absent from the environment) or about a
retry budget that must start from zero.

The stub records the REQUEST as carefully as the reply: which path the backend
chose, which header carried the key, whether `model` was sent at all. Half of
these items are about the request, not the answer.
"""

from __future__ import annotations

import time

from families import BASE, MEMORY, OPEN, VLLM, ai, call, save_kernel_log, vllm
from helpers.creds import credentials
from spec import CTX_RESERVE, CTX_WINDOW
from stubs.ollama_chat import Handler as OllamaHandler, OllamaStub
from stubs.openai_chat import Handler as OpenAIHandler, OpenAIChatStub

NAME = "providers"

DONE = '<call name="done"></call>'

#: `(case, script, env_drop, extra)` per provider. `env_drop` names an
#: environment variable the kernel is started WITHOUT — the no-key case.
#: `extra["key"]` makes the record name `credentials` (the `vllm` ref in the
#: seeded `creds` store, which reads `VLLM_API_KEY`): the one place a key is
#: meant to be read, so the request's `Authorization` header is measured.
VLLM_CASES = [
    ("ok_baseline", [DONE], (), {}),
    ("keyed_baseline", [DONE], (), {"key": True}),
    ("keyed_no_key", [DONE], ("VLLM_API_KEY",), {"key": True}),
    ("http_400_json", [{"status": 400, "body": {
        "object": "error", "message": "stub says no", "type": "BadRequestError",
        "param": None, "code": 400}}], (), {}),
    ("http_401_wants_key", [{"status": 401, "body": {"error": "unauthorized"}}], (), {}),
    ("http_502_html_twice_then_200", [
        {"status": 502, "content_type": "text/html", "body": "<html>bad gateway</html>"},
        {"status": 502, "content_type": "text/html", "body": "<html>bad gateway</html>"},
        DONE], (), {}),
    # A stream that ends with NO finish_reason: vLLM died mid-answer.
    ("stream_cut_no_finish_reason", [{"text": DONE, "cut": True}], (), {}),
    # …and the same cut INSIDE the reasoning channel: the think span must still close.
    ("stream_cut_mid_reasoning", [{"reasoning": "weighing it", "text": "", "cut": True}],
     (), {}),
    # A main turn cut at the record's max_tokens fails, naming the cut.
    ("finish_length", [{"text": "an answer the budget cut", "finish_reason": "length"}],
     (), {}),
    # The reasoning channel (`reasoning_content`) before the answer.
    ("reasoning_then_answer", [{"reasoning": "weighing it", "text": DONE}], (), {}),
    # B2.11 — one line, a stall, then the rest: a token event must reach the
    # stream before the whole reply is released.
    ("token_before_release", [{"text": "first line\n" + DONE, "stall_mid": 2}], (), {}),
]

OLLAMA_CASES = [
    ("ollama_ok_baseline", [DONE], (), {}),
    ("ollama_no_model", [DONE], (), {"no_model": True}),
    ("ollama_500", [{"status": 500, "body": {"error": "stub blew up"}}], (), {}),
]


def drive(ctx, provider: str, case: str, stub, handler_meta: dict, env_drop,
          keyed: dict[str, str] | None = None) -> None:
    """One boot, one send, everything recorded."""
    import shutil

    rec, ports = ctx.rec, ctx.ports
    rel = f"{provider}/{case}"
    # A FRESH TREE PER CASE. Each case is its own boot, and a `.fantastic` left
    # by the previous one would hydrate an `ai` whose record belongs to another
    # provider — the second case would then measure the first one's agent.
    tree = ctx.work / ".fantastic"
    if tree.exists():
        shutil.rmtree(tree)
    # `keyed` = `{ref: endpoint}` the seeded `creds` store holds for this boot.
    k = ctx.boot(reach=["readings", "ai", "web", "rest", "ws"], port=ports.take("main"),
                 env_drop=tuple(env_drop), creds=keyed)
    cb = {**BASE, "rest": "*", "web": "*", "ws": "*"}
    try:
        rec.json(f"{rel}/create_readings", k.create(
            parent_id=k.root_id, id="readings", handler_module="file_bridge.tools", allow=["."], root=".",
            **OPEN, callable_by=cb))
        rec.json(f"{rel}/create_ai", k.create(
            **{"parent_id": k.root_id, "id": "ai", **OPEN, "callable_by": cb,
               **ai(can_call={**BASE, "readings": "*"}),
               "context_window": CTX_WINDOW, "output_reserve": CTX_RESERVE,
               **handler_meta}))
        time.sleep(0.8)
        rec.json(f"{rel}/reflect", call(k, "ai", {"type": "reflect"}))
        t0 = time.monotonic()
        reply = call(k, "ai", {"type": "send", "text": "say done", "client_id": "probe"},
                     timeout=90)
        rec.json(f"{rel}/send_reply", {"reply": reply},
                 harness={"waited_s": round(time.monotonic() - t0, 2)})
        time.sleep(2.0)
        rec.json(f"{rel}/rows_read", call(k, MEMORY, {"type": "read", "owner": "ai"}))
        rec.json(f"{rel}/status", call(k, "ai", {"type": "status", "client_id": "probe"}))
        rec.json(f"{rel}/requests", stub.requests)
    finally:
        rec.http(f"{rel}/http", k.http)
        k.close()
        save_kernel_log(rec, k, f"{rel}/kernel_log.txt")


def run(ctx) -> None:
    rec, ports = ctx.rec, ctx.ports
    (ctx.work / "a.txt").write_text("7\n")

    # ── vllm: the OpenAI chat wire ──
    for case, script, env_drop, extra in VLLM_CASES:
        stub = OpenAIChatStub(rec, f"vllm/{case}/stub", ports.take("stub"))
        stub.set_chain(case, script)
        stub.start(OpenAIHandler)
        meta = {"handler_module": VLLM, **vllm(stub.base_url())}
        keyed = None
        if extra.get("key"):
            meta["credentials"] = credentials("vllm")
            keyed = {"vllm": stub.base_url()}
        try:
            drive(ctx, "vllm", case, stub, meta, env_drop, keyed)
        finally:
            stub.flush()
            stub.stop()

    # ── ollama: the PATH is the measurement ──
    for case, script, env_drop, extra in OLLAMA_CASES:
        stub = OllamaStub(rec, f"ollama/{case}/stub", ports.take("stub"))
        stub.set_script(script)
        stub.start(OllamaHandler)
        meta = {"handler_module": "ollama_backend.tools", "endpoint": stub.base_url()}
        if not extra.get("no_model"):
            meta["model"] = "stub-model"
        try:
            drive(ctx, "ollama", case, stub, meta, env_drop)
        finally:
            stub.flush()
            stub.stop()
