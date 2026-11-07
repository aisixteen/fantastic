# ws_bridge — outbound WebSocket bridge

Dials a remote `web_ws` surface at `ws://<host>:<port>/<peer_id>/ws` (`host` explicit). **Sealed by default** — open: `update_agent <id> ingress_rule=allow_all_ingress`. Rules are read off the record per frame: a seal or an opening on a LIVE leg holds from the next frame, no reboot.
Token on frame envelope (`auth_token`). Same `ingress_rule`/`egress_rule`/`auth` as every io_bridge leg.

Verbs: `boot` · `reconnect` · `forward(target,payload[,forward_timeout])` · `watch_remote(target)` · `unwatch_remote(target)` · `reflect`
Transports: `ws` (default) · `memory` (test). No tunnel is spawned here — an ssh tunnel is an `exec_bridge` job or devops; dial its local end.
Emits: `bridge_up` · `bridge_down` · remote events for a `watch_remote` subscription only (payload + `src`); anything unsubscribed is dropped.

**Redial.** When the link drops, the leg emits `bridge_down` and redials the same `host`/`port`/`peer_id` by itself — at once, then after 1 s, doubling to 30 s, until a link is up (`bridge_up`). Its `watch_remote` subscriptions are asked for again on the new link, so a local watcher of the leg keeps receiving without subscribing anew; calls in flight at the drop fail, and the next call answers. `shutdown` (or deleting the leg) ends the redial; `reconnect` stays the manual verb.

**Two-way.** At every boot (`reconnect` included) the first frame is `{type:"hello", guid, leg}` (this kernel's guid, when it has one): the remote `web_ws` then reaches this kernel's agents as `<guid>|<agent>`, and a redial replaces the dropped connection there. Inbound calls are served AS THIS LEG: its `can_call` and the target's `callable_by` decide. An inbound `{type:"watch"|"unwatch", src}` watches a local agent AS THIS LEG (the ingress rule, then `watch` on `src` in its `can_call` and the target's `callable_by`); its events go back as `{type:"event", src, payload}` until `unwatch` or the link drops, and a reply goes out after the events its call produced. A refused watch is answered `{type:"error", src, …}`.

Frame ids are `<leg id>-<boot nonce>-<n>` — fresh per boot, so a restarted sender never names an execution the peer already settled.

Reply frames: `{type:"reply", id, data, secret?}`. `secret` lists dotted paths into `data` (the far bundle's reply marks); a forward notes them, so the calling kernel records the forward's reply as `[secret]` while the caller gets `data` whole. A malformed `secret` is ignored and the reply still delivered. A forwarded `set` marks its own `payload.fields.<name>` in the caller's records (`payload_secret`).
