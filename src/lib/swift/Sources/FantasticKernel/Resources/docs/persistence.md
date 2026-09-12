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
own `agent.json` — one bridge, records + sidecars together.

The root's `reflect` ALSO carries a stable **`guid`** — the kernel's own
instance identity, minted ONCE at boot and persisted into the root record
(`.fantastic/agent.json`). It is PERSISTENCE-scoped: it round-trips a reboot
verbatim (an explicit `guid` always wins, a minted one is never regenerated)
— unlike the run-scoped `env`/`version`, which are read inline each reflect.
A relay peer advertises this value as its `owner_guid` (distinct from a relay
LEG's own `guid` on a `relay_connector` record).

## The message log — `messages.xml`

Every message the kernel makes — a turn, a delivered reply, a fold, a recall, a `body`
(a reply too large to inline, its text merged in), a `view`, a `call` per send, a `task`
per fork — is one `<row>` element, appended in id order, in `.fantastic/messages.xml`:
ONE XML document opened by `<messages v="1">` and never closed (a stream). Scalars are
attributes, text (`content`, `title`, `why`) and lists are child elements; the stdlib
XML layer escapes `<` and `&` on the way out and restores them on the way in, so a body
holding `</content>` cannot break the log. Hydrate is the incremental parser
(`XMLPullParser`), one completed row at a time; EOF without `</messages>` is normal.
A row cut by a crash mid-append, or a log of another version, is a LOUD error at boot —
nothing is skipped or repaired; truncate at the last complete `</row>`.

- **The kernel never knows disk.** It starts empty every boot. The bootstrap runs this
  bundle's cold `read_messages` (through `fs`, before any agent or id exists) and hands
  the rows to the kernel from above, setting the counter to `max(id) + 1`. A second
  ingestion of the same log is refused by the kernel on its first duplicate id — the
  user's fault, surfaced, never guarded.
- **The pump** (`_MessagePump`, root loader only) tails `rows_stream(after)` from the
  highest id present, batches 0.15 s, and appends one `write_stream` per batch through
  the discovered provider. Its own calls to the store — and every operator-context
  call on the store, i.e. this bundle's persistence traffic — never enter the log.
- **Order, by contract:** hydrate → subscribe → only then children. The loader's `boot`
  subscribes right after the ledger rehydrates and before anything that creates or
  wakes agents.
- **No provider ⇒ RAM only**, said once on stderr. Swap the sink by wiring a provider
  that answers `write_stream`; a table-backed one later needs no kernel change.
- **Stop:** `shutdown_agents` (drain, the `shutdown` verb, quiet) then the kernel awaits
  `flush_pending` — the pump lands every message minted so far — before the teardown
  walk. `shutdown_kernel` does all of it.
