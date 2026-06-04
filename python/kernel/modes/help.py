"""CLI help renderer (reads cli/help.md)."""

from __future__ import annotations


def _print_help() -> None:
    """Print the CLI cheatsheet — `cli/help.md`. It lives in the `cli`
    bundle because the CLI is what renders it: file-backed, editable
    markdown, not a hardcoded string. Points at `fantastic reflect
    return_readme=true` for the live-system bootstrap."""
    import importlib.resources

    try:
        src = importlib.resources.files("cli") / "help.md"
        print(src.read_text(encoding="utf-8").rstrip())
    except (ModuleNotFoundError, FileNotFoundError, OSError, TypeError):
        print("fantastic — run `fantastic reflect return_readme=true` to bootstrap.")
