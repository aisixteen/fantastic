# Scenario probes — python

The stateful fixture roles the shared scenarios name (`dispenser`,
`flaky_plant`, `purchase_desk`, `user`, the oracle and the slow worker), as
REAL bundles resolvable by `handler_module`.

They live here, not in `src/lib/python`, because they are TEST-ONLY. The
python kernel resolves a `handler_module` only from its bundle catalog plus
`register_unlisted` (rust's `BundleRegistry::register_unlisted`), so a module
on `sys.path` is never enough: `scenario_probes.py` REGISTERS them, unlisted —
`register()` in process (`helpers/kernel.py`), or run as the spawned kernel's
entry in place of `main.py` (`helpers/host/driver.py`, `parity/spec.py`). A
production kernel never runs it, so the role simply cannot be played — the same
gate rust gets from `--features scenario-probes` and swift from
`-DSCENARIO_PROBES`.

Module names are shared with the compiled runtimes on purpose: one
`ROLE_BUNDLE` map in `runners/host.py` serves every runtime.
