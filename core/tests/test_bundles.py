"""Tests for core.bundles — BundleStore."""

import json
from pathlib import Path

import pytest
from core.bundles import BundleStore


@pytest.fixture
def bundles_dir(tmp_path):
    """Create a tmp directory with two mock bundles."""
    (tmp_path / "alpha").mkdir()
    (tmp_path / "alpha" / "template.json").write_text(
        json.dumps(
            {
                "bundle": "alpha",
                "parameters": {"default_width": 400, "default_height": 300},
            }
        )
    )
    (tmp_path / "alpha" / "source.py").write_text("print('alpha')\n")

    (tmp_path / "beta").mkdir()
    (tmp_path / "beta" / "template.json").write_text(
        json.dumps(
            {
                "bundle": "beta",
                "parameters": {"default_width": 600, "default_height": 400},
            }
        )
    )
    return tmp_path


# ── list_bundles ─────────────────────────────────────────────────


def test_list_bundles_empty(tmp_path):
    store = BundleStore(tmp_path)
    assert store.list_bundles() == []


def test_list_bundles(bundles_dir):
    store = BundleStore(bundles_dir)
    names = [b["bundle"] for b in store.list_bundles()]
    assert names == ["alpha", "beta"]


def test_list_bundles_skips_no_template(tmp_path):
    (tmp_path / "no_tmpl").mkdir()
    store = BundleStore(tmp_path)
    assert store.list_bundles() == []


def test_list_bundles_skips_invalid_json(tmp_path):
    (tmp_path / "bad").mkdir()
    (tmp_path / "bad" / "template.json").write_text("{invalid json")
    store = BundleStore(tmp_path)
    assert store.list_bundles() == []


# ── get_bundle ───────────────────────────────────────────────────


def test_get_bundle(bundles_dir):
    store = BundleStore(bundles_dir)
    result = store.get_bundle("alpha")
    assert result["bundle"] == "alpha"
    assert result["parameters"]["default_width"] == 400


def test_get_bundle_not_found(bundles_dir):
    store = BundleStore(bundles_dir)
    assert store.get_bundle("nonexistent") is None


# ── apply_bundle ─────────────────────────────────────────────────


def test_apply_bundle(bundles_dir, tmp_path):
    store = BundleStore(bundles_dir)
    agent_dir = tmp_path / "agent_out"
    agent_dir.mkdir()
    result = store.apply_bundle("alpha", agent_dir)
    assert result["bundle"] == "alpha"
    assert result["width"] == 400
    assert result["height"] == 300
    assert (agent_dir / "source.py").read_text() == "print('alpha')\n"


def test_apply_bundle_no_source(bundles_dir, tmp_path):
    store = BundleStore(bundles_dir)
    agent_dir = tmp_path / "agent_out"
    agent_dir.mkdir()
    # beta has no source.py
    result = store.apply_bundle("beta", agent_dir)
    assert result["bundle"] == "beta"
    assert not (agent_dir / "source.py").exists()


def test_apply_bundle_not_found(bundles_dir, tmp_path):
    store = BundleStore(bundles_dir)
    with pytest.raises(ValueError, match="not found"):
        store.apply_bundle("nonexistent", tmp_path)


def test_apply_bundle_uses_template_defaults(bundles_dir, tmp_path):
    store = BundleStore(bundles_dir)
    agent_dir = tmp_path / "agent_out"
    agent_dir.mkdir()
    result = store.apply_bundle("alpha", agent_dir)
    assert result["width"] == 400
    assert result["height"] == 300


# ── edge cases ───────────────────────────────────────────────────


def test_nonexistent_dir():
    store = BundleStore(Path("/nonexistent/dir"))
    assert store.list_bundles() == []
