# web_ws — inbound WebSocket leg

Mounts `ws://host/<id>/ws` on a parent `web` agent. **Sealed by default** — a bare leg
denies all frames. Open: `update_agent <id> ingress_rule=allow_all`.
Token on frame **envelope** (`auth_token`, sibling of `type`/`target` — never inside payload).

Frames: `call` · `emit` · `watch` · `unwatch` · `state_subscribe` · `state_unsubscribe`
Binary frames supported (4-byte-BE-length header + JSON envelope + raw bytes).
