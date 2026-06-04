"""Transport ABC — the seam between the two runner bundles.

local_runner and ssh_runner share seven lifecycle verbs (reflect / boot /
start / stop / restart / status / get_webapp) and the WS health probe.
They differ ONLY in *transport*: local drives a `fantastic` subprocess via
the filesystem (`.fantastic/lock.json` on disk, `os.kill`,
`subprocess.Popen`); ssh drives the remote over `ssh` and threads a local
`ssh -L` tunnel.

`Transport` captures exactly that divergent surface. `runner_core.core`
holds the shared verb bodies and calls these methods; each bundle ships a
concrete `Transport` (`LocalTransport`, `SSHTransport`) built PER CALL from
the agent record (`Transport(kernel.get(id))`), so both backends coexist in
one kernel with no module-global provider state.

Async-correct: every method that touches IO is `async` (ssh shells out and
awaits; local's filesystem reads are cheap but stay `async` for a uniform
interface). The poll-loop constants are read through properties so a test
monkeypatching the *bundle module's* `LOCK_POLL_TIMEOUT` etc. is honoured
by core's loops.
"""

from __future__ import annotations

import abc


class Transport(abc.ABC):
    """One project's transport. Built per-call from the agent record."""

    def __init__(self, record: dict | None):
        self.rec: dict = record or {}

    # ─── identity / reflect ──────────────────────────────────────

    @property
    @abc.abstractmethod
    def sentence(self) -> str:
        """One-line self-description for reflect."""

    @abc.abstractmethod
    def reflect_fields(self) -> dict:
        """Transport-specific record fields + live process state to merge
        into the reflect reply (everything except id/sentence/verbs)."""

    # ─── start preconditions ─────────────────────────────────────

    @abc.abstractmethod
    def validate_start(self) -> dict | None:
        """Return an {error: ...} dict if the record can't be started
        (missing fields, bad dir), else None."""

    @abc.abstractmethod
    async def already_running(self) -> dict | None:
        """If the project is already live, return the success dict to
        short-circuit `start` (with an already_* flag); else None."""

    @abc.abstractmethod
    async def bring_up(self) -> dict | None:
        """Spawn the daemon (local: pre-create web record + Popen; ssh:
        compound ssh bootstrap). Return {error: ...} on failure, else None.
        After this returns cleanly, core polls the lock to confirm liveness."""

    @abc.abstractmethod
    async def finish_start(self, pid: int) -> dict:
        """Given the confirmed daemon pid (from the lock), do any
        post-bringup work (local: nothing; ssh: open the tunnel) and return
        the verb's success dict."""

    @abc.abstractmethod
    def start_timeout_error(self) -> dict:
        """The {error: ...} dict returned when lock.json never appears
        within lock_poll_timeout."""

    # ─── stop ────────────────────────────────────────────────────

    @abc.abstractmethod
    async def stop(self) -> dict:
        """Tear the project down (local: SIGTERM→SIGKILL the local pid +
        sweep lock; ssh: kill tunnel + ssh-kill remote pid). Idempotent;
        returns the verb's success dict. Owns the whole stop flow because
        the death semantics diverge too far to share."""

    # ─── live state ──────────────────────────────────────────────

    @abc.abstractmethod
    async def read_lock(self) -> dict | None:
        """Read the project's `.fantastic/lock.json` (local: filesystem;
        ssh: `cat` over ssh). Returns the parsed dict or None."""

    @abc.abstractmethod
    async def pid_alive(self, pid: int) -> bool:
        """Is `pid` alive (local: os.kill 0; ssh: `kill -0` over ssh)?"""

    @abc.abstractmethod
    async def web_port(self) -> int | None:
        """The port the webapp serves on (local: discovered from the web
        agent record on disk; ssh: the configured remote_port)."""

    @abc.abstractmethod
    async def status(self) -> dict:
        """The transport-specific {running.../ws_ok...} status dict. ws_ok
        comes from `runner_core.health._ws_health(self.ws_port)`."""

    @abc.abstractmethod
    async def get_webapp(self, id: str) -> dict:
        """Canvas UI descriptor {url, default_width, default_height, title}
        when live, else {error}."""

    # ─── poll-loop constants (read through the bundle module) ─────

    @property
    @abc.abstractmethod
    def lock_poll_timeout(self) -> float:
        """Seconds to wait for lock.json after bring_up."""

    @property
    @abc.abstractmethod
    def lock_poll_interval(self) -> float:
        """Poll interval while waiting for lock.json."""
