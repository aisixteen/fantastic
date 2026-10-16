"""_sys — the OS boundary of `fs`, the one disk surface.

What `fs` does that differs by OS family: creating a file that never goes through a
link planted at its name (and is private from its first byte when its mode says
so — `create_new`), judging a PRIVATE file — the `0600` contract `read_private`
keeps — on the one open it is read from (`open_private`), telling a link from what
it points at (`is_link`: on Windows a junction too), and which wire paths are
absolute (`is_absolute`: on Windows a drive or a UNC share too). Plus the fixtures
the tests set and read the private-file contract with (`set_mode`, `mode_of`,
`own_uid`; rust `fantastic_file::test_support`). One module per OS family, picked
ONCE, here; `fs` sees only these names.
"""

from __future__ import annotations

import os
from typing import NamedTuple


class Private(NamedTuple):
    """What `open_private` found: `fd` open on a private regular file (the
    caller reads and closes it), or `None` with nothing left open — and either
    way the entry's permission bits and owner, for the refusal text."""

    fd: int | None
    mode: int
    uid: int


if os.name == "nt":
    from file_bridge._sys.nt import *  # noqa: E402, F403
else:
    from file_bridge._sys.posix import *  # noqa: E402, F403
