"""_sys — the OS boundary of the substrate.

Every call the kernel package makes into the operating system that differs by OS
family sits behind a small named function here: the workdir lock's file lock
(`_lock.py`; rust `fantastic_kernel::sys`), the daemon's stop signals
(`modes/longrun.py`), the encoding of the process's own stdout/stderr (`main.py`),
and the shape an `OSError` is printed in when it reaches the wire (`posix_error`:
the kernel's raised-reply text; `posix_errors`: `file_bridge.fs`, the one disk
surface). Callers see only these OS-neutral names; one module per OS family holds
the calls, picked ONCE, here.
"""

from __future__ import annotations

import os

if os.name == "nt":
    from kernel._sys.nt import *  # noqa: F403
else:
    from kernel._sys.posix import *  # noqa: F403
