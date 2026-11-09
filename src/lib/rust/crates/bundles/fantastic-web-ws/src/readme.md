# web_ws — inbound WebSocket leg

Mounts `ws://host/<id>/ws` on a parent `web` agent: the host's one catch-all `/{host_id}/ws`. `route` (optional record field): a literal mount path such as `/page_ws/ws` — the leg then mounts only that path, and the URL's first segment names the host it watches. A second catch-all leg on the same host fails web boot; a `route` that is not a string starting with `/` with no `{` or `}` is refused at the write. **Sealed by default** — a bare leg
denies all frames. Open: `update_agent <id> ingress_rule=allow_all_ingress`.
Token on frame **envelope** (`auth_token`, sibling of `type`/`target` — never inside payload).

Frames: `call` · `emit` · `watch` · `unwatch` · `state_subscribe` · `state_unsubscribe` · `hello`
Emits (on this leg): `socket_open {client_id}` when a socket connects, `socket_closed {client_id, guid}`
when it closes (peer closed, tunnel died, keepalive timeout), after its waiting calls settled. The dialed
side never dials back: a program that needs the link watches the leg and waits for the next `socket_open`.
Binary frames supported (4-byte-BE-length header + JSON envelope + raw bytes): the
envelope holds `null` where the bytes belong and `_binary_path`, the steps to them — a
JSON array, a key as a string (taken literally: `["payload","a.b"]` is the key `"a.b"`),
a list index as an int (`["payload","items",0,"data"]`).

`{type:"call", target, payload, id?}` — the reply is the call's result.

**One leg is one principal**: every admitted frame (call, emit, watch,
state_subscribe) is handed to this leg (its `inbound_*` verbs), which performs it AS
ITSELF — its `can_call` and the target's `callable_by` decide, and execution names
are one caller's. Everyone admitted to one leg is one tenant.

**The page as a peer kernel.** A client that is itself a kernel (the browser's ts
kernel) sends `{type:"hello", guid}`; this leg then registers that guid with the host
kernel, which routes `send`s to `<guid>|<agent>` over this socket as
`{type:"call", id, target, payload}` frames — answered by
`{type:"reply"|"error", id, data, secret?}` — `secret` (present only when the leg's
onward reply named secret paths) lists dotted paths into `data` that the far end
records as `[secret]`; `data` is always whole. One guid per connection (pinned by the first
hello); a sealed leg carries no peer. **The tabs of one identity are one principal**:
a guid already live on another connection is another tab of it,
and this connection joins it — a host call to `<guid>|<agent>` goes to EVERY live
connection of that guid and settles with the first reply (the others are dropped); a
host emit on `<guid>|<agent>` goes to every one as `{type:"emit", target, payload}`,
and a reply this leg sends one tab is mirrored to the others as `{type:"reply_mirror", id, data}`. When a socket closes, that connection stops
being a route (the guid stays an address while another connection holds it) and every
call still waiting on it settles as a `transport_error`. This leg only carries the frames: the
host kernel routes and gates the sender, the page kernel gates its own agents.

**A kernel's leg.** A `ws_bridge` dialing in says `{type:"hello", guid, leg}` (its own
id as `leg`): the same address, plus watches. A host `watch` of `<guid>|<agent>` (gated
like a send: `watch` on that id in the watcher's `can_call`) rides this socket as
`{type:"watch"|"unwatch", src}`; the leg streams the agent's events back as
`{type:"event", src, payload}` to the host's watchers. One connection per kernel guid:
a newer hello for it replaces the older connection (closed; its waiting calls settle
`transport_error`, text `bridge_down: …`), and the host's watches of the guid are asked
for again on the new one — events emitted while no connection was up are not replayed.
A page's agents cannot be watched. Call frame ids carry a per-connection nonce
(`h-<nonce>-<n>`), so a reconnected kernel never sees an id twice.
