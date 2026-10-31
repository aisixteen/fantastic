"""THE SWIFT KERNEL RUNTIME IS RETIRED (#53): a guard so the cleanup holds.

The host runtimes are python and rust, plus the ts browser kernel. The only
swift left in the repo is the Foundation Models bridge, a dependency-free
package inside the rust `fantastic-foundation-models-backend` crate that its
`build.rs` builds on macOS. These tests read the tracked files as text and
fail when docs, comments or harness prose describe a swift kernel again.
"""

from __future__ import annotations

import re
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

ROOT = next(
    p for p in Path(__file__).resolve().parents if (p / "e2e" / ".e2e-root").exists()
)
E2E = ROOT / "e2e"
THIS = Path(__file__).resolve().relative_to(ROOT).as_posix()

#: Paths whose swift mentions are kept on purpose: history, the dated design
#: plans, and every home of the Foundation Models bridge.
ALLOWED_PATHS = (
    "CHANGELOG.md",
    "docs/io_core_spec.md",
    "docs/io_bridge_blueprint.md",
    "src/lib/rust/crates/bundles/fantastic-foundation-models-backend/",
    "src/lib/rust/README.md",
    "src/lib/rust/selftest/foundation_models_backend.md",
    "src/lib/rust/.cargo/config.toml",
    "src/.cargo/config.toml",
    "src/tui/Cargo.toml",
    "src/lib/rust/crates/fantastic-cli/",
    ".github/workflows/build.yml",
    "integration_tests/emergence/",  # dated run reports, kept as history
    # contract sentence the Swift app relies on; canvas web UI text; a lockfile
    "src/tui/crates/fantastic-home-kernel/docs/home.md",
    "src/tui/web/canvas/readme.md",
    "src/app/Cargo.lock",
    "packaging/flatpak/cargo-sources.json",  # generated vendored crate list (swift-rs)
    THIS,
)

#: Line patterns that name swift without naming a swift kernel.
ALLOWED_LINES = re.compile(
    r"swiftshader"  # chrome's software GL flag
    r"|\|rs\|swift\|"  # the fold strategy's file-extension regex
    r"|relayd"  # the relay kernel, built in its own repo
    r"|Foundation Models|FMCore|FantasticFMBridge",
    re.I,
)

SWIFT = re.compile(r"\bswift\b", re.I)


def _tracked() -> list[str]:
    if shutil.which("git") is None or not (ROOT / ".git").exists():
        pytest.skip("not a git checkout")
    out = subprocess.run(
        ["git", "ls-files", "-z"], cwd=ROOT, capture_output=True, check=True
    )
    return [p for p in out.stdout.decode().split("\0") if p]


def _read(rel: str) -> str:
    return (ROOT / rel).read_text(encoding="utf-8")


def _offenders() -> list[str]:
    hits = []
    for rel in _tracked():
        if rel.startswith(ALLOWED_PATHS):
            continue
        path = ROOT / rel
        if not path.is_file():
            continue
        try:
            text = path.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        for n, line in enumerate(text.splitlines(), 1):
            if SWIFT.search(line) and not ALLOWED_LINES.search(line):
                hits.append(f"{rel}:{n}: {line.strip()[:120]}")
    return hits


def test_no_stale_swift_runtime_mentions_outside_the_allowlist():
    hits = _offenders()
    assert hits == [], (
        f"{len(hits)} lines still describe a swift kernel runtime:\n" + "\n".join(hits)
    )


def test_docs_name_two_host_runtimes():
    contributing = _read("CONTRIBUTING.md")
    assert "two host runtimes" in contributing
    assert not any(line.startswith("| **swift**") for line in contributing.splitlines())
    for rel in ("CITATION.cff", "NOTICE", "SECURITY.md"):
        assert "swift" not in _read(rel).lower(), rel


def test_changelog_unreleased_records_the_removal():
    text = _read("CHANGELOG.md")
    start = text.index("## [Unreleased]")
    end = text.find("\n## [", start + 1)
    unreleased = text[start : end if end != -1 else len(text)]
    assert "### Removed" in unreleased, "no ### Removed in [Unreleased]"
    removed = unreleased.split("### Removed", 1)[1].split("\n### ", 1)[0]
    assert "Swift kernel runtime" in removed
    assert "Foundation Models" in removed


def test_runtime_enum_doc_lists_no_swift():
    lines = [
        line
        for line in _read("src/lib/python/CLAUDE.md").splitlines()
        if "`python` |" in line
    ]
    assert lines, "the runtime enum line is gone"
    for line in lines:
        assert "`swift`" not in line, line
        assert "`python` | `rust` | `ts`" in line, line


def test_parity_readme_has_no_swift_commands():
    text = _read("e2e/parity/README.md")
    assert "--runtime swift" not in text
    assert "/swift" not in text


def test_harness_refuses_swift_runtime(monkeypatch):
    monkeypatch.syspath_prepend(str(E2E))
    sys.modules.pop("run", None)
    import run

    assert run.HOST_RUNTIMES == ("python", "rust")
    assert "swift" not in run.CWDS
    assert "swift" not in run.SCENARIO_RUNNERS
