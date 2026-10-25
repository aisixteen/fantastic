"""The coder+reviewer lane (E48 peer_review) names its roles (#174):
`FANTASTIC_AI_MODEL_CODER` / `FANTASTIC_AI_MODEL_REVIEWER` pick each model, both
falling back to `FANTASTIC_AI_MODEL`. No model or network is reached.
"""

from __future__ import annotations

import importlib.util
import os
import sys
from pathlib import Path

import pytest

E2E = next(p for p in Path(__file__).resolve().parents if (p / "e2e" / ".e2e-root").exists()) / "e2e"
sys.path.insert(0, str(E2E))
PROBE = E2E / "tests" / "coding" / "E48_peer_review" / "probe.py"

ENDPOINT = "https://api.example.test/v1/messages"
KEY = "sk-ant-TEST-KEY-DO-NOT-LOG"
MODEL = "test-model-default"
CODER = "haiku-x"
REVIEWER = "sonnet-y"


@pytest.fixture
def plug_env(monkeypatch):
    for var in ("ANTHROPIC_API_KEY", "ANTHROPIC_ENDPOINT", "FANTASTIC_AI_MODEL", "BACKEND"):
        monkeypatch.delenv(var, raising=False)
    for var in list(os.environ):
        if var.startswith("FANTASTIC_AI_MODEL_"):
            monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("ANTHROPIC_ENDPOINT", ENDPOINT)
    monkeypatch.setenv("ANTHROPIC_API_KEY", KEY)
    monkeypatch.setenv("FANTASTIC_AI_MODEL", MODEL)
    monkeypatch.setenv("BACKEND", "anthropic")
    return monkeypatch


def _probe():
    spec = importlib.util.spec_from_file_location("e48_peer_review_probe", PROBE)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def test_review_lane_default_model_for_both_roles(plug_env):
    p = _probe()
    recs = p.lane_records(p.BK)
    assert recs["coder"]["model"] == MODEL
    assert recs["reviewer"]["model"] == MODEL


def test_review_lane_coder_and_reviewer_get_different_models(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_CODER", CODER)
    plug_env.setenv("FANTASTIC_AI_MODEL_REVIEWER", REVIEWER)
    p = _probe()
    recs = p.lane_records(p.BK)
    assert recs["coder"]["model"] == CODER
    assert recs["reviewer"]["model"] == REVIEWER


def test_review_lane_override_one_role_only(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_REVIEWER", REVIEWER)
    p = _probe()
    recs = p.lane_records(p.BK)
    assert recs["coder"]["model"] == MODEL
    assert recs["reviewer"]["model"] == REVIEWER


def test_review_lane_records_differ_only_in_model(plug_env):
    plug_env.setenv("FANTASTIC_AI_MODEL_CODER", CODER)
    plug_env.setenv("FANTASTIC_AI_MODEL_REVIEWER", REVIEWER)
    p = _probe()
    recs = p.lane_records(p.BK)
    strip = lambda r: {k: v for k, v in r.items() if k != "model"}
    assert strip(recs["coder"]) == strip(recs["reviewer"])


def test_review_lane_roles_used_header_order(plug_env):
    p = _probe()
    p.lane_records(p.BK)
    assert list(p.BK.roles_used()) == ["coder", "reviewer"]


def test_review_lane_probe_passes_role_records_to_connect():
    src = PROBE.read_text()
    assert 'record=recs["coder"]' in src
    assert 'record=recs["reviewer"]' in src
