# ws_bridge — outbound WebSocket bridge

Dials a remote `web_ws` surface at `ws://<host>:<port>/<peer_id>/ws` (`host` explicit). **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`. Rules are read off the record per frame: a seal or an opening on a LIVE leg holds from the next frame, no reboot.
Token on frame envelope (`auth_token`). Same `ingress_rule`/`egress_rule`/`auth` as every io_bridge leg.

Verbs: `boot` · `reconnect` · `forward(target,payload[,timeout])` · `watch_remote(target)` · `unwatch_remote(target)` · `reflect`
Transports: `ws` (default) · `memory` (test). No tunnel is spawned here — an ssh tunnel is an `exec_bridge` job or devops; dial its local end.
Emits: `bridge_up` · `bridge_down` · remote events for a `watch_remote` subscription only (payload + `src`); anything unsubscribed is dropped.

Secret values: mark your `forward` call — `secret: ["payload.payload.<field>"]` /
`["result"]`, or an array of steps `[["payload","payload","a.b"]]` — and the frame
carries them as `payload.<field>` / `result` / `["payload","a.b"]`, so the far
kernel records `<secret>` too (see `io_bridge`). An inbound frame's `secret` is
dispatched verbatim.

**SEALED for secrets to a non-loopback host** (owner, 2026-10-03): `ws://` carries
every frame in clear, so on a leg whose `host` is not `localhost` / `127.0.0.1` /
`[::1]` a `forward` call carrying ANY `secret` mark is refused before a frame leaves,
and an inbound `call` frame that names `secret` is answered with the same refusal and
never dispatched: `{reason:"unauthorized", error:"ws_bridge: a call carrying secret
marks may not cross a ws:// leg to a non-loopback host — ws:// carries every frame in
clear", hint:"dial a loopback host (an ssh tunnel's local end) to send a secret"}`.
Unmarked calls cross as before. wss:// (TLS) is the later path.

Inbound calls (a paired transport) are served only from the ONE pinned peer: set `peer_guid` (the peer kernel's guid). Every frame names its caller `<guid>|<agent>`, and the target's `callable_by` must name it (or `<guid>|*`) — accepting this leg is not accepting its peer.

Frame ids are `<leg id>-<boot nonce>-<n>` — fresh per boot, so a restarted sender never names an execution the peer already settled.
