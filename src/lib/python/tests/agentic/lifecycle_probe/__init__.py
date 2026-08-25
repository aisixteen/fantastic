"""A test bundle, on disk so a manifest can name it from any pytest invocation.

`configs/lifecycle.yaml` declares an agent whose `handler_module` is
`lifecycle_probe.tools`. That name has to resolve through `importlib` in TWO
places: `Agent._dispatch`, and `Kernel.load` when the persisted record is read
back at reboot. Registering the module into `sys.modules` from a validator makes
the first work and the second depend on WHICH test file pytest happened to
import — measured: `pytest tests/agentic/test_manifest_contract.py` alone made
PC-4 fail for this config only, because `Kernel.load` dropped the record
(`[kernel] skipping agent slowbot: bundle lifecycle_probe.tools not installed in
this runtime`) and the loader re-CREATED it on the reboot pass.

`tests/agentic/` is already on `sys.path` (pytest prepends a conftest's own
directory, which is how `_engine` imports resolve), so a real package here is
importable for every entry point into the suite and couples nothing to
collection order.
"""
