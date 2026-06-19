"""Public API for the fantastic kernel.

The substrate has two types:
  - `Agent` — recursive node in the kernel tree. Every entity is an
    Agent (or an Agent subclass like `Cli`). Some have children
    (populated), some don't (leaves).
  - `Kernel` — tree-wide shared context (flat agents index, state
    subscribers, bundle resolver cache). NOT an agent.

Composition is explicit and lives in `main.py`: the ROOT is an
`kernel_state` agent (`id="kernel_state"`) that owns `.fantastic/`:

    kernel = Kernel()
    kernel.load(read_tree(".fantastic"))   # root = kernel_state at id="kernel_state"
    Cli(kernel, parent=kernel.root)         # stdout renderer (only if tty)

The substrate (`kernel/`) knows nothing about specific bundles. Any
class with a `handler(id, payload, agent)` callable in its declared
`handler_module` plugs in. External code (bundles, tests, conftest)
imports from this package.
"""

from __future__ import annotations

from kernel._agent import Agent
from kernel._bundles import _find_bundle_module
from kernel._env import _load_dotenv
from kernel.modes import dispatch_argv
from kernel._kernel import (
    BUNDLE_ENTRY_GROUP,
    INBOX_BOUND,
    Kernel,
    _current_sender,
    _summarize_payload,
    sender_context,
)
from kernel._state import CURRENT_VERSION
from kernel._lock import (
    LOCK_FILE,
    FantasticLock,
    _pid_alive,
    _read_lock,
    acquire_lock,
    release_lock,
)

__all__ = [
    # Substrate types
    "Agent",
    "Kernel",
    # Constants
    "INBOX_BOUND",
    "BUNDLE_ENTRY_GROUP",
    "CURRENT_VERSION",
    "sender_context",
    "_current_sender",
    "_summarize_payload",
    # Lock
    "LOCK_FILE",
    "FantasticLock",
    "acquire_lock",
    "_pid_alive",
    "_read_lock",
    "release_lock",
    # Env
    "_load_dotenv",
    # Bundles
    "_find_bundle_module",
    # CLI modes
    "dispatch_argv",
]
