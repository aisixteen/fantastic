"""_sys — the OS boundary of exec_bridge (rust `fantastic_exec_bridge::sys`).

Every OS call `proc` (the one process surface) makes lives here, one module per OS
family, picked ONCE, here:

- **the control channel** — `ctrl_pair`: a connected stream-socket pair, the
  child end inherited by exactly one job;
- **a job** — `spawn_argv` (piped) / `spawn_pty` (on a `Pty`) start one inside a
  CONTAINER — a session on POSIX, a Job Object on Windows — so `kill_session`
  takes it and everything it launched; `interrupt` is its Ctrl-C;
- **a terminal** — `open_pty` answers a `Pty`: the kernel's end, read from the
  event loop (`start_reading` / `stop_reading`), written in full (`write`),
  resized (`resize`), closed (`close`);
- **the PATH grammar** — `names_a_file`, `child_path`, `exe_names`,
  `is_executable`: the OS's halves of `proc.resolve`;
- **the child's own shell** — `SHELL_VAR`, the child env var that names it
  (`SHELL`; on Windows `COMSPEC`), and `LOGIN_ARGS`, what makes it a login shell
  (`-l`; nothing on Windows — `cmd.exe` has no login mode);
- the fixtures the tests read a job with — `session_members`, `pid_alive`,
  `popen_inheriting` (a bare program inheriting a control socket) — and
  `OS_ENV`, the variables a child needs from the OS just to start (none on
  POSIX; `SYSTEMROOT` on Windows, without which Winsock cannot start in the child,
  so its connector has no socket). `_child_env` passes them down to EVERY job,
  whatever `inherit_env` says (rust `sys::OS_ENV`).
"""

from __future__ import annotations

import os

if os.name == "nt":
    from exec_bridge._sys.nt import *  # noqa: F403
else:
    from exec_bridge._sys.posix import *  # noqa: F403
