"""A model per role (#174): `FANTASTIC_AI_MODEL` is the default model for every AI a
probe creates; `FANTASTIC_AI_MODEL_<ROLE>` overrides it for that role only. A
self-managed-groups run gives the master one model and its workers another.

No model is reached here: the plug builds records from the environment the test
monkeypatches, and nothing touches a network.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

import pytest

E2E = next(p for p in Path(__file__).resolve().parents if (p / "e2e" / ".e2e-root").exists()) / "e2e"
sys.path.insert(0, str(E2E))

from backends import backend  # noqa: E402

ENDPOINT = "https://api.example.test/v1/messages"
KEY = "sk-ant-TEST-KEY-DO-NOT-LOG"
MODEL = "test-model-default"
WORKER = "test-model-worker"
MASTER = "test-model-master"


@pytest.fixture
def plug_env(monkeypatch):
    for var in ("ANTHROPIC_API_KEY", "ANTHROPIC_ENDPOINT", "FANTASTIC_AI_MODEL", "MAX_TOKENS",
                "ANTHROPIC_CACHING", "ANTHROPIC_PRICE_INPUT_PER_MTOK",
                "ANTHROPIC_PRICE_OUTPUT_PER_MTOK", "BACKEND"):
        monkeypatch.delenv(var, raising=False)
    for var in list(os.environ):
        if var.startswith("FANTASTIC_AI_MODEL_"):
            monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("ANTHROPIC_ENDPOINT", ENDPOINT)
    monkeypatch.setenv("ANTHROPIC_API_KEY", KEY)
    monkeypatch.setenv("FANTASTIC_AI_MODEL", MODEL)
    monkeypatch.setenv("ANTHROPIC_CACHING", "provider_cache")
    return monkeypatch


def test_default_model_for_every_role_when_no_override(plug_env):
    bk = backend("anthropic")
    assert bk.model_for("coder") == MODEL
    assert bk.model_for("master") == MODEL
    assert bk.record(role="coder")["model"] == MODEL


def test_role_override_applies_to_that_role_only(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_WORKER", WORKER)
    bk = backend("anthropic")
    assert bk.model_for("worker") == WORKER
    assert bk.model_for("master") == MODEL
    assert bk.record(role="worker")["model"] == WORKER
    assert bk.record(role="master")["model"] == MODEL


def test_two_roles_get_two_models(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_WORKER", WORKER)
    plug_env.setenv("FANTASTIC_AI_MODEL_MASTER", MASTER)
    bk = backend("anthropic")
    assert bk.record(role="master")["model"] == MASTER
    assert bk.record(role="worker")["model"] == WORKER


def test_unknown_role_gets_default(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_WORKER", WORKER)
    assert backend("anthropic").model_for("nobody_heard_of_it") == MODEL


def test_empty_override_falls_back_to_default(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_WORKER", "")
    assert backend("anthropic").model_for("worker") == MODEL


def test_role_name_is_normalised(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_REVIEW_LANE", WORKER)
    bk = backend("anthropic")
    assert bk.model_for("review-lane") == WORKER
    assert bk.model_for("Review Lane") == WORKER


def test_record_with_role_keeps_every_other_field(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_WORKER", WORKER)
    bk = backend("anthropic")
    plain = bk.record()
    roled = bk.record(role="worker")
    for key in ("caching", "endpoint", "credentials", "max_tokens", "handler_module"):
        assert roled[key] == plain[key], key
    assert set(roled) == set(plain)
    assert roled["model"] == WORKER and plain["model"] == MODEL


def test_record_overrides_still_win_over_role_model(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_WORKER", WORKER)
    rec = backend("anthropic").record(role="worker", model="x")
    assert rec["model"] == "x"
    # the role picks a model; it is not a record field
    assert "role" not in rec


def test_roles_used_lists_asked_roles_in_order(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_MASTER", MASTER)
    bk = backend("anthropic")
    assert bk.roles_used() == {}
    bk.record(role="master")
    bk.model_for("worker")
    bk.record()  # a roleless record names no role
    assert list(bk.roles_used().items()) == [("master", MASTER), ("worker", MODEL)]
    # a copy: the caller cannot rewrite the plug's ledger
    bk.roles_used()["master"] = "changed"
    assert bk.roles_used()["master"] == MASTER


def test_missing_default_model_is_still_refused(plug_env):
    plug_env.delenv("FANTASTIC_AI_MODEL")
    plug_env.setenv("FANTASTIC_AI_MODEL_MASTER", MASTER)
    with pytest.raises(SystemExit) as why:
        backend("anthropic")
    assert "FANTASTIC_AI_MODEL" in str(why.value)
    # the default restored, the role override applies again
    plug_env.setenv("FANTASTIC_AI_MODEL", MODEL)
    assert backend("anthropic").model_for("master") == MASTER


def test_foundation_models_names_no_model_for_any_role(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_MASTER", MASTER)
    bk = backend("foundation_models")
    rec = bk.record(role="master")
    assert "model" not in rec
    assert rec["handler_module"] == "foundation_models_backend.tools"
    assert bk.model_for("master") == ""


def test_record_without_role_unchanged(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_MASTER", MASTER)
    bk = backend("anthropic")
    rec = bk.record()
    assert bk.roles_used() == {}
    assert rec["handler_module"] == "anthropic_backend.tools"
    assert rec["model"] == MODEL
    assert rec["endpoint"] == ENDPOINT
    assert rec["caching"] == "provider_cache"
    assert rec["credentials"] == {"store": "creds", "ref": "anthropic"}
