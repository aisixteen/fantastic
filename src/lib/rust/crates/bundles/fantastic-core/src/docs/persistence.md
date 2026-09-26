> Wire a `file_bridge` provider to make the tree durable; no provider ⇒ RAM.

# Durable state: wire a persistence provider (or stay in RAM)

The root (`kernel_state`) auto-persists records ONLY through a stream
provider it **discovers**: the first `file_bridge` child of the root whose
`root` resolves to `.fantastic`. **No provider ⇒ no persistence — the tree
lives in RAM** (lost on restart). There is no direct-write fallback (this is
a kernel, not a SaaS — one path, no hidden second route). Wire one to make
the kernel durable:

    fantastic kernel create_agent parent_id=kernel_state handler_module=file_bridge.tools \
        id=store root=.fantastic ingress_rule=allow_all_ingress   # the persistence sink

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
both**: bundles that need a sidecar (`text_file`, `scheduler`, the ai
backends) point their `file_bridge_id` at this same `.fantastic` store and
write store-relative `agents/<id>/<file>`, so their state lands next to their
own `agent.yaml` — one bridge, records + sidecars together.

The root's `reflect` ALSO carries a stable **`guid`** — the kernel's own
instance identity, minted ONCE at boot and persisted into the root record
(`.fantastic/agent.yaml`). It is PERSISTENCE-scoped: it round-trips a reboot
verbatim (an explicit `guid` always wins, a minted one is never regenerated)
— unlike the run-scoped `env`/`version`, which are read inline each reflect.
A relay peer advertises this value as its `owner_guid` (distinct from a relay
LEG's own `guid` on a `relay_connector` record).

## Messages live in MEMORY STORES — the loader keeps records only

The kernel keeps no conversation and writes no message log. Every message lives in a
MEMORY STORE agent (`ram_store` in this lib: in RAM, the topology of a SQL store —
rows, fold refs, grants with a cascade, a text index; a persistent store is the same
verbs over a real database). A store is multi-tenant: every row sits under an OWNER
key `<kernel guid>|<agent id>` (the operator is `kernel`; a caller from another kernel
arrives already qualified with its own guid).

- **The root names its store.** `memory_store_id` on the root record is MANDATORY for
  a started kernel: `kernel_start` refuses without it — create a `ram_store` and name it
  there if you have none. The start sequence: root → the store → the flusher → every
  other agent in registration order. A one-shot `fantastic <id> <verb>` runs an
  UNSTARTED kernel, so an admin can create the store before the first start.
- **Executions** — a `call` row per dispatch (payload verbatim), a `settle` row per
  ending (a NAMED call's whole result, a dead letter's error, else `{status, bytes}`
  — the kernel's own store reads are executions too, so a body here would re-record
  the store inside the store), `task`, `ack` — are minted in RAM and FLUSHED into the root's
  store under the kernel's own key (debounced 0.15 s). A flush describes itself: it
  mints its own `flush` row and writes it in the same batch, so the flusher's own
  traffic is recognised by what it carried, never by an exclusion. A settled execution
  leaves RAM once flushed. A NAMED call (`id` on the item) is looked up in the store
  before dispatch, so a re-send returns the recorded outcome — across a flush and a
  restart (C9). `ledger` is a projection over the store plus RAM.
- **Retention** — `ledger_cap` (`--ledger-cap N|inf` / `FANTASTIC_LEDGER_CAP`): over the
  cap the oldest settled executions are pruned down to 90% — unnamed first, named last;
  an unacknowledged dead letter is kept.
- **Dead letters** — a `failed`/`timed_out`/`cancelled` execution is announced by the
  `on_deadletter` event on the root; `ack_deadletter` acknowledges it. There is no
  re-send path.
- **The id counter** — one kernel counter mints every row id. Its high-water mark
  `next_id` sits in the root record beside `guid`, reserved in blocks of 1000: a start
  writes the reservation, a started kernel writes each new block. A counter moved
  backwards is the admin's to fix.
- **Conversations** — each AI agent names its own `memory_store_id`; its turns, folds and
  views are rows under its key. Reply BODIES are never stored — a reply row keeps its
  one-line `origin` (what to re-read), and after a restart the model re-reads from it.
- **Raw bytes are a LENGTH.** A `bytes` value anywhere in a stored payload or result is
  written `{len: N}` — the only reduction; base64 is banned.
- **Stop:** `shutdown_agents` (drain, the `shutdown` verb, quiet — cancelled work becomes
  dead letters), then the kernel's final flush while the store is alive, then the
  loader's `flush_pending` writes the pending RECORDS, then the teardown walk.
  `kernel_stop` does all of it.
