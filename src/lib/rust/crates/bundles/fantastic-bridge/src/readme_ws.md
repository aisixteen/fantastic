# ws_bridge — outbound WebSocket bridge

Dials a remote `web_ws` surface at `ws://<host>:<port>/<peer_id>/ws` (`host` explicit). **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`.
Token on frame envelope (`auth_token`). Same `ingress_rule`/`egress_rule`/`auth` as every io_bridge leg.

Verbs: `boot` · `reconnect` · `forward(target,payload[,timeout])` · `watch_remote(target)` · `unwatch_remote(target)` · `reflect`
Transports: `ws` (default) · `memory` (test). No tunnel is spawned here — an ssh tunnel is an `exec_bridge` job or devops; dial its local end.
Emits: `bridge_up` · `bridge_down` · remote events for a `watch_remote` subscription only (payload + `src`); anything unsubscribed is dropped.

Inbound calls (a paired transport) are served only from the ONE pinned peer: set `peer_guid` (the peer kernel's guid). Every frame names its caller `<guid>|<agent>`, and the target's `callable_by` must name it (or `<guid>|*`) — accepting this leg is not accepting its peer.
