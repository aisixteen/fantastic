"""Public API for the kernel package. `kernel.py` at repo root is the
match/case CLI router; everything else lives in this package."""

from __future__ import annotations

from kernel._bundles import _find_bundle_module, _seed_singletons
from kernel._call import cmd_call, cmd_reflect
from kernel._env import _load_dotenv
from kernel._install import cmd_install, cmd_install_bundle
from kernel._kernel import (
    AGENTS_DIR,
    BUNDLE_ENTRY_GROUP,
    FANTASTIC_DIR,
    INBOX_BOUND,
    Kernel,
    _current_sender,
    _summarize_payload,
)
from kernel._lock import (
    LOCK_FILE,
    _pid_alive,
    _read_lock,
    _release_serve_lock,
    acquire_serve_lock,
)
from kernel._repl import _coerce, _parse_at, _print_result, _read_line, cmd_repl
from kernel._serve import cmd_serve

__all__ = [
    # Core
    "Kernel",
    "INBOX_BOUND",
    "FANTASTIC_DIR",
    "AGENTS_DIR",
    "BUNDLE_ENTRY_GROUP",
    "_current_sender",
    "_summarize_payload",
    # Lock
    "LOCK_FILE",
    "acquire_serve_lock",
    "_pid_alive",
    "_read_lock",
    "_release_serve_lock",
    # Env
    "_load_dotenv",
    # Bundles
    "_find_bundle_module",
    "_seed_singletons",
    # CLI handlers
    "cmd_repl",
    "cmd_serve",
    "cmd_call",
    "cmd_reflect",
    "cmd_install",
    "cmd_install_bundle",
    # REPL helpers
    "_coerce",
    "_parse_at",
    "_print_result",
    "_read_line",
]
