"""vllm_backend — what a record leaves out is READ FROM THE SERVER: `model`, `max_tokens`, `context_window` and `kv_slots`, read before a
verb that runs a turn and written back to the record. The bodies below are the
shapes a live vLLM serves: `/v1/models` names
`max_model_len`, `/metrics` carries one `vllm:cache_config_info` line whose
`kv_cache_max_concurrency` is vLLM's own count of full-length requests.
"""

from __future__ import annotations

import json

import httpx
import pytest
from vllm_backend import server
from vllm_backend.tools import validate_meta

ENDPOINT = "http://vllm.test/v1"
MODEL = "qwen3.8-27b"


def _models(*entries: tuple[str, int]) -> str:
    return json.dumps(
        {
            "object": "list",
            "data": [
                {"id": i, "object": "model", "max_model_len": n} for i, n in entries
            ],
        }
    )


def _metrics(*concurrency: str) -> str:
    lines = ["# HELP vllm:cache_config_info Information of the LLMEngine CacheConfig"]
    for c in concurrency:
        lines.append(
            'vllm:cache_config_info{block_size="432",engine="0",'
            f'kv_cache_max_concurrency="{c}",kv_cache_size_tokens="74561",'
            'model_name="qwen3.8-27b"} 1.0'
        )
    lines.append('vllm:num_requests_running{engine="0",model_name="qwen3.8-27b"} 0.0')
    return "\n".join(lines) + "\n"


def _server(
    seen: list,
    *,
    models: str | None = None,
    metrics: str | None = None,
    models_status: int = 200,
    metrics_status: int = 200,
):
    """A vLLM that answers `/v1/models` and `/metrics`; every request is recorded
    as (url, authorization)."""
    models = models if models is not None else _models((MODEL, 65536))
    metrics = metrics if metrics is not None else _metrics("1.1377245508982037")

    def handler(req: httpx.Request) -> httpx.Response:
        seen.append((str(req.url), req.headers.get("authorization")))
        if req.url.path.endswith("/models"):
            return httpx.Response(models_status, text=models)
        if req.url.path.endswith("/metrics"):
            return httpx.Response(metrics_status, text=metrics)
        return httpx.Response(404, text="not found")

    return httpx.MockTransport(handler)


async def _nokey(endpoint: str) -> None:
    return None


async def _key(endpoint: str) -> str:
    return "sekret"


class _Kernel:
    """The two record calls `resolve` makes."""

    def __init__(self, rec: dict):
        self.rec = {"id": "v1", **rec}
        self.updates: list[dict] = []

    def get(self, id):
        return self.rec if id == "v1" else None

    def update(self, id, **meta):
        self.updates.append(meta)
        self.rec.update(meta)
        return self.rec


def _rec(**kw) -> dict:
    return {
        "endpoint": ENDPOINT,
        "kv_overflow": "evict",
        "memory_store_id": "memory",
        **kw,
    }


async def _resolve(k: _Kernel, transport, key=_nokey):
    return await server.resolve(
        "v1", k, key=key, validate=validate_meta, transport=transport
    )


# ─── the read ────────────────────────────────────────────────────────────────


async def test_what_the_record_leaves_out_is_read():
    seen: list = []
    k = _Kernel(_rec())
    assert await _resolve(k, _server(seen)) is None
    assert k.updates == [
        {"model": MODEL, "max_tokens": 65536, "context_window": 65536, "kv_slots": 1}
    ]
    assert [u for u, _ in seen] == [
        "http://vllm.test/v1/models",
        "http://vllm.test/metrics",
    ]
    # written back: the next verb reads nothing
    seen.clear()
    assert await _resolve(k, _server(seen)) is None
    assert seen == [] and len(k.updates) == 1


async def test_a_field_the_record_sets_is_used_as_set_never_read():
    seen: list = []
    k = _Kernel(_rec(model=MODEL, max_tokens=8000, context_window=262144, kv_slots=6))
    assert await _resolve(k, _server(seen)) is None
    assert seen == [] and k.updates == []
    # only kv_slots left out: only the metrics page is read
    k = _Kernel(_rec(model=MODEL, max_tokens=8000, context_window=262144))
    assert await _resolve(k, _server(seen)) is None
    assert [u for u, _ in seen] == ["http://vllm.test/metrics"]
    assert k.updates == [{"kv_slots": 1}]


async def test_the_record_s_model_picks_its_entry_among_several():
    seen: list = []
    body = _models(("other", 4096), (MODEL, 262144))
    k = _Kernel(_rec(model=MODEL, kv_slots=6))
    assert await _resolve(k, _server(seen, models=body)) is None
    assert k.updates == [{"max_tokens": 262144, "context_window": 262144}]


async def test_several_served_models_and_no_model_refuse_naming_them():
    body = _models(("a", 4096), ("b", 8192))
    k = _Kernel(_rec(kv_slots=6))
    r = await _resolve(k, _server([], models=body))
    assert r["reason"] == "bad_arg" and r["arg"] == "model", r
    assert "serves a, b — not one model; set model on the record" in r["error"], r
    assert k.updates == []


async def test_a_model_the_server_does_not_serve_is_refused():
    k = _Kernel(_rec(model="nope", kv_slots=6))
    r = await _resolve(k, _server([]))
    assert r["arg"] == "max_tokens", r
    assert f"does not serve model 'nope' — it serves {MODEL}" in r["error"], r


async def test_a_door_without_metrics_refuses_naming_kv_slots():
    k = _Kernel(_rec(model=MODEL, max_tokens=8000, context_window=262144))
    r = await _resolve(k, _server([], metrics_status=404))
    assert r == {
        "error": "kv_slots: GET http://vllm.test/metrics answered HTTP 404 — the "
        "server (or a door in front of it) does not serve it; set kv_slots on the "
        "record",
        "reason": "bad_arg",
        "arg": "kv_slots",
    }, r
    assert k.updates == []


async def test_an_unreachable_server_refuses_naming_the_field():
    def down(req):
        raise httpx.ConnectError("refused", request=req)

    k = _Kernel(_rec())
    r = await _resolve(k, httpx.MockTransport(down))
    assert r["arg"] == "model", r
    assert r["error"] == (
        "model: GET http://vllm.test/v1/models failed — the server is not reachable "
        "there; set model on the record"
    ), r


@pytest.mark.parametrize(
    "concurrency, slots", [("1.1377245508982037", 1), ("6.9", 6), ("3", 3)]
)
async def test_kv_slots_is_the_floor_of_vllm_s_concurrency(concurrency, slots):
    k = _Kernel(_rec(model=MODEL, max_tokens=8000, context_window=262144))
    assert await _resolve(k, _server([], metrics=_metrics(concurrency))) is None
    assert k.rec["kv_slots"] == slots


@pytest.mark.parametrize(
    "metrics, words",
    [
        (_metrics("0.6"), "holds 0.6 full-length conversations, under one"),
        (_metrics("2.0", "3.0"), "carries 2 vllm:cache_config_info lines, not one"),
        (_metrics(), "carries 0 vllm:cache_config_info lines, not one"),
        (
            'vllm:cache_config_info{block_size="432"} 1.0\n',
            "names no kv_cache_max_concurrency",
        ),
    ],
)
async def test_a_metrics_page_that_names_no_count_refuses(metrics, words):
    k = _Kernel(_rec(model=MODEL, max_tokens=8000, context_window=262144))
    r = await _resolve(k, _server([], metrics=metrics))
    assert r["arg"] == "kv_slots" and words in r["error"], r


async def test_metrics_url_on_the_record_wins_and_the_key_stays_on_the_endpoint_host():
    seen: list = []
    k = _Kernel(_rec(metrics_url="http://metrics.test:9100/metrics"))
    assert await _resolve(k, _server(seen), key=_key) is None
    assert seen == [
        ("http://vllm.test/v1/models", "Bearer sekret"),
        ("http://metrics.test:9100/metrics", None),
    ]
    # the same host gets the key
    seen.clear()
    k = _Kernel(_rec(model=MODEL, max_tokens=8000, context_window=262144))
    assert await _resolve(k, _server(seen), key=_key) is None
    assert seen == [("http://vllm.test/metrics", "Bearer sekret")]


async def test_the_server_s_values_pass_the_write_check():
    # an explicit thinking budget that the server's window cannot hold
    k = _Kernel(_rec(model=MODEL, kv_slots=6, thinking_token_budget=70000))
    r = await _resolve(k, _server([]))
    assert r["reason"] == "bad_arg" and r["arg"] == "max_tokens", r
    assert r["error"].startswith(
        "the server's values do not fit the record: thinking_token_budget=70000 is "
        "not below max_tokens=65536"
    ), r
    assert k.updates == []


def test_metrics_url_is_the_endpoint_s_v1_replaced_by_metrics():
    assert server.metrics_url({"endpoint": "http://h:10000/v1"}) == (
        "http://h:10000/metrics"
    )
    assert server.metrics_url({"endpoint": "https://d.net/llm/openai/v1/"}) == (
        "https://d.net/llm/openai/metrics"
    )
    assert server.metrics_url({"endpoint": "http://h:10000/api"}) is None
    assert (
        server.metrics_url({"endpoint": "http://h/api", "metrics_url": "http://m/x"})
        == "http://m/x"
    )


# ─── the write ───────────────────────────────────────────────────────────────


def test_a_record_with_only_endpoint_and_kv_overflow_is_accepted():
    assert validate_meta(_rec(), {}) is None


@pytest.mark.parametrize(
    "meta, fragment",
    [
        ({"context_window": "big"}, "context_window must be a positive int"),
        ({"context_window": 0}, "context_window must be a positive int"),
        ({"metrics_url": "vllm.test/metrics"}, "metrics_url must be an http(s) URL"),
        ({"metrics_url": 9100}, "metrics_url must be an http(s) URL"),
        (
            {"endpoint": "http://h:10000/api"},
            "kv_slots is left to the server, but the endpoint 'http://h:10000/api' "
            "does not end in /v1",
        ),
    ],
)
def test_a_write_the_server_read_cannot_honour_is_refused(meta, fragment):
    assert fragment in (validate_meta(meta, _rec()) or ""), validate_meta(meta, _rec())


def test_an_endpoint_without_v1_passes_with_metrics_url_or_kv_slots():
    rec = _rec(endpoint="http://h:10000/api")
    assert validate_meta({"metrics_url": "http://h:10000/metrics"}, rec) is None
    assert validate_meta({"kv_slots": 3}, rec) is None


# ─── through the kernel ──────────────────────────────────────────────────────


def _vllm(seen: list, *, metrics_status: int = 200):
    """The server above, plus a chat that ends the chain with `done`."""
    base = _server(seen, metrics_status=metrics_status)

    def handler(req: httpx.Request) -> httpx.Response:
        if req.url.path.endswith("/chat/completions"):
            seen.append((str(req.url), None))
            body = (
                "data: "
                + json.dumps(
                    {
                        "choices": [
                            {
                                "delta": {"content": '<call name="done"></call>'},
                                "finish_reason": "stop",
                            }
                        ]
                    }
                )
                + "\n\ndata: [DONE]\n\n"
            )
            return httpx.Response(
                200, text=body, headers={"content-type": "text/event-stream"}
            )
        return base.handle_request(req)

    return httpx.MockTransport(handler)


@pytest.fixture
def mock_http(monkeypatch):
    """Route every httpx client of this process to the given transport."""
    real = httpx.AsyncClient

    def use(transport):
        monkeypatch.setattr(
            httpx,
            "AsyncClient",
            lambda **kw: real(**{**kw, "transport": transport}),
        )

    return use


async def _create(kernel, **meta):
    payload = {
        "type": "create_agent",
        "parent_id": kernel.id,
        "handler_module": "vllm_backend.tools",
        **meta,
    }
    return (await kernel.send([{"target": kernel.id, "payload": payload}]))[0]["result"]


async def _call(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def test_a_send_reads_the_server_first_and_reflect_shows_the_values(
    seeded_kernel, mock_http
):
    k = seeded_kernel
    seen: list = []
    mock_http(_vllm(seen))
    r = await _create(k, id="v1", **_rec())
    assert r.get("id") == "v1", r
    before = await _call(k, "v1", {"type": "reflect"})
    assert before["max_tokens"] is None and before["kv"]["slots"] is None, before
    await _call(k, "v1", {"type": "send", "text": "hi", "client_id": "t"})
    rec = k.get("v1")
    assert (
        rec["model"],
        rec["max_tokens"],
        rec["context_window"],
        rec["kv_slots"],
    ) == (
        MODEL,
        65536,
        65536,
        1,
    ), rec
    after = await _call(k, "v1", {"type": "reflect"})
    assert after["max_tokens"] == 65536 and after["kv"]["slots"] == 1, after
    assert [u for u, _ in seen][:2] == [
        "http://vllm.test/v1/models",
        "http://vllm.test/metrics",
    ], seen


async def test_a_send_the_server_cannot_answer_is_refused_naming_the_field(
    seeded_kernel, mock_http
):
    k = seeded_kernel
    seen: list = []
    mock_http(_vllm(seen, metrics_status=404))
    await _create(k, id="v1", **_rec(model=MODEL, max_tokens=8000, context_window=8192))
    r = await _call(k, "v1", {"type": "send", "text": "hi", "client_id": "t"})
    assert r["reason"] == "bad_arg" and r["arg"] == "kv_slots", r
    assert "answered HTTP 404" in r["error"], r
    assert k.get("v1").get("kv_slots") is None
    assert not any(u.endswith("/chat/completions") for u, _ in seen), seen


async def test_a_verb_that_runs_no_turn_reads_nothing(seeded_kernel, mock_http):
    k = seeded_kernel
    seen: list = []
    mock_http(_vllm(seen))
    await _create(k, id="v1", **_rec())
    for verb in ("reflect", "status", "context_status", "rows"):
        await _call(k, "v1", {"type": verb})
    assert seen == [] and k.get("v1").get("kv_slots") is None


async def test_a_key_the_store_refuses_refuses_naming_the_first_field():
    from ai_core.credentials import CredentialError

    async def refused(endpoint):
        raise CredentialError("vllm_backend: the store refused the key")

    k = _Kernel(_rec(model=MODEL))
    r = await _resolve(k, _server([]), key=refused)
    assert r == {
        "error": "vllm_backend: the store refused the key",
        "reason": "bad_arg",
        "arg": "max_tokens",
    }, r


async def test_a_llama_cpp_entry_gives_its_window_as_meta_n_ctx():
    body = json.dumps(
        {
            "object": "list",
            "data": [{"id": MODEL, "object": "model", "meta": {"n_ctx": 131072}}],
        }
    )
    k = _Kernel(_rec(model=MODEL, kv_slots=6))
    assert await _resolve(k, _server([], models=body)) is None
    assert k.updates == [{"max_tokens": 131072, "context_window": 131072}]


async def test_an_entry_with_neither_max_model_len_nor_n_ctx_is_refused():
    body = json.dumps(
        {"object": "list", "data": [{"id": MODEL, "object": "model", "meta": {}}]}
    )
    k = _Kernel(_rec(kv_slots=6))
    r = await _resolve(k, _server([], models=body))
    assert r["reason"] == "bad_arg" and r["arg"] == "max_tokens", r
    assert "names no max_model_len or meta.n_ctx" in r["error"], r
    assert k.updates == []
