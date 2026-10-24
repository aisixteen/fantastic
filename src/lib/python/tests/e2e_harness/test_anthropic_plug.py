"""The Anthropic e2e plug (#302): a run reaches the Messages API through the same
plug point as every other backend, with the key read by the `creds` store at use
time, the meter reading Anthropic's SSE, and no key value in any record or log.

No model is named here: the model, endpoint, prices and caching come from the
environment the test monkeypatches. The mock (`e2e/mocks/anthropic.py`) answers
where a stream is needed; nothing reaches a network.
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

E2E = next(p for p in Path(__file__).resolve().parents if (p / "e2e" / ".e2e-root").exists()) / "e2e"
sys.path.insert(0, str(E2E))

from backends import backend  # noqa: E402
from helpers import creds, meter  # noqa: E402
from mocks.anthropic import AnthropicMock  # noqa: E402

ENDPOINT = "https://api.example.test/v1/messages"
KEY = "sk-ant-TEST-KEY-DO-NOT-LOG"
MODEL = "test-model-anthropic"


@pytest.fixture
def plug_env(monkeypatch):
    for var in ("ANTHROPIC_API_KEY", "ANTHROPIC_ENDPOINT", "FANTASTIC_AI_MODEL", "MAX_TOKENS",
                "ANTHROPIC_CACHING", "ANTHROPIC_PRICE_INPUT_PER_MTOK",
                "ANTHROPIC_PRICE_OUTPUT_PER_MTOK", "BACKEND"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("ANTHROPIC_ENDPOINT", ENDPOINT)
    monkeypatch.setenv("ANTHROPIC_API_KEY", KEY)
    monkeypatch.setenv("FANTASTIC_AI_MODEL", MODEL)
    monkeypatch.setenv("ANTHROPIC_CACHING", "provider_cache")
    monkeypatch.setenv("ANTHROPIC_PRICE_INPUT_PER_MTOK", "3")
    monkeypatch.setenv("ANTHROPIC_PRICE_OUTPUT_PER_MTOK", "15")
    return monkeypatch


def test_anthropic_is_a_registered_plug(plug_env):
    rec = backend("anthropic").record()
    assert rec["handler_module"] == "anthropic_backend.tools"
    assert rec["model"] == MODEL
    assert rec["endpoint"] == ENDPOINT
    assert rec["caching"] == "provider_cache"
    assert rec["price_input_per_mtok"] == 3.0
    assert rec["price_output_per_mtok"] == 15.0
    assert rec["credentials"] == {"store": "creds", "ref": "anthropic"}
    assert KEY not in json.dumps(rec)


def test_creds_entry_seeds_an_env_ref_for_anthropic(plug_env):
    assert creds.env_refs()["anthropic"] == ("ANTHROPIC_API_KEY", ENDPOINT)
    refs = creds.env_refs()
    store = creds.store_record("root", refs)
    assert store["refs"]["anthropic"] == {"host": ENDPOINT, "env": {"api_key": "ANTHROPIC_API_KEY"}}
    # the store names the variable, never its value
    assert KEY not in json.dumps(store)

    plug_env.delenv("ANTHROPIC_ENDPOINT")
    assert "anthropic" not in creds.env_refs()


def test_blockers_name_the_missing_key_and_endpoint(plug_env):
    plug_env.delenv("ANTHROPIC_API_KEY")
    plug_env.delenv("ANTHROPIC_ENDPOINT")
    problems = " ".join(backend("anthropic").blockers())
    assert "ANTHROPIC_API_KEY" in problems
    assert "ANTHROPIC_ENDPOINT" in problems


def test_wrong_endpoint_is_flagged_and_a_bad_caching_value_is_rejected(plug_env):
    import backends

    assert backends.anthropic_endpoint_problem(ENDPOINT) is None
    assert backends.anthropic_endpoint_problem("https://api.example.test/v1/chat/completions")
    assert backends.anthropic_endpoint_problem("ftp://api.example.test/v1/messages")

    plug_env.setenv("ANTHROPIC_CACHING", "bogus")
    with pytest.raises(SystemExit) as why:
        backend("anthropic")
    assert "ANTHROPIC_CACHING" in str(why.value)


def test_preflight_names_missing_anthropic_key_and_endpoint_before_the_model(tmp_path):
    # A fresh interpreter, so the check is the one an operator runs: no model set,
    # no key, no endpoint — all three are named, and the run is not ready.
    env = {"PATH": os.environ.get("PATH", ""), "HOME": str(tmp_path),
           "BACKEND": "anthropic"}
    out = subprocess.run([sys.executable, str(E2E / "preflight.py")], env=env,
                         capture_output=True, text=True, timeout=120)
    assert out.returncode == 1
    for needle in ("ANTHROPIC_API_KEY", "ANTHROPIC_ENDPOINT", "FANTASTIC_AI_MODEL"):
        assert needle in out.stdout, out.stdout


def test_meter_parses_anthropic_usage(plug_env):
    # The frames of a real turn from the mock, not a hand copy of its wire.
    frames: list[dict] = []

    class Capture:
        def stream(self, ctype, chunks):
            for chunk in chunks:
                for line in chunk.decode().split("\n"):
                    if line.startswith("data:"):
                        frames.append(json.loads(line[5:]))

    mock = AnthropicMock(model=MODEL)
    mock.prompt_tokens = 100
    mock.turn(Capture(), {}, "hello there")
    rec: dict = {}
    think: list[str] = []
    said: list[str] = []
    for obj in frames:
        meter._note(rec, obj)
        meter._text(obj, think, said)
    assert rec["prompt_tokens"] == 100
    assert rec["cached_tokens"] == 0
    assert rec["cache_write_tokens"] == 0
    assert rec["completion_tokens"] == len("hello there") // 4
    assert rec["finish_reason"] == "end_turn"
    assert "".join(said) == "hello there"

    # hand-built frames with cache counts and a thinking block
    rec = {}
    think, said = [], []
    hand = [
        {"type": "message_start", "message": {"usage": {
            "input_tokens": 10, "cache_creation_input_tokens": 20,
            "cache_read_input_tokens": 30, "output_tokens": 1}}},
        {"type": "content_block_delta", "index": 0,
         "delta": {"type": "thinking_delta", "thinking": "hmm "}},
        {"type": "content_block_delta", "index": 1,
         "delta": {"type": "text_delta", "text": "hello"}},
        {"type": "message_delta", "delta": {"stop_reason": "end_turn", "stop_sequence": None},
         "usage": {"output_tokens": 7}},
    ]
    for obj in hand:
        meter._note(rec, obj)
        meter._text(obj, think, said)
    assert rec["prompt_tokens"] == 60  # input + cache write + cache read
    assert rec["cached_tokens"] == 30
    assert rec["cache_write_tokens"] == 20
    assert rec["completion_tokens"] == 7
    assert rec["finish_reason"] == "end_turn"
    assert "".join(think) == "hmm "
    assert "".join(said) == "hello"

    # an OpenAI-shaped chunk still parses as before
    rec = {}
    think, said = [], []
    openai = {"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}],
              "usage": {"prompt_tokens": 5, "completion_tokens": 2,
                        "prompt_tokens_details": {"cached_tokens": 1}}}
    meter._note(rec, openai)
    meter._text(openai, think, said)
    assert (rec["prompt_tokens"], rec["completion_tokens"], rec["cached_tokens"]) == (5, 2, 1)
    assert rec["finish_reason"] == "stop"
    assert said == ["hi"]


def test_meter_forwards_to_anthropic_endpoint_and_keeps_key_out_of_logs(plug_env, tmp_path):
    from starlette.testclient import TestClient

    upstream = AnthropicMock(model=MODEL).require_key(KEY)
    upstream.reply("forwarded answer")
    upstream.start()
    try:
        out = tmp_path / "meter.jsonl"
        body = {"model": MODEL, "max_tokens": 64, "system": "be brief",
                "messages": [{"role": "user", "content": [{"type": "text", "text": "ping"}]}],
                "stream": True}
        with TestClient(meter.app(f"http://127.0.0.1:{upstream.port}", str(out))) as client:
            resp = client.post("/m/case-one/v1/messages", json=body,
                               headers={"x-api-key": KEY, "anthropic-version": "2023-06-01"})
        assert resp.status_code == 200
        assert "message_stop" in resp.text  # the whole stream came back through the meter
        # the upstream saw the key on the header
        assert upstream.keys() == [KEY]
    finally:
        upstream.stop()

    lines = out.read_text().splitlines()
    assert len(lines) == 1
    assert json.loads(lines[0])["tag"] == "case-one"
    # the key is written nowhere under the run's dir
    for path in tmp_path.rglob("*"):
        if path.is_file():
            assert KEY not in path.read_text(errors="replace"), path
    assert (tmp_path / "turns" / "case-one" / "001.txt").exists()


def test_meter_report_prices_a_request_from_the_environment(plug_env):
    from helpers import meter_report

    plug_env.setenv("ANTHROPIC_PRICE_CACHE_READ_PER_MTOK", "0.3")
    plug_env.setenv("ANTHROPIC_PRICE_CACHE_WRITE_PER_MTOK", "3.75")

    # 1M uncached input at 3, 1M cache read at 0.3, 1M cache write at 3.75, 1M output at 15
    usd, long = meter_report.request_usd(
        {"prompt_tokens": 3_000_000, "cache_write_tokens": 1_000_000,
         "cached_tokens": 1_000_000, "completion_tokens": 1_000_000},
        meter_report._price_env())
    assert not long
    assert usd == pytest.approx(3 + 0.3 + 3.75 + 15)

    plug_env.setenv("ANTHROPIC_LONG_CONTEXT_THRESHOLD_TOKENS", "200000")
    plug_env.setenv("ANTHROPIC_PRICE_INPUT_LONG_PER_MTOK", "6")
    plug_env.setenv("ANTHROPIC_PRICE_OUTPUT_LONG_PER_MTOK", "22.5")
    prices = meter_report._price_env()
    # above the threshold the long-context rates apply to the whole request
    usd, long = meter_report.request_usd(
        {"prompt_tokens": 250_000, "cache_write_tokens": 0, "cached_tokens": 0,
         "completion_tokens": 1_000_000}, prices)
    assert long
    assert usd == pytest.approx(250_000 * 6 / 1e6 + 22.5)

    # no prices, no cost
    plug_env.delenv("ANTHROPIC_PRICE_INPUT_PER_MTOK")
    assert meter_report.request_usd({"prompt_tokens": 1}, meter_report._price_env()) == (None, False)
