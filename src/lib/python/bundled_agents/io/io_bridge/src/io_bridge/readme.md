# io_bridge — IO channel model (shared library, not an agent)

The kernel interior is **open**. Every IO edge is **sealed by default** — a fresh leg
denies until the operator opens it consciously (G2). Content is always addressed
**by agent id, never by path** (G1).

## Open a sealed leg
```
update_agent <id> ingress_rule=allow_all_ingress      # open wide (local/dev)
update_agent <id> ingress_rule=password_ingress       # require group token ($FANTASTIC_GROUP_TOKEN)
```
Token goes on the **frame envelope** (`auth_token`), never inside the payload.

## Rules — names are MANIFOLD-SCOPED
A rule name binds to exactly one direction. There is no name that means one thing
inbound and another outbound, so a name written into the wrong field is a loud
error instead of a silent policy change.

| `ingress_rule` (who may call in) | effect |
|---|---|
| `allow_all_ingress` | open |
| `deny_ingress` | sealed (the default when the field is absent) |
| `password_ingress` | require env token |

| `egress_rule` (what we present outbound) | effect |
|---|---|
| `silent_egress` | present nothing (the default) |
| `password_egress` | stamp the group token |

`auth` still sets both at once and takes the bare **stem** (`auth: password` ⇒
`password_ingress` + `password_egress`); an inbound-only stem (`allow_all`, `deny`)
presents nothing outbound. Writing an egress name into `ingress_rule` — or an
egress-only stem into `auth` — seals the leg AND names the mistake, rather than
looking like an ordinary seal. That failure was real: `auth: silent` used to seal
every inbound call while reporting only a bare unknown name.

## Derivations (the actual agents)
`ws_bridge` · `relay_connector` · `web_ws` · `web_rest` · `file_bridge` ·
`exec_bridge` — each has its own readme; `reflect readme=true` on the sealed agent to
learn how to open it.

The last two are MODALITY derivations, not wire bridges: they import only
`gate_inbound` / `resolve_ingress` / `describe` and supply their own verbs, where
`ws_bridge` and `relay_connector` use the engine (`make_verbs`, transports, the codec).
`file_bridge` is the fs edge, `exec_bridge` the process edge; each keeps its dangerous
primitive in ONE module (`fs`, `proc`) so its law — running-dir clamp, interpreter
allowlist — cannot be reached around.
