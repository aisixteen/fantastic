> Wire a `file_bridge` provider to make the tree durable; no provider ⇒ RAM.

# Durable state: wire a persistence provider (or stay in RAM)

The root (`kernel_state`) auto-persists records ONLY through a stream
provider it **discovers**: the first `file_bridge` child of the root whose
`root` resolves to `.fantastic`. **No provider ⇒ no persistence — the tree
lives in RAM** (lost on restart). There is no direct-write fallback (this is
a kernel, not a SaaS — one path, no hidden second route). Wire one to make
the kernel durable:

    fantastic kernel create_agent handler_module=file_bridge.tools \
        id=store root=.fantastic ingress_rule=allow_all   # the persistence sink

Once it exists, every record auto-persists through its `write_stream`/`delete`
(readme-seeding too). It's a normal, visible, re-gateable agent — reflect it
like any other. Point it at a `network_bridge` instead and state persists to
a remote store. (`allow_all` lets the loader's own writes through; **seal it
and persistence stops** — that's your choice, with no fallback to soften it.)

**You don't have to guess the wiring — read it.** The ROOT reflect carries
`persistence: {provider:<id>|null}` (which store is persisting records, or
`null` = RAM); each tree node carries its posture inline (`ingress_rule` —
absent on an io leg ⇒ sealed; `root`; `file_bridge_id`). One `reflect
tree=all` shows every leg's lock + the live provider. And **one store serves
both**: bundles that need a sidecar (`yaml_state`, `scheduler`, the ai
backends) point their `file_bridge_id` at this same `.fantastic` store and
write store-relative `agents/<id>/<file>`, so their state lands next to their
own `agent.json` — one bridge, records + sidecars together.

The root's `reflect` ALSO carries a stable **`guid`** — the kernel's own
instance identity, minted ONCE at boot and persisted into the root record
(`.fantastic/agent.json`). It is PERSISTENCE-scoped: it round-trips a reboot
verbatim (an explicit `guid` always wins, a minted one is never regenerated)
— unlike the run-scoped `env`/`version`, which are read inline each reflect.
A relay peer advertises this value as its `owner_guid` (distinct from a relay
LEG's own `guid` on a `relay_connector` record).
