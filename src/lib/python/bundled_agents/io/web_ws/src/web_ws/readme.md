# web_ws — inbound WebSocket leg

Mounts `ws://host/<id>/ws` on a parent `web` agent. **Sealed by default** — a bare leg
denies all frames. Open: `update_agent <id> ingress_rule=allow_all_ingress`.
Token on frame **envelope** (`auth_token`, sibling of `type`/`target` — never inside payload).

Frames: `call` · `emit` · `watch` · `unwatch` · `state_subscribe` · `state_unsubscribe` · `hello`
Binary frames supported (4-byte-BE-length header + JSON envelope + raw bytes): the
envelope holds `null` where the bytes belong and `_binary_path`, the steps to them — a
JSON array, a key as a string (taken literally: `["payload","a.b"]` is the key `"a.b"`),
a list index as an int (`["payload","items",0,"data"]`).

`{type:"call", target, payload, id?, secret?}` — `secret` is the item's marks
(`["payload.key"]`, `["result"]`, an array of steps `[["payload","a.b"]]`): the call
is dispatched with them, so this kernel
records `<secret>` for those values exactly as for a local caller; a malformed mark is
the kernel's own `bad_arg` refusal, before dispatch. The reply is the real result.

**One leg is one principal** (audit S6, 2026-10-03): every connection admitted to this
leg is dispatched AS the leg, so grants, a credential_store's `readers`, execution
names and a secret-marked execution's join (the real outcome goes to its original
caller — the leg) see ONE caller. Everyone admitted to one leg is one principal — one
tenant per leg.

**The page as a peer kernel.** A client that is itself a kernel (the browser's ts
kernel) sends `{type:"hello", guid}`; this leg then registers that guid with the host
kernel, which routes `send`s to `<guid>|<agent>` over this socket as
`{type:"call", id, target, payload, caller}` frames — answered by
`{type:"reply"|"error", id, data}`. One guid per connection (pinned by the first
hello); a sealed leg carries no peer. **The tabs of one identity are one principal**
(owner, 2026-10-03): a guid already live on another connection is another tab of it,
and this connection joins it — a host call to `<guid>|<agent>` goes to EVERY live
connection of that guid and settles with the first reply (the others are dropped); a
host emit on `<guid>|<agent>` goes to every one as `{type:"emit", target, payload, caller?}`,
and a reply this leg sends one tab is mirrored to the others as `{type:"reply_mirror", id, data}`. A
host call marked `secret` carries its paths on that frame (`secret`), so the
page kernel masks its records of it too. When a socket closes, that connection stops
being a route (the guid stays an address while another connection holds it) and every
call still waiting on it settles as a `transport_error`. This leg only carries the frames: the
host kernel routes and gates the sender, the page kernel gates its own agents.
