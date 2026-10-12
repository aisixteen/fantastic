"""The three AI backends' RECORD surface, pinned: schema, write-time checks, the
provider each record builds, and the reflect extras each record shows.

Every backend coerces its record twice today — once in `make_provider` and once
in `_reflect_extra` — and wraps `ai_core.schema.validate` in its own
`validate_meta`. These tests hold the observable result of each, per backend and
per knob (absent and present), so collapsing that into one schema-driven path
(reduction candidate B) keeps what every backend builds, shows and refuses.
"""

from __future__ import annotations

import importlib

import pytest
from _testkit import ensure_memory

from ai_core.schema import AI_FIELDS, field, schema, validate

BACKENDS = (
    "ollama_backend",
    "responses_backend",
    "vllm_backend",
    "anthropic_backend",
)

_AI_TAIL = [
    ("memory_store_id", "str", True, None, None),
    ("context_window", "int", False, 131072, None),
    ("output_reserve", "int", False, 1024, None),
    (
        "context_strategy",
        "enum",
        False,
        "compact",
        ["compact", "truncate", "strip", "fold"],
    ),
    ("recent_n", "int", False, 6, None),
    ("system_prompt", "str", False, None, None),
    ("file_bridge_id", "str", False, None, None),
    ("system_prompt_file", "str", False, None, None),
    ("pre_system_prompt", "str", False, None, None),
    ("post_system_prompt", "str", False, None, None),
]


#: the shared fields a backend declares itself (its own field replaces the shared
#: one): vllm_backend's `context_window`, read from the server when left out.
_OVERRIDDEN = {
    "vllm_backend": ("context_window",),
    "anthropic_backend": ("context_window",),
}


def _tail(backend):
    own = _OVERRIDDEN.get(backend, ())
    return [t for t in _AI_TAIL if t[0] not in own]


#: (name, type, required, default-or-None, enum-or-None) per field, in order.
SCHEMAS = {
    "ollama_backend": (
        [
            ("endpoint", "str", True, None, None),
            ("model", "str", False, None, None),
            ("num_ctx", "int", False, None, None),
            ("temperature", "float", False, 0.3, None),
            ("num_predict", "int", False, None, None),
            ("think", "bool", False, None, None),
            ("credentials", "credentials", False, None, None),
        ]
    ),
    "responses_backend": (
        [
            ("endpoint", "str", True, None, None),
            ("model", "str", True, None, None),
            ("max_tokens", "int", False, 8192, None),
            ("sampling", "map", False, None, None),
            ("credentials", "credentials", True, None, None),
        ]
    ),
    "anthropic_backend": (
        [
            ("endpoint", "str", True, None, None),
            ("model", "str", True, None, None),
            ("max_tokens", "int", False, None, None),
            ("context_window", "int", False, None, None),
            ("temperature", "float", False, None, None),
            ("thinking_budget", "int", False, None, None),
            ("credentials", "credentials", False, None, None),
        ]
    ),
    "vllm_backend": (
        [
            ("endpoint", "str", True, None, None),
            ("model", "str", False, None, None),
            ("max_tokens", "int", False, None, None),
            ("context_window", "int", False, None, None),
            ("kv_slots", "int", False, None, None),
            ("metrics_url", "str", False, None, None),
            ("kv_overflow", "enum", True, None, ["evict", "reject"]),
            ("temperature", "float", False, 0.2, None),
            ("top_p", "float", False, 0.8, None),
            ("top_k", "int", False, 20, None),
            ("reasoning_effort", "enum", False, "medium", ["low", "medium", "xhigh"]),
            ("thinking_token_budget", "int", False, 16000, None),
            ("min_p", "float", False, None, None),
            ("presence_penalty", "float", False, None, None),
            ("frequency_penalty", "float", False, None, None),
            ("repetition_penalty", "float", False, None, None),
            ("no_repeat_ngram_size", "int", False, None, None),
            ("seed", "int", False, None, None),
            ("stop", "list", False, None, None),
            ("kv_ttl", "float", False, 1800.0, None),
            ("connect_timeout", "float", False, 5.0, None),
            ("read_timeout", "float", False, 900.0, None),
            ("credentials", "credentials", False, None, None),
        ]
    ),
}


def _tools(backend: str):
    return importlib.import_module(f"{backend}.tools")


def _expected_field(name, type_, required, default, enum) -> list:
    keys = ["name", "type", "required"]
    if enum is not None:
        keys.append("enum")
    if default is not None:
        keys.append("default")
    keys.append("hint")
    return keys


# ─── the schema ──────────────────────────────────────────────────────────────


@pytest.mark.parametrize("backend", BACKENDS)
def test_each_backend_schema_is_exact(backend):
    own = SCHEMAS[backend]
    sch = _tools(backend).RECORD_SCHEMA
    # no `key_var`: a key is never read from the environment — a backend that
    # sends one has a `credentials` field
    assert list(sch) == ["fields"]
    want = own + _tail(backend)
    got = [
        (f["name"], f["type"], f["required"], f.get("default"), f.get("enum"))
        for f in sch["fields"]
    ]
    assert got == want
    for f, w in zip(sch["fields"], want):
        # field key order is wire shape (the catalog is JSON a page renders)
        assert list(f) == _expected_field(*w), f
        assert isinstance(f["hint"], str) and f["hint"].strip(), f


async def test_the_catalog_serves_each_backend_schema_verbatim(seeded_kernel):
    r = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "reflect", "bundles": "all"}}]
        )
    )[0]["result"]
    by = {b["name"]: b for b in r["bundles"]}
    for backend in BACKENDS:
        assert by[backend]["schema"] == _tools(backend).RECORD_SCHEMA, backend
        assert by[backend]["handler_module"] == f"{backend}.tools"


def test_the_schema_builders_refuse_malformed_fields():
    with pytest.raises(ValueError) as e:
        field("x", "number", "h")
    assert str(e.value) == (
        "field 'x': type 'number' is not one of "
        "('str', 'int', 'float', 'bool', 'enum', 'list', 'map', 'credentials')"
    )
    with pytest.raises(ValueError) as e:
        field("x", "enum", "h")
    assert str(e.value) == "field 'x': `enum` values go with type 'enum', only"
    with pytest.raises(ValueError) as e:
        field("x", "str", "h", enum=("a",))
    assert str(e.value) == "field 'x': `enum` values go with type 'enum', only"
    with pytest.raises(ValueError) as e:
        field("x", "int", "h", required=True, default=3)
    assert str(e.value) == "field 'x': a required field has no default"
    # a backend field named like a shared one REPLACES it, in the backend's place
    sch = schema([field("recent_n", "int", "h")])
    assert [f["name"] for f in sch["fields"]].count("recent_n") == 1
    assert sch["fields"][0] == {
        "name": "recent_n",
        "type": "int",
        "required": False,
        "hint": "h",
    }
    with pytest.raises(ValueError) as e:
        schema([field("x", "int", "h"), field("x", "int", "h")])
    assert str(e.value).startswith("duplicate field in schema: ['x', 'x', ")
    assert [f["name"] for f in AI_FIELDS] == [t[0] for t in _AI_TAIL]


# ─── the write-time check ────────────────────────────────────────────────────


@pytest.mark.parametrize("backend", BACKENDS)
def test_validate_meta_names_every_missing_required_field(backend):
    own = SCHEMAS[backend]
    required = [n for (n, _t, req, _d, _e) in own + _tail(backend) if req]
    err = _tools(backend).validate_meta({}, {})
    assert err is not None
    assert err.startswith(
        f"{backend}: required on the record and missing: {', '.join(required)} "
        "— nothing is guessed. "
    ), err
    # each named WITH its hint, `; `-joined, in schema order
    sch = {f["name"]: f for f in _tools(backend).RECORD_SCHEMA["fields"]}
    assert err.endswith("; ".join(f"{n} = {sch[n]['hint']}" for n in required)), err


@pytest.mark.parametrize("backend", BACKENDS)
def test_validate_meta_judges_the_merged_record(backend):
    own = SCHEMAS[backend]
    full = {
        # each backend's own endpoint shape (anthropic reads its model list beside
        # a `/messages` URL)
        "endpoint": "http://x/v1/messages"
        if backend == "anthropic_backend"
        else "http://x/v1",
        "model": "m",
        "max_tokens": 8000,
        "kv_slots": 2,
        "kv_overflow": "evict",
        "context_window": 262144,
        "memory_store_id": "memory",
        "credentials": {"store": "creds", "ref": "r"},
    }
    rec = {n: full[n] for (n, _t, req, _d, _e) in own + _tail(backend) if req}
    assert _tools(backend).validate_meta({}, rec) is None
    # a blank string is absent; None is absent
    blank = {**rec, "endpoint": "  "}
    assert "missing: endpoint" in _tools(backend).validate_meta({}, blank)
    assert "missing: endpoint" in _tools(backend).validate_meta({"endpoint": None}, rec)


@pytest.mark.parametrize("backend", BACKENDS)
def test_an_enum_outside_its_set_is_refused_with_the_generic_text(backend):
    err = _tools(backend).validate_meta({"context_strategy": "zip"}, {})
    assert err == (
        "context_strategy='zip' is not one of the legal values: "
        "compact, truncate, strip, fold"
    )


@pytest.mark.parametrize(
    "meta, text",
    [
        (
            {"reasoning_effort": "high"},
            "reasoning_effort='high' is not one vLLM accepts — legal values: low, "
            'medium, xhigh ("high" answers 400; xhigh needs a thinking_token_budget '
            "or it can think without answering)",
        ),
        (
            {"kv_overflow": "drop"},
            "kv_overflow='drop' is not a rotation policy — legal values: evict "
            "(the least-recently-used conversation rotates out), reject (refuse "
            "the newcomer)",
        ),
        ({"max_tokens": 0}, "max_tokens must be a positive int"),
        ({"kv_slots": 1.5}, "kv_slots must be a positive int"),
        ({"kv_ttl": True}, "kv_ttl must be a positive number of seconds"),
        (
            {"connect_timeout": 0},
            "connect_timeout must be a positive number of seconds",
        ),
        ({"read_timeout": "9"}, "read_timeout must be a positive number of seconds"),
        ({"thinking_token_budget": 0}, "thinking_token_budget must be a positive int"),
        (
            {"thinking_token_budget": True},
            "thinking_token_budget must be a positive int",
        ),
        (
            {"thinking_token_budget": 9000, "max_tokens": 9000},
            "thinking_token_budget=9000 is not below max_tokens=9000 — it could "
            "never fire, and a turn would end mid-thought with no answer; keep it "
            "below max_tokens (half is a good start)",
        ),
    ],
)
def test_vllm_own_refusal_texts_are_exact(meta, text):
    from vllm_backend.tools import validate_meta

    assert validate_meta(meta, {}) == text


def test_vllm_checks_run_before_the_schema_and_a_none_skips_them():
    from vllm_backend.tools import validate_meta

    # the vllm-specific enum text wins over schema.validate's generic one
    assert validate_meta({"kv_overflow": "x"}, {}).startswith(
        "kv_overflow='x' is not a rotation policy"
    )
    # None means "not set" for every own check — only the schema speaks then
    err = validate_meta(
        {
            "max_tokens": None,
            "kv_slots": None,
            "kv_ttl": None,
            "thinking_token_budget": None,
        },
        {},
    )
    assert err.startswith("vllm_backend: required on the record and missing: ")


def test_validate_is_the_shared_body():
    sch = schema([field("a", "enum", "pick", enum=("x", "y"))])
    assert validate(sch, "b", {"a": "z"}, {}) == (
        "a='z' is not one of the legal values: x, y"
    )


# ─── the record → provider (make_provider) ───────────────────────────────────


class _Spy:
    """Wraps a provider class's __init__ and records the kwargs it was built
    with — the observable of `make_provider`. The `key` reader (a closure) is
    kept apart, in `keys`."""

    def __init__(self, monkeypatch, cls):
        self.calls: list = []
        self.keys: list = []
        real = cls.__init__
        spy = self

        def init(obj, *a, **kw):
            rest = dict(kw)
            spy.keys.append(rest.pop("key", None))
            spy.calls.append((a, rest))
            real(obj, *a, **kw)

        monkeypatch.setattr(cls, "__init__", init)


class _K:
    """The one surface `make_provider` reads: `kernel.get(id)`."""

    def __init__(self, rec):
        self.rec = rec

    def get(self, id):
        return self.rec


def test_ollama_make_provider_coerces_each_knob(monkeypatch):
    from ollama_backend.provider import OllamaProvider
    from ollama_backend.tools import make_provider

    spy = _Spy(monkeypatch, OllamaProvider)
    make_provider("o", _K({"endpoint": "http://e", "model": "m"}))
    make_provider(
        "o",
        _K(
            {
                "endpoint": "http://e",
                "model": "m",
                "num_ctx": "4096",
                "temperature": "0.5",
                "num_predict": 0,
                "think": 0,
            }
        ),
    )
    assert spy.calls == [
        (
            (),
            {
                "endpoint": "http://e",
                "model": "m",
                "num_ctx": None,
                "temperature": None,
                "num_predict": None,
                "think": None,
            },
        ),
        (
            (),
            {
                "endpoint": "http://e",
                "model": "m",
                "num_ctx": 4096,
                "temperature": 0.5,
                "num_predict": None,  # 0 is "unset" for a budget
                "think": False,  # a set falsy think is still sent
            },
        ),
    ]


def test_responses_make_provider_from_the_record(monkeypatch):
    from responses_backend.provider import ResponsesProvider
    from responses_backend.tools import make_provider as mk

    spy = _Spy(monkeypatch, ResponsesProvider)
    mk("a", _K({"endpoint": "http://e", "model": "m"}))
    mk(
        "a",
        _K(
            {
                "endpoint": "http://e",
                "model": "m",
                "max_tokens": "100",
                "sampling": {"top_k": 3},
            }
        ),
    )
    assert spy.calls == [
        (
            (),
            {
                "endpoint": "http://e",
                "model": "m",
                "max_tokens": 8192,
                "sampling": None,
            },
        ),
        (
            (),
            {
                "endpoint": "http://e",
                "model": "m",
                "max_tokens": 100,
                "sampling": {"top_k": 3},
            },
        ),
    ]
    assert all(callable(k) for k in spy.keys) and len(spy.keys) == 2


def test_vllm_make_provider_builds_the_effective_posture(monkeypatch):
    from vllm_backend.provider import VllmProvider
    from vllm_backend.tools import make_provider

    spy = _Spy(monkeypatch, VllmProvider)
    base = {
        "endpoint": "http://e",
        "model": "m",
        "max_tokens": 8000,
        "context_window": 262144,
        "kv_slots": 3,
        "kv_overflow": "reject",
    }
    make_provider("v", _K(base))
    make_provider(
        "v",
        _K(
            {
                **base,
                "temperature": 0.9,
                "thinking_token_budget": 2000,
                "seed": 7,
                "presence_penalty": None,
                "kv_ttl": 60,
                "connect_timeout": 2,
                "read_timeout": 30,
            }
        ),
    )
    (_, a), (_, b) = spy.calls
    assert a == {
        "agent_id": "v",
        "endpoint": "http://e",
        "model": "m",
        "max_tokens": 8000,
        # the record's `context_window`, as set
        "window": 262144,
        "kv_slots": 3,
        "kv_overflow": "reject",
        "kv_ttl": 1800.0,
        "sampling": {
            "temperature": 0.2,
            "top_p": 0.8,
            "top_k": 20,
            "reasoning_effort": "medium",
            "thinking_token_budget": 16000,
        },
        "explicit_budget": False,
        "connect_timeout": 5.0,
        "read_timeout": 900.0,
    }
    assert b["sampling"] == {
        "temperature": 0.9,
        "top_p": 0.8,
        "top_k": 20,
        "reasoning_effort": "medium",
        "thinking_token_budget": 2000,
        "seed": 7,
    }
    assert b["explicit_budget"] is True
    assert (b["kv_ttl"], b["connect_timeout"], b["read_timeout"]) == (60.0, 2.0, 30.0)


def test_vllm_window_is_the_records_context_window_as_set(monkeypatch):
    """`context_window` is the server's window — set on the record, or read from
    the server before the turn — and used as set (never a default); the
    whole-window rule compares max_tokens with it."""
    from vllm_backend.provider import VllmProvider
    from vllm_backend.tools import make_provider

    spy = _Spy(monkeypatch, VllmProvider)
    base = {
        "endpoint": "http://e",
        "model": "m",
        "max_tokens": 262144,
        "kv_slots": 1,
        "kv_overflow": "evict",
    }
    make_provider("v", _K({**base, "context_window": 262144}))
    ((_, a),) = spy.calls
    assert a["window"] == 262144
    with pytest.raises(ValueError, match="`context_window` is not on the agent record"):
        make_provider("v", _K(base))


@pytest.mark.parametrize(
    "rec, text",
    [
        (
            {"model": "m", "max_tokens": 1, "kv_slots": 1, "kv_overflow": "evict"},
            "vllm_backend: `endpoint` is required on the agent record — nothing is "
            "guessed (update_agent <id> endpoint=…)",
        ),
        (
            {"endpoint": "e", "model": "m", "kv_slots": 1, "kv_overflow": "evict"},
            "vllm_backend: `max_tokens` is not on the agent record, a positive int, and "
            "was not read from the server (update_agent <id> max_tokens=…)",
        ),
        (
            {
                "endpoint": "e",
                "model": "m",
                "max_tokens": 1,
                "context_window": 1,
                "kv_slots": True,
                "kv_overflow": "evict",
            },
            "vllm_backend: `kv_slots` is not on the agent record, a positive int, and "
            "was not read from the server (update_agent <id> kv_slots=…)",
        ),
        (
            {"endpoint": "e", "model": "m", "max_tokens": 1, "kv_slots": 1},
            "vllm_backend: `kv_overflow` is required on the agent record — evict "
            "(the least-recently-used conversation rotates out) or reject (refuse "
            "the newcomer) (update_agent <id> kv_overflow=…)",
        ),
    ],
)
def test_vllm_make_provider_refusal_texts(rec, text):
    from vllm_backend.tools import make_provider

    with pytest.raises(ValueError) as e:
        make_provider("v", _K(rec))
    assert str(e.value) == text


@pytest.mark.parametrize("backend", ["ollama_backend", "responses_backend"])
def test_required_meta_failure_text(backend):
    with pytest.raises(ValueError) as e:
        _tools(backend).make_provider("x", _K({"endpoint": " ", "model": "m"}))
    assert str(e.value) == (
        f"{backend}: `endpoint` is required on the agent record — nothing is "
        "guessed (update_agent <id> endpoint=…)"
    )


# ─── the reflect extras, per knob absent and present ─────────────────────────


async def _create(k, backend: str, aid: str, **meta) -> dict:
    store = ensure_memory(k)
    payload = {
        "type": "create_agent",
        "parent_id": k.id,
        "handler_module": f"{backend}.tools",
        "id": aid,
        "endpoint": "http://127.0.0.1:9/v1",
        "model": "stub",
        "memory_store_id": store,
        **meta,
    }
    rec = (await k.send([{"target": k.id, "payload": payload}]))[0]["result"]
    assert rec.get("id") == aid, rec
    return rec


async def _reflect(k, aid: str) -> dict:
    return (await k.send([{"target": aid, "payload": {"type": "reflect"}}]))[0][
        "result"
    ]


async def _update(k, aid: str, **meta) -> None:
    out = (
        await k.send(
            [{"target": k.id, "payload": {"type": "update_agent", "id": aid, **meta}}]
        )
    )[0]["result"]
    assert "error" not in out, out


_SHARED = (
    "id",
    "sentence",
    "model",
    "endpoint",
    "context_window",
    "context_strategy",
    "memory_store_id",
    "generating",
)


async def test_ollama_reflect_extras(seeded_kernel):
    k = seeded_kernel
    await _create(k, "ollama_backend", "o1")
    r = await _reflect(k, "o1")
    assert {x: r[x] for x in ("temperature", "num_ctx", "num_predict", "think")} == {
        "temperature": 0.3,
        "num_ctx": None,
        "num_predict": None,
        "think": None,
    }
    assert list(r)[: len(_SHARED)] == list(_SHARED)
    await _update(k, "o1", temperature=1, num_ctx=8192, num_predict=128, think=False)
    r = await _reflect(k, "o1")
    assert {x: r[x] for x in ("temperature", "num_ctx", "num_predict", "think")} == {
        "temperature": 1.0,
        "num_ctx": 8192,
        "num_predict": 128,
        "think": False,
    }
    assert isinstance(r["temperature"], float)


async def test_responses_reflects_no_extras(seeded_kernel):
    k = seeded_kernel
    await _create(
        k,
        "responses_backend",
        "a1",
        max_tokens=100,
        sampling={"top_k": 3},
        credentials={"store": "creds", "ref": "r"},
    )
    r = await _reflect(k, "a1")
    assert list(r) == list(_SHARED) + [
        "can_call",
        "callable_by",
        "tree",
    ]
    assert (r["model"], r["endpoint"]) == ("stub", "http://127.0.0.1:9/v1")


async def test_vllm_reflect_extras_with_every_knob_set(seeded_kernel):
    k = seeded_kernel
    await _create(
        k,
        "vllm_backend",
        "v1",
        endpoint="http://vllm.pin/v1",
        max_tokens=8000,
        context_window=262144,
        kv_slots=4,
        kv_overflow="reject",
        thinking_token_budget=3000,
        top_k=5,
        stop=["</s>"],
        kv_ttl=90,
        connect_timeout=3,
        read_timeout=60,
    )
    r = await _reflect(k, "v1")
    assert r["sampling"] == {
        "temperature": 0.2,
        "top_p": 0.8,
        "top_k": 5,
        "reasoning_effort": "medium",
        "thinking_token_budget": 3000,
        "stop": ["</s>"],
    }
    assert r["thinking_token_budget_source"] == "record"
    assert r["last_thinking_budget"] is None
    assert r["max_tokens"] == 8000
    assert r["timeouts"] == {"connect": 3.0, "read": 60.0}
    assert r["kv"] == {
        "slots": 4,
        "overflow": "reject",
        "ttl": 90.0,
        "live": 0,
        "conversations": [],
        "evicted": 0,
        "rejected": 0,
    }
    extra = (
        "sampling",
        "thinking_token_budget_source",
        "last_thinking_budget",
        "max_tokens",
        "timeouts",
        "kv",
    )
    assert list(r)[: len(_SHARED) + len(extra)] == list(_SHARED) + list(extra)
