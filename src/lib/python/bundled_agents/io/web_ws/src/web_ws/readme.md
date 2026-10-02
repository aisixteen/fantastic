# web_ws — inbound WebSocket leg

Mounts `ws://host/<id>/ws` on a parent `web` agent. **Sealed by default** — a bare leg
denies all frames. Open: `update_agent <id> ingress_rule=allow_all_ingress`.
Token on frame **envelope** (`auth_token`, sibling of `type`/`target` — never inside payload).

Frames: `call` · `emit` · `watch` · `unwatch` · `state_subscribe` · `state_unsubscribe` · `hello`
Binary frames supported (4-byte-BE-length header + JSON envelope + raw bytes): the
envelope holds `null` where the bytes belong and `_binary_path`, the steps to them — a
JSON array, a key as a string (taken literally: `["payload","a.b"]` is the key `"a.b"`),
a list index as an int (`["payload","items",0,"data"]`).

`{type:"call", target, payload, id?, secret?}` — `secret` is the item's marked paths
(`["payload.key"]`, `["result"]`): the call is dispatched with them, so this kernel
records `<secret>` for those values exactly as for a local caller; a malformed mark is
the kernel's own `bad_arg` refusal, before dispatch. The reply is the real result.

**The page as a peer kernel.** A client that is itself a kernel (the browser's ts
kernel) sends `{type:"hello", guid}`; this leg then registers that guid with the host
kernel, which routes `send`s to `<guid>|<agent>` over this socket as
`{type:"call", id, target, payload, caller}` frames — answered by
`{type:"reply"|"error", id, data}`. One guid per connection (pinned by the first
hello); a guid already live on another connection is refused; a sealed leg carries no
peer. A host call marked `secret` carries its paths on that frame (`secret`), so the
page kernel masks its records of it too. When the socket closes the guid stops being an address and every call still
waiting on it settles as a `transport_error`. This leg only carries the frames: the
host kernel routes and gates the sender, the page kernel gates its own agents.
