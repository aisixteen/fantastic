# Scenario probes — python

The stateful fixture roles the shared scenarios name (`dispenser`,
`flaky_plant`, `purchase_desk`), as REAL bundles resolvable by
`handler_module`.

They live here, not in `src/lib/python`, because they are TEST-ONLY. A
production python kernel never has this directory on `PYTHONPATH`, so
`import dispenser_probe.tools` fails and the role simply cannot be played
— the same gate rust gets from `--features scenario-probes` and swift
from `-DSCENARIO_PROBES`, expressed in the mechanism python actually has.

Module names are shared with the compiled runtimes on purpose: one
`ROLE_BUNDLE` map in `runners/host.py` serves every runtime.
