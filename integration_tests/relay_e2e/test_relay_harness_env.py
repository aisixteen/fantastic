"""The relay harness finds our relay (`relayd`) through the `RELAYD` env var only."""

from __future__ import annotations

import pytest

import relay_harness as rh


def test_the_skip_message_names_the_relayd_env_var(monkeypatch):
    monkeypatch.delenv("RELAYD", raising=False)
    monkeypatch.setattr(rh, "_TARGET", "local")
    with pytest.raises(pytest.skip.Exception) as e:
        rh.require_relay()
    assert "RELAYD" in str(e.value), e.value


def test_relayd_comes_from_the_env_var(monkeypatch, tmp_path):
    binary = tmp_path / "relayd"
    binary.write_text("#!/bin/sh\n")
    binary.chmod(0o755)
    monkeypatch.setenv("RELAYD", str(binary))
    assert rh._relayd() == binary
    monkeypatch.setenv("RELAYD", str(tmp_path / "missing"))
    assert rh._relayd() is None
    src = open(rh.__file__).read()
    assert "fantastic_relay" not in src and "swift build" not in src
