# static_loader — declarative YAML roster

A loader agent that hydrates a **fixed roster** from a `fantastic.yaml` manifest.
Use it when the set of agents should be declared, not composed live.

Init a workspace with it:

```
fantastic init static_loader ./fantastic.yaml
```

That seeds a `static_loader` agent (record field `manifest: ./fantastic.yaml`)
under the root and boots it. On `boot` the loader reads the manifest and creates
each declared agent as a child of itself — kernel bookkeeping: each create is a
direct call of the kernel's own `create_agent`, never a gated send and never
standing power the loader holds. That pass runs only on the kernel's own boot (the
cold boot, or `create_agent` booting the loader it just made): a `boot` an agent
sends is refused (`reason: unauthorized`), so reaching the loader never re-runs
the manifest.

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

Every declared agent gets the baseline grant (`$self`) plus whatever `can_call` /
`callable_by` the manifest names. After boot the roster is **closed**: an agent is
deny-all and creates only when granted `create_agent` on the root, both ends, so the
manifest is the whole roster. Every entry MUST carry an
explicit `id` — boot is idempotent by id (a reboot re-reads the manifest and
RE-APPLIES the declared fields over an existing agent), so the manifest and the
persisted tree never duplicate. An entry with no `id` is refused loud
(`booted: false`): nothing could match it, so each reboot would mint another
agent beside the last one's and persist it.

The kernel knows nothing about YAML; this bundle owns the format — the same way
`kernel_state` owns the on-disk `.fantastic` layout.
