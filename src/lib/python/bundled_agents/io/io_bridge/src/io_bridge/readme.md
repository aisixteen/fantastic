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
The change is LIVE: every leg — the wire bridges (`ws_bridge`, `relay_connector`)
as much as `web_ws`/`web_rest`/`file_bridge`/`exec_bridge` — reads its
`ingress_rule`/`egress_rule`/`auth` off its record per frame, so a seal (or an
opening) takes effect on the very next frame, no reboot, and `reflect` always shows
the rule the gate enforces.

## Rules — names are MANIFOLD-SCOPED
A rule name binds to exactly one direction. There is no name that means one thing
inbound and another outbound, so a name written into the wrong field is a loud
error instead of a silent policy change.

| `ingress_rule` (who may call in) | effect |
|---|---|
| `allow_all_ingress` | open |
| `deny_ingress` | sealed (the default when the field is absent) |
| `password_ingress` | require env token (compared as UTF-8 bytes, constant-time — any presented token is a plain allow/deny) |

| `egress_rule` (what we present outbound) | effect |
|---|---|
| `silent_egress` | present nothing (the default) |
| `password_egress` | stamp the group token |

`auth` still sets both at once and takes the bare **stem** (`auth: password` ⇒
`password_ingress` + `password_egress`); an inbound-only stem (`allow_all`, `deny`)
presents nothing outbound. Writing an egress name into `ingress_rule` (or any name
no rule implements, or a spec that is not a string or `{type, env?}` of strings) is
refused at the write, naming the legal values; an egress-only stem in `auth` seals
the leg AND names the mistake, rather than looking like an ordinary seal. That
failure was real: `auth: silent` used to seal every inbound call while reporting
only a bare unknown name.

## Secret marks cross the wire
A call frame is `{type:"call", id, target, payload, caller?, secret?}`. `secret` is
the batch item's marks — dotted strings (`payload.<field…>` / `result[.<field…>]`)
and/or arrays of steps (`["payload", "a.b"]`: a string step a literal key, an int
step a list index) — carried verbatim: the receiving kernel dispatches the item WITH them, so its call/settle
rows, ledger, state stream and memory store show `<secret>` exactly as the caller's
do, and a malformed mark is its own refusal, before dispatch. The reply carries the
real result back. No leg, transport or codec logs or keeps a frame. EXCEPT a relay:
`relay_connector` is SEALED for marks (relayd records what it routes unmasked) — it
refuses a marked `forward` and answers a marked inbound `call` with a refusal.

`forward` takes the marks from ITS OWN call and re-roots them at the forwarded item,
each in the spelling it came in: `payload.payload.<f…>` → `payload.<f…>`,
`["payload","payload",<step>…]` → `["payload",<step>…]`, `result[.<f…>]` /
`["result",<step>…]` as is; a mark on forward's own fields (`payload.target`,
`payload.timeout`) stays on this kernel's record.
```
send([{target:"<bridge>", payload:{type:"forward", target:"creds",
       payload:{type:"set", ref, fields:{api_key:"…"}}},
       secret:["payload.payload.fields"]}])
```

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

## Frame ids are unique across boots
A `forward`'s frame `id` is `<leg id>-<nonce>-<n>`: the nonce is minted at EVERY
boot (12 hex chars), the count restarts under it. The receiver uses the frame id as
the execution's NAME, so an id a restarted sender reused would JOIN an execution it
had already settled — answered with the old outcome, the new call never run.
