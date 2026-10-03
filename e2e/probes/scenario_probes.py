"""THE SCENARIO PROBES, registered — python's `--features scenario-probes`.

The python kernel resolves a `handler_module` only from its bundle catalog plus
`register_unlisted` (rust's `BundleRegistry::register_unlisted`, swift's
`registerUnlisted`), so a probe on `sys.path` is not enough: it must be
REGISTERED, unlisted, in the kernel's own process. The same six rust registers
behind its feature.

  * in process — `register()` (`helpers/kernel.py`, and a probe that boots its
    own kernel);
  * spawned — run THIS file instead of `main.py` (`helpers/host/driver.py`,
    `parity/spec.py`): it registers the probes, then hands argv to `main.py`'s
    own entry. Run as a script, its directory (`e2e/probes`) is `sys.path[0]`,
    which is how the probe packages import. A production kernel never runs it.
"""

from __future__ import annotations

import dispenser_probe.tools
import oracle_probe.tools
import plant_probe.tools
import purchase_probe.tools
import slow_probe.tools
import user_probe.tools

from kernel import register_unlisted

PROBES = (
    dispenser_probe.tools,
    plant_probe.tools,
    purchase_probe.tools,
    user_probe.tools,
    oracle_probe.tools,
    slow_probe.tools,
)


def register() -> None:
    """Register every probe UNLISTED under its module name: it dispatches and
    reloads, and never appears in the `bundles` catalog."""
    for mod in PROBES:
        register_unlisted(mod.__name__, mod)


if __name__ == "__main__":
    register()
    from main import main_dispatch

    main_dispatch()
