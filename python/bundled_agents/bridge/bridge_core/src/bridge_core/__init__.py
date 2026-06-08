"""bridge_core — shared engine for the kernel-bridge family.

`from bridge_core import core` for the engine + `make_verbs`/`dispatch`/`on_delete`;
the transport contract + in-memory test transport are re-exported here for convenience.
"""

from bridge_core._transport import ConnectionClosed, MemoryTransport, _BaseTransport

__all__ = ["ConnectionClosed", "MemoryTransport", "_BaseTransport"]
