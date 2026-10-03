"""CLI help renderer (reads cli/help.md)."""

from __future__ import annotations


def _print_help() -> None:
    """Print the CLI cheatsheet — `cli/help.md`. It lives in the `cli`
    bundle because the CLI is what renders it: file-backed, editable
    markdown, not a hardcoded string. Points at `fantastic reflect
    readme=true` for the live-system bootstrap. An unreadable cheatsheet
    raises: a substitute line hid a broken install (audit 2026-10-03,
    ARCH2-9)."""
    import importlib.resources

    src = importlib.resources.files("cli") / "help.md"
    print(src.read_text(encoding="utf-8").rstrip())
