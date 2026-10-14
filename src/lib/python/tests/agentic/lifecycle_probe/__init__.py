"""A test bundle, on disk so a manifest can name it from any pytest invocation.

`configs/lifecycle.yaml` declares an agent whose `handler_module` is
`lifecycle_probe.tools`. That name has to resolve in TWO places: `Agent._dispatch`,
and `Kernel.load` when the persisted record is read back at reboot. The kernel
resolves a handler_module only from the bundle catalog plus `register_unlisted`,
so `tests/agentic/conftest.py` registers this module unlisted — the one entry every
invocation of the suite passes through, so resolution never depends on WHICH test
file pytest happened to import (a validator-time registration would leave a single
test file run alone with `[kernel] skipping agent slowbot: bundle
lifecycle_probe.tools not installed in this runtime`, and the loader would re-CREATE
it on the reboot pass).

`tests/agentic/` is already on `sys.path` (pytest prepends a conftest's own
directory, which is how `_engine` imports resolve), so the conftest imports this
package like any other.
"""
