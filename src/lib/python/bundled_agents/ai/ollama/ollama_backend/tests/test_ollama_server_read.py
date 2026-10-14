"""ollama_backend — a `model` the record leaves out is READ FROM THE SERVER: the ONE
model `GET /api/tags` lists, read before a verb that runs a
turn and written back. Only the model: the window ollama loads with is not in the
API. An optional key (`credentials`) rides every request, the model list included."""

from __future__ import annotations

import json
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest
from ollama_backend import server
from ollama_backend.provider import OllamaProvider
from ollama_backend.tools import validate_meta

ENDPOINT = "http://ollama.test"


def _tags(*names: str) -> str:
    return json.dumps({"models": [{"name": n, "model": n, "size": 1} for n in names]})


def _server(seen: list, body: str | None = None, status: int = 200):
    def handler(req: httpx.Request) -> httpx.Response:
        seen.append((str(req.url), req.headers.get("authorization")))
        return httpx.Response(status, text=_tags("qwen3:8b") if body is None else body)

    return httpx.MockTransport(handler)


async def _nokey(endpoint):
    return None


async def _key(endpoint):
    return "sekret"


class _Kernel:
    def __init__(self, rec: dict):
        self.rec = {"id": "o1", "endpoint": ENDPOINT, **rec}
        self.updates: list[dict] = []

    def get(self, id):
        return self.rec if id == "o1" else None

    def update(self, id, **meta):
        self.updates.append(meta)
        self.rec.update(meta)


async def _resolve(k, transport, key=_nokey):
    return await server.resolve("o1", k, key=key, transport=transport)


async def test_a_left_out_model_is_the_one_the_server_lists_and_is_written_back():
    seen: list = []
    k = _Kernel({})
    assert await _resolve(k, _server(seen)) is None
    assert k.updates == [{"model": "qwen3:8b"}]
    assert seen == [("http://ollama.test/api/tags", None)]
    seen.clear()
    assert await _resolve(k, _server(seen)) is None
    assert seen == [] and len(k.updates) == 1


async def test_a_model_the_record_sets_is_never_read():
    seen: list = []
    k = _Kernel({"model": "gemma4:12b"})
    assert await _resolve(k, _server(seen)) is None
    assert seen == [] and k.updates == []


@pytest.mark.parametrize(
    "body, words",
    [
        (
            _tags("a:1", "b:2"),
            "serves a:1, b:2 — not one model; set model on the record",
        ),
        (_tags(), "serves nothing — not one model; set model on the record"),
        ("<html>", "is not a model list"),
    ],
)
async def test_a_server_without_one_model_refuses_naming_what_it_serves(body, words):
    k = _Kernel({})
    r = await _resolve(k, _server([], body))
    assert r["reason"] == "bad_arg" and r["arg"] == "model", r
    assert words in r["error"], r
    assert k.updates == []


async def test_a_server_that_does_not_answer_refuses_naming_the_url():
    k = _Kernel({})
    r = await _resolve(k, _server([], "nope", status=404))
    assert r == {
        "error": "model: GET http://ollama.test/api/tags answered HTTP 404 — the server "
        "(or a door in front of it) does not serve it; set model on the record",
        "reason": "bad_arg",
        "arg": "model",
    }, r

    def down(req):
        raise httpx.ConnectError("refused", request=req)

    r = await _resolve(_Kernel({}), httpx.MockTransport(down))
    assert r["error"] == (
        "model: GET http://ollama.test/api/tags failed — the server is not reachable "
        "there; set model on the record"
    ), r


async def test_the_key_rides_the_model_list_when_the_record_names_one():
    seen: list = []
    await _resolve(_Kernel({}), _server(seen), key=_key)
    assert seen == [("http://ollama.test/api/tags", "Bearer sekret")]


async def test_the_key_rides_the_chat_only_when_set():
    def client():
        c = MagicMock()

        async def stream():
            yield {"message": {"content": "ok"}}

        c.chat = AsyncMock(return_value=stream())
        c.__aenter__ = AsyncMock(return_value=c)
        c.__aexit__ = AsyncMock(return_value=False)
        return c

    with patch("ollama.AsyncClient", return_value=client()) as made:
        p = OllamaProvider(endpoint=ENDPOINT, model="m", key=_key)
        assert [x async for x in p.chat([{"role": "user", "content": "hi"}])] == ["ok"]
    made.assert_called_once_with(
        host=ENDPOINT, headers={"Authorization": "Bearer sekret"}
    )
    with patch("ollama.AsyncClient", return_value=client()) as made:
        p = OllamaProvider(endpoint=ENDPOINT, model="m", key=_nokey)
        assert [x async for x in p.chat([{"role": "user", "content": "hi"}])] == ["ok"]
    made.assert_called_once_with(host=ENDPOINT)


def test_a_record_with_only_endpoint_and_a_store_is_accepted():
    assert (
        validate_meta({"endpoint": ENDPOINT, "memory_store_id": "memory"}, {}) is None
    )
    err = validate_meta({"memory_store_id": "memory"}, {})
    assert err and "missing: endpoint —" in err, err


# ─── through the kernel ──────────────────────────────────────────────────────


@pytest.fixture
def mock_http(monkeypatch):
    real = httpx.AsyncClient

    def use(transport):
        monkeypatch.setattr(
            httpx, "AsyncClient", lambda **kw: real(**{**kw, "transport": transport})
        )

    return use


async def _call(kernel, target, payload):
    return (await kernel.send([{"target": target, "payload": payload}]))[0]["result"]


async def _create(kernel, **meta):
    return await _call(
        kernel,
        kernel.id,
        {
            "type": "create_agent",
            "parent_id": kernel.id,
            "handler_module": "ollama_backend.tools",
            "id": "o1",
            "endpoint": ENDPOINT,
            "memory_store_id": "memory",
            **meta,
        },
    )


async def test_a_send_reads_the_model_first(seeded_kernel, mock_http, fake_providers):
    seen: list = []
    mock_http(_server(seen))
    r = await _create(seeded_kernel)
    assert r.get("id") == "o1", r
    await _call(seeded_kernel, "o1", {"type": "send", "text": "hi", "client_id": "t"})
    assert seeded_kernel.get("o1")["model"] == "qwen3:8b"
    assert seen[0] == ("http://ollama.test/api/tags", None)


async def test_a_send_the_server_cannot_answer_is_refused(seeded_kernel, mock_http):
    mock_http(_server([], "nope", status=404))
    await _create(seeded_kernel)
    r = await _call(
        seeded_kernel, "o1", {"type": "send", "text": "hi", "client_id": "t"}
    )
    assert r["reason"] == "bad_arg" and r["arg"] == "model", r
    assert seeded_kernel.get("o1").get("model") is None


async def test_a_verb_that_runs_no_turn_reads_nothing(seeded_kernel, mock_http):
    seen: list = []
    mock_http(_server(seen))
    await _create(seeded_kernel)
    for verb in ("reflect", "status", "context_status", "rows"):
        await _call(seeded_kernel, "o1", {"type": verb})
    assert seen == []
