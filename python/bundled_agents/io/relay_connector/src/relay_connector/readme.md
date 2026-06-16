# relay_connector — relay-kernel router bridge

Dial a relay-KERNEL (`../fantastic_relay`) at `ws://<host>/<guid>` with the group
password in `X-Fantastic-Auth` and subprotocol `fantastic.relay.v1`. Reach a
partner kernel by its GUID; the relay routes by `target`. Same verbs/auth surface
as `ws_bridge` (both ride the shared `io_bridge` engine) — `forward`,
`watch_remote`/`unwatch_remote` work identically; replies and binary
`read_stream` chunks are tunneled over the relay.

**Sealed by default** — open the inbound leg: `update_agent <id> ingress_rule=allow_all`
(or `password` for the group token). The bridge-leg `ingress_rule`/`egress_rule`
gate the tunneled calls and are INDEPENDENT of the relay's own connection auth.

**Self-healing** — the leg auto-connects (eager initial dial + background
auto-connect) and auto-reconnects with the `reconnect` backoff (seconds, default
10; `0` = one-shot/legacy, raise on first failure). `reflect.connected` tracks the
LIVE socket. A no-reply `keepalive` verb is sent every `heartbeat` seconds to keep
the relay's liveness view of us green.

**Directory** (the relay's own `relay` agent, addressed `target:"relay"`, NOT the
partner) — render/track the live peer list without polling:
- `list_peers` → one-shot `{peers:[{guid, status(green|yellow|red), last_seen, since}]}`.
- `watch_directory` / `unwatch_directory` → subscribe; live `peer_joined`/
  `peer_left`/`peer_evicted`/`peer_status` events re-emit on THIS connector's inbox
  (a local `kernel.watch(<connector_id>)` sees them).

**Identity** — `guid` (our id = the WS path) is **auto-minted on first boot if
absent and persisted** into the record, so every later hydration re-dials the SAME
`ws://host/<guid>`; pass an explicit `guid` to pick your own (it always wins), and a
minted one is never regenerated. `partner_guid` stays explicit (you can't invent a
peer's address) — discover it via the partner's `reflect` or the directory
(`list_peers`). Caveat: a persisted `guid` is a durable identity, so cloning a
`.fantastic` store to another machine carries the same id and the relay will see a
duplicate — re-mint by clearing `guid` on the clone.

**Directory typing (kernelgroup)** — a connector advertises an opaque **attrs blob**
to the relay (the relay STORES + reflects it into `list_peers` entries + a
`peer_updated` event, and never interprets it). Well-known keys:
- `role` — `"manager" | "kernel"` (default `kernel`). A **manager** owns kernels and
  exposes control; a **kernel** is driven (standalone, or owned by a manager).
- `owner_guid` — the managing peer's guid (`null` = standalone).
- `exposes` — an opaque control-surface list the peer advertises (e.g.
  `["stop","restart"]`).

Set them at create (record meta) and they advertise on connect (re-announced on
every reconnect — the relay drops per-connection state on a drop). `set_identity`
(verb: optional `role`/`owner_guid`/`exposes`, merged) updates them live + persists,
so a manager can publish/retract a kernel's control surface at runtime. **reach ≠
control**: the directory carries who-owns-what; driving a kernel owned by another
manager is a plain `forward` to that manager (gated by ingress, as always). A plain
peer (no typed keys) advertises nothing — identical to an un-typed leg.

Record fields: `relay_url` · `guid` (auto-minted if absent) · `partner_guid` (peer to
reach) · `relay_token` (X-Fantastic-Auth) · `heartbeat` (s, default 30) ·
`reconnect` (s, default 10) · `role` · `owner_guid` · `exposes`. Transport: `relay`.
