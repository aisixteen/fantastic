# web_ws — inbound WebSocket leg

Mounts `ws://host/<id>/ws` on a parent `web` agent. **Sealed by default** — a bare leg
denies all frames. Open: `update_agent <id> ingress_rule=allow_all_ingress`.
Token on frame **envelope** (`auth_token`, sibling of `type`/`target` — never inside payload).

Frames: `call` · `emit` · `watch` · `unwatch` · `state_subscribe` · `state_unsubscribe` · `hello`
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
`{type:"reply"|"error", id, data}`. One guid per connection (pinned by the first
hello); a sealed leg carries no peer. **The tabs of one identity are one principal**
(owner, 2026-10-03): a guid already live on another connection is another tab of it,
and this connection joins it — a host call to `<guid>|<agent>` goes to EVERY live
connection of that guid and settles with the first reply (the others are dropped); a
host emit on `<guid>|<agent>` goes to every one as `{type:"emit", target, payload}`,
and a reply this leg sends one tab is mirrored to the others as `{type:"reply_mirror", id, data}`. When a socket closes, that connection stops
being a route (the guid stays an address while another connection holds it) and every
call still waiting on it settles as a `transport_error`. This leg only carries the frames: the
host kernel routes and gates the sender, the page kernel gates its own agents.
