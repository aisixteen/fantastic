# io_bridge — IO channel model (shared library, not an agent)

The kernel interior is **open**. Every IO edge is **sealed by default** — a fresh leg
denies until the operator opens it consciously (G2). Content is always addressed
**by agent id, never by path** (G1).

## Open a sealed leg
```
update_agent <id> ingress_rule=allow_all      # open wide (local/dev)
update_agent <id> ingress_rule=password       # require group token ($FANTASTIC_GROUP_TOKEN)
```
Token goes on the **frame envelope** (`auth_token`), never inside the payload.

## Rules
| name | effect |
|---|---|
| `allow_all` | open |
| `deny_inbound` | sealed (default) |
| `password` | require env token |
| `silent` | egress default (present nothing) |

## Derivations (the actual agents)
`ws_bridge` · `relay_connector` · `web_ws` · `web_rest` · `file_bridge` — each has its
own readme; `reflect readme=true` on the sealed agent to learn how to open it.
