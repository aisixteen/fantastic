"""Readme-contract guard — HOST bundle readmes describe CAPABILITY only.

Part-3 decoupling rule: a host bundle's readme (and its reflect `sentence`)
must NOT name any frontend/client tech.  The host is client-agnostic — it
describes what it DOES and its verb/event surface; the FRONTEND bundle is the
one that declares which host capability it fronts.  An LLM weaves the pairing
from those two self-descriptions.

This is a pure STATIC scan across python/rust/swift sources (no kernel binary
needed) — it always runs and fails on a client-intent word.  Run:
    cd integration_tests && uv run pytest decoupling/test_readme_contract.py
"""

from __future__ import annotations

import re
from pathlib import Path

_REPO = Path(__file__).resolve().parents[2]  # file → decoupling/ → integration_tests/ → repo root

# Client-intent tokens a HOST readme/sentence must not contain (case-insensitive).
DENY = [
    "xterm",
    "iframe",
    "ai_chat_webapp",
    "chat_ui",
    "settings_ui",
    "html_agent",
    "gl_agent",
    "telemetry_pane",
    "terminal_webapp",
    "canvas_webapp",
    "the ui",
    "chat ui",
    "ui state",
    "ui,",
    "browser frame",
    "browser-pastable",
    "browser-bus",
    "broadcastchannel",
    "transport.js",
    "browser",
    "javascript",
]

# Root protocol docs legitimately describe the two-kernel / browser model.
EXEMPT_SUBSTR = [
    "loader/kernel_state/src/kernel_state/readme.md",  # python root
    "fantastic-core/src/readme.md",  # rust root
    "RootReadme",  # swift root readme source
]

# The `web` bundle genuinely serves vendored assets; tolerate the asset
# filenames there (but its prose must still avoid client framing — checked
# by the non-asset tokens).
WEB_ASSET_TOKENS = {"transport.js", "xterm"}


def _is_exempt(path: Path) -> bool:
    s = str(path)
    return any(x in s for x in EXEMPT_SUBSTR)


def _is_web(path: Path) -> bool:
    s = str(path)
    return "/web/" in s or "fantastic-web/" in s or "FantasticWeb" in s


def _readme_sources() -> list[tuple[Path, str]]:
    """Return (path, text) for every host readme.

    Covers python and rust `readme.md` files under their bundle trees, and
    swift inline `var readme: String? { ... }` literal blocks extracted from
    `Fantastic*/**/*.swift` sources.
    """
    out: list[tuple[Path, str]] = []
    for base in [
        _REPO / "src" / "lib" / "python" / "bundled_agents",
        _REPO / "src" / "lib" / "rust" / "crates" / "bundles",
    ]:
        for f in base.glob("**/readme.md"):
            out.append((f, f.read_text(encoding="utf-8", errors="ignore")))
    for f in (_REPO / "src" / "lib" / "swift" / "Sources").glob("Fantastic*/**/*.swift"):
        txt = f.read_text(encoding="utf-8", errors="ignore")
        for m in re.finditer(r"var readme:\s*String\?\s*\{(.*?)\n\s*\}", txt, re.S):
            out.append((f, m.group(1)))
    return out


def _sentence_lines() -> list[tuple[Path, int, str]]:
    """Return (path, lineno, line) for every source line that sets a `sentence`
    literal across python/rust/swift runtimes.

    These are the one-liners an LLM sees from a `reflect` call (without
    readme=true), so they must be equally client-agnostic.  Test fixtures and
    example files are excluded — they are not part of the bundle contract.
    """
    out: list[tuple[Path, int, str]] = []
    globs = [
        (_REPO / "src" / "lib" / "python" / "bundled_agents", "**/tools.py"),
        (_REPO / "src" / "lib" / "rust" / "crates" / "bundles", "**/*.rs"),
        (_REPO / "src" / "lib" / "swift" / "Sources", "Fantastic*/**/*.swift"),
    ]
    _TEST_MARKERS = ("/tests.rs", "/tests/", "/examples/", "test_", "Tests/")
    for base, g in globs:
        for f in base.glob(g):
            if any(t in str(f) for t in _TEST_MARKERS):
                continue  # test/example fixtures aren't the bundle contract
            for i, line in enumerate(
                f.read_text(encoding="utf-8", errors="ignore").splitlines(), 1
            ):
                if "sentence" in line.lower() and ('"' in line or "'" in line):
                    out.append((f, i, line))
    return out


def _leaks_in(text: str, *, web: bool) -> list[str]:
    low = text.lower()
    found = []
    for word in DENY:
        if word in low:
            if web and word in WEB_ASSET_TOKENS:
                continue
            found.append(word)
    return found


def test_host_readmes_are_client_agnostic():
    hits: list[str] = []
    for path, text in _readme_sources():
        if _is_exempt(path):
            continue
        for word in _leaks_in(text, web=_is_web(path)):
            hits.append(f"{path.relative_to(_REPO)} :: readme :: {word!r}")
    assert not hits, "host readme client-intent leaks:\n" + "\n".join(
        f"  {h}" for h in sorted(set(hits))
    )


def test_reflect_sentences_are_client_agnostic():
    hits: list[str] = []
    for path, lineno, line in _sentence_lines():
        if _is_exempt(path):
            continue
        for word in _leaks_in(line, web=_is_web(path)):
            hits.append(f"{path.relative_to(_REPO)}:{lineno} :: sentence :: {word!r}")
    assert not hits, "reflect sentence client-intent leaks:\n" + "\n".join(
        f"  {h}" for h in sorted(set(hits))
    )
