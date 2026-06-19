# ws_bridge — outbound WebSocket bridge

Dials a remote `web_ws` surface. **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all`.
Token on frame envelope (`auth_token`). Same `ingress_rule`/`egress_rule`/`auth` as every io_bridge leg.

Verbs: `boot` · `reconnect` · `forward(target,payload[,timeout])` · `watch_remote(target)` · `unwatch_remote(target)` · `reflect`
Transports: `ws` (default) · `ssh+ws` · `memory` (test)
Emits: `bridge_up` · `bridge_down` · remote events via `watch_remote`
