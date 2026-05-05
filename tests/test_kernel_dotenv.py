"""kernel._load_dotenv — stdlib .env loader.

Semantics: KEY=value lines into os.environ, shell wins over file
(existing entries are never overwritten), `export ` prefix tolerated,
matching surrounding quotes stripped, comments and blank lines skipped."""

from __future__ import annotations

import os

from kernel import _load_dotenv


def test_load_missing_file_is_noop(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    assert _load_dotenv() == 0


def test_load_basic_key_value_pairs(tmp_path, monkeypatch):
    (tmp_path / ".env").write_text("FOO=bar\nBAZ=qux\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FOO", raising=False)
    monkeypatch.delenv("BAZ", raising=False)
    assert _load_dotenv() == 2
    assert os.environ["FOO"] == "bar"
    assert os.environ["BAZ"] == "qux"


def test_existing_env_wins_over_file(tmp_path, monkeypatch):
    """The shell's value is authoritative — file MUST NOT clobber it."""
    (tmp_path / ".env").write_text("FOO=from-file\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("FOO", "from-shell")
    n = _load_dotenv()
    assert n == 0  # nothing actually set
    assert os.environ["FOO"] == "from-shell"


def test_skips_blank_lines_and_comments(tmp_path, monkeypatch):
    (tmp_path / ".env").write_text("\n# a comment\n   \nFOO=bar\n# another\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FOO", raising=False)
    assert _load_dotenv() == 1
    assert os.environ["FOO"] == "bar"


def test_strips_matching_surrounding_quotes(tmp_path, monkeypatch):
    (tmp_path / ".env").write_text(
        'A="quoted value"\n'
        "B='single quoted'\n"
        "C=unquoted\n"
        "D=\"mixed'\n"  # mismatched — leave as-is
    )
    monkeypatch.chdir(tmp_path)
    for k in ("A", "B", "C", "D"):
        monkeypatch.delenv(k, raising=False)
    _load_dotenv()
    assert os.environ["A"] == "quoted value"
    assert os.environ["B"] == "single quoted"
    assert os.environ["C"] == "unquoted"
    assert os.environ["D"] == "\"mixed'"


def test_export_prefix_tolerated(tmp_path, monkeypatch):
    """The same file should source cleanly in bash AND load here."""
    (tmp_path / ".env").write_text("export FOO=bar\nexport  BAZ=qux\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FOO", raising=False)
    monkeypatch.delenv("BAZ", raising=False)
    _load_dotenv()
    assert os.environ["FOO"] == "bar"
    assert os.environ["BAZ"] == "qux"


def test_skips_lines_without_equals(tmp_path, monkeypatch):
    (tmp_path / ".env").write_text("garbage line no equals\nFOO=bar\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FOO", raising=False)
    assert _load_dotenv() == 1
    assert os.environ["FOO"] == "bar"


def test_skips_empty_key(tmp_path, monkeypatch):
    (tmp_path / ".env").write_text("=value\nFOO=bar\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("FOO", raising=False)
    assert _load_dotenv() == 1


def test_value_with_equals_signs_preserved(tmp_path, monkeypatch):
    """Token after first `=` is the value (URLs, base64, etc.)."""
    (tmp_path / ".env").write_text(
        "DATABASE_URL=postgres://u:p@host/db?sslmode=require\n"
    )
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("DATABASE_URL", raising=False)
    _load_dotenv()
    assert os.environ["DATABASE_URL"] == "postgres://u:p@host/db?sslmode=require"


def test_nvapi_key_roundtrip(tmp_path, monkeypatch):
    """Smoke test for the actual use case: NVAPI_KEY in .env."""
    (tmp_path / ".env").write_text("NVAPI_KEY=nvapi-secret-xxxxxxxxxxxx\n")
    monkeypatch.chdir(tmp_path)
    monkeypatch.delenv("NVAPI_KEY", raising=False)
    _load_dotenv()
    assert os.environ["NVAPI_KEY"] == "nvapi-secret-xxxxxxxxxxxx"
