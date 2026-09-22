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

from families import BASE, OPEN, call, save_kernel_log
from spec import CTX_RESERVE, CTX_WINDOW
from stubs.anthropic_sse import AnthropicStub, Handler as AnthropicHandler
from stubs.ollama_chat import Handler as OllamaHandler, OllamaStub
from stubs.openai_chat import Handler as OpenAIHandler, OpenAIChatStub

NAME = "providers"

DONE = '<call name="done"></call>'

#: `(case, script, env_drop, extra meta)` per provider. `env_drop` names an
#: environment variable the kernel is started WITHOUT — the no-key cases.
ANTHROPIC_CASES = [
    ("ok_baseline", [DONE], (), {}),
    ("http_400_json", [{"status": 400, "body": {"type": "error", "error": {
        "type": "invalid_request_error", "message": "stub says no"}}}], (), {}),
    ("http_502_html_twice_then_200", [
        {"status": 502, "content_type": "text/html", "body": "<html>bad gateway</html>"},
        {"status": 502, "content_type": "text/html", "body": "<html>bad gateway</html>"},
        DONE], (), {}),
    ("event_error_midstream", [{"text": DONE, "event_error": True}], (), {}),
    ("stream_cut_before_stop", [{"text": DONE, "cut": True}], (), {}),
    ("stop_reason_tool_use_no_block", [{"text": "thinking about a tool",
                                        "stop_reason": "tool_use"}], (), {}),
    ("no_key_at_all", [DONE], ("ANTHROPIC_KEY", "ANTHROPIC_API_KEY"), {}),
]

OPENAI_CASES = [
    ("nim_ok_baseline", [DONE], (), {"handler": "nvidia_nim_backend.tools"}),
    ("nim_429_retry_after_then_200", [
        {"status": 429, "retry_after": 1, "body": {"error": "rate limited"}}, DONE],
     (), {"handler": "nvidia_nim_backend.tools"}),
    ("nim_429_twice", [
        {"status": 429, "retry_after": 1, "body": {"error": "rate limited"}},
        {"status": 429, "retry_after": 1, "body": {"error": "rate limited"}},
        DONE],
     (), {"handler": "nvidia_nim_backend.tools"}),
    ("nim_no_key", [DONE], ("NVIDIA_API_KEY",), {"handler": "nvidia_nim_backend.tools"}),
    ("llamacpp_ok_baseline", [DONE], (), {"handler": "llamacpp_backend.tools"}),
    # B2.11 — one line, a stall, then the rest: a token event must reach the
    # stream before the whole reply is released.
    ("llamacpp_token_before_release", [{"text": "first line\n" + DONE, "stall": 2}],
     (), {"handler": "llamacpp_backend.tools"}),
    ("llamacpp_no_temperature_no_model", [DONE], (),
     {"handler": "llamacpp_backend.tools", "no_model": True}),
]

OLLAMA_CASES = [
    ("ollama_ok_baseline", [DONE], (), {}),
    ("ollama_no_model", [DONE], (), {"no_model": True}),
    ("ollama_500", [{"status": 500, "body": {"error": "stub blew up"}}], (), {}),
]


def drive(ctx, provider: str, case: str, stub, handler_meta: dict, env_drop) -> None:
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
    k = ctx.boot(reach=["readings", "ai", "web", "rest", "ws"], port=ports.take("main"),
                 env_drop=tuple(env_drop))
    cb = {**BASE, "rest": "*", "web": "*", "ws": "*"}
    try:
        rec.json(f"{rel}/create_readings", k.create(
            parent_id=k.root_id, id="readings", handler_module="file_bridge.tools", root=".",
            **OPEN, callable_by=cb))
        rec.json(f"{rel}/create_ai", k.create(
            **{"parent_id": k.root_id, "id": "ai", **OPEN, "callable_by": cb,
               "can_call": {**BASE, "readings": "*"},
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
        rec.json(f"{rel}/rows_read", call(k, "kernel", {"type": "rows_read", "agent": "ai"}))
        rec.json(f"{rel}/status", call(k, "ai", {"type": "status", "client_id": "probe"}))
        rec.json(f"{rel}/requests", stub.requests)
    finally:
        rec.http(f"{rel}/http", k.http)
        k.close()
        save_kernel_log(rec, k, f"{rel}/kernel_log.txt")


def run(ctx) -> None:
    rec, ports = ctx.rec, ctx.ports
    (ctx.work / "a.txt").write_text("7\n")

    # ── anthropic ──
    for case, script, env_drop, extra in ANTHROPIC_CASES:
        stub = AnthropicStub(rec, f"anthropic/{case}/stub", ports.take("stub"))
        stub.set_chain(case, script)
        stub.start(AnthropicHandler)
        try:
            drive(ctx, "anthropic", case, stub,
                  {"handler_module": "anthropic_backend.tools", "model": "stub-model",
                   "endpoint": stub.url()}, env_drop)
        finally:
            stub.flush()
            stub.stop()

    # ── openai-compatible: nvidia_nim and llamacpp on ONE surface ──
    for case, script, env_drop, extra in OPENAI_CASES:
        stub = OpenAIChatStub(rec, f"openai/{case}/stub", ports.take("stub"))
        stub.set_script(script)
        stub.start(OpenAIHandler)
        meta = {"handler_module": extra["handler"], "endpoint": stub.base_url()}
        if not extra.get("no_model"):
            meta["model"] = "stub-model"
        try:
            drive(ctx, "openai", case, stub, meta, env_drop)
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
