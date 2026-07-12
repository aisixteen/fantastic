> Wire a `file_bridge` provider to make the tree durable; no provider ⇒ RAM.

# Durable state: wire a persistence provider (or stay in RAM)

**Persistence is wired, not automatic.** The tree lives in RAM until the
root has somewhere to write: it auto-persists records (and every agent's
sidecars) ONLY through a `file_bridge` provider it DISCOVERS — the first
`file_bridge` child of the root whose `root` resolves to `.fantastic`. NO
provider ⇒ no persistence, lost on exit; there is no direct-write fallback.
So wire the store FIRST, before anything worth keeping:

    create_agent handler_module=file_bridge.tools id=store root=.fantastic ingress_rule=allow_all

The `file_bridge` edge is SEALED by default — `ingress_rule=allow_all` opens
it; `reflect` then shows `persistence: {provider: "store"}`. One store
serves both the root's records AND every agent's sidecar: the sidecar lands
at `agents/<id>/state.yaml`, next to that agent's own `agent.json`
(store-relative — no `.fantastic/.fantastic/…` nesting).

The root's `reflect` ALSO carries a stable **`guid`** — the kernel's own
instance identity, minted ONCE at boot and persisted into the root record
(`.fantastic/agent.json`). It is PERSISTENCE-scoped: it round-trips a reboot
verbatim (an explicit `guid` always wins, a minted one is never regenerated)
— unlike the run-scoped `env`/`version`, which are read inline each reflect.
A relay peer advertises this value as its `owner_guid` (distinct from a relay
LEG's own `guid` on a `relay_connector` record).
