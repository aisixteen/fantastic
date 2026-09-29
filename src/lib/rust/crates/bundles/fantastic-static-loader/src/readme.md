# static_loader — declarative YAML roster

A loader agent that hydrates a **fixed roster** from a `fantastic.yaml` manifest.
Use it when the set of agents should be declared, not composed live.

Init a workspace with it:

```
fantastic init static_loader ./fantastic.yaml
```

That seeds a `static_loader` agent (record field `manifest: ./fantastic.yaml`)
under the root and boots it. On `boot` the loader reads the manifest and creates
each declared agent as a child of itself — under `operator_context()`, so the
hydration is the operator's authority at boot, not standing power the loader
holds.

Manifest:

```yaml
agents:
  - id: memory
    handler_module: file_bridge.tools
  - id: worker
    handler_module: exec_bridge.tools
    allowed: ['python3 .*']   # the command(s) this leg may spawn
    can_call:            # deny-all: name the lateral edges this agent may use
      memory: [set, read]
```

Every declared agent gets the Unix baseline grant
(`$self`/`$parent`/`$children`/`$loader`) plus whatever `can_call` the manifest
names. After boot the roster is **closed**: an agent is deny-all and cannot
`create_agent`, so the manifest is the whole roster. Every entry MUST carry an
explicit `id` — boot is idempotent by id (a reboot re-reads the manifest and
RE-APPLIES the declared fields over an existing agent), so the manifest and the
persisted tree never duplicate. An entry with no `id` is refused loud
(`booted: false`): nothing could match it, so each reboot would mint another
agent beside the last one's and persist it.

The kernel knows nothing about YAML; this bundle owns the format — the same way
`kernel_state` owns the on-disk `.fantastic` layout.
