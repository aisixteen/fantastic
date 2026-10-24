# ws_bridge — outbound WebSocket bridge

Dials a remote `web_ws` surface at `ws://<host>:<port>/<peer_id>/ws` (`host` explicit). **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`. Rules are read off the record per frame: a seal or an opening on a LIVE leg holds from the next frame, no reboot.
Token on frame envelope (`auth_token`). Same `ingress_rule`/`egress_rule`/`auth` as every io_bridge leg.

Verbs: `boot` · `reconnect` · `forward(target,payload[,forward_timeout])` · `watch_remote(target)` · `unwatch_remote(target)` · `reflect`
Transports: `ws` (default) · `memory` (test). No tunnel is spawned here — an ssh tunnel is an `exec_bridge` job or devops; dial its local end.
Emits: `bridge_up` · `bridge_down` · remote events for a `watch_remote` subscription only (payload + `src`); anything unsubscribed is dropped.

Inbound calls (a paired transport) are served AS THIS LEG: its `can_call` and the target's `callable_by` decide.

Frame ids are `<leg id>-<boot nonce>-<n>` — fresh per boot, so a restarted sender never names an execution the peer already settled.

Reply frames: `{type:"reply", id, data, secret?}`. `secret` lists dotted paths into `data` (the far bundle's reply marks); a forward notes them, so the calling kernel records the forward's reply as `[secret]` while the caller gets `data` whole. A malformed `secret` is ignored and the reply still delivered. A forwarded `set` marks its own `payload.fields.<name>` in the caller's records (`payload_secret`).
