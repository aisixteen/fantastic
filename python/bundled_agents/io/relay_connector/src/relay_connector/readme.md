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

Record fields: `relay_url` · `guid` (our id = WS path) · `partner_guid` (peer to
reach) · `relay_token` (X-Fantastic-Auth) · `heartbeat` (s, default 30).
Transport: `relay`.
