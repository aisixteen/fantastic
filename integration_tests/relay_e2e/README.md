# relay_e2e — kernels through the relay-kernel router (`relay_connector`)

End-to-end proof that two host kernels reach each other through a **relay-KERNEL
router** (`relayd`, sibling repo `../fantastic_relay/relaykernel`) via the
`relay_connector` transport. Each kernel runs a `relay_connector` agent that dials
the relay at `ws://<host>/<guid>` (subprotocol `fantastic.relay.v1`, group password
in the `X-Fantastic-Auth` header, checked once at the WS upgrade). The relay routes
by `target`; the connectors **tunnel** the shared `io_bridge` `call`/`reply` frames
to each other by GUID, so a `kernel.send` round-trips A → relay → B → reply.

No certs, no TLS/mTLS, no Ed25519, no token-issuer, no rendezvous — that whole
zero-trust `cloud_bridge` model is gone. The only relay-connection secret is the
group password (`RELAY_PASSWORD`), set on the relay as `FANTASTIC_GROUP_TOKEN` with
`RELAY_INGRESS_RULE=password_ingress`. The per-leg `ingress_rule`/`egress_rule`/`auth` that
gate the **tunneled** bridge calls are INDEPENDENT of that connection password.

On each kernel the password lives in a `credential_store`, never on the leg's record
(`relay_token` is not a `relay_connector` field). `seed_relay` (operator, before the
daemon starts) creates `creds` declaring the ref `relay/group` (`host` = the relay's
`host:port`), accepting `get` from the leg `cb` and `set` from `web_ws`;
`set_relay_password` writes the `password` field through `web_ws`, marked
`secret: ["fields"]`; the leg names `credentials: {store: creds, ref: relay/group}`.

## Grants (the kernel is deny-all)

Every agent is stamped `{$self: "*"}` on both `can_call` and `callable_by`; every
other edge is granted explicitly on BOTH ends. Each kernel is seeded (operator
one-shots, `helpers/seeding.py` + `seed_relay`) with:

| agent | role | grants |
|---|---|---|
| `kernel_state` (root) | names `memory_store_id: memory` (no daemon starts without one) | accepts `web_ws` (`*`) and the leg's `reflect` |
| `store` | `file_bridge` @ `.fantastic` — the record store | — (the loader writes as bookkeeping) |
| `memory` | `ram_store` — the kernel's execution rows | — (the kernel writes as bookkeeping) |
| `web` | the HTTP host | reaches `$subtree` (pulls its legs' `get_routes`) |
| `web_ws` | the driver — every `KernelProc.call` is performed AS it | reaches the root, `creds` (`set`), `cb` (`*`); accepts `$parent` |
| `creds` | `credential_store`, file backend | accepts `cb` (`get`), `web_ws` (`set`) |
| `cb` | the `relay_connector` leg, created by `web_ws` under the root | reaches `creds` (`get`), the root (`reflect`) — a tunneled inbound call is served AS the leg; accepts `web_ws` |

The stream test adds B's `fs` file_bridge, accepting the leg's
`write_stream`/`read_stream`, and names it in B's leg's `can_call`.

## Two targets (like the rest of the suite)

- **`FANTASTIC_TARGET=local`** (default): a local `relayd` subprocess on loopback;
  kernels dial `ws://127.0.0.1:<port>`.
- **`FANTASTIC_TARGET=container`**: the `relay:latest` image, published on the host;
  kernel CONTAINERS reach it via the host gateway
  `ws://host.containers.internal:<port>` (the cross-container "unit" model — no
  shared network).

## The matrix

`test_relay_any_to_any` (and the auth variants) are parametrized over the **3 host
runtimes × 2** — every unordered pair (**any-to-any**) plus **same-kind**:

| pairs |
|---|
| python↔python · python↔rust · rust↔rust |

All three host runtimes ship a `relay_connector` transport (same `handler_module`,
`relay_connector.tools`) and interop any-to-any over the same relay. A pair skips
only if a runtime's binary isn't built (clear skip reason).

### Tests

- **`test_relay_any_to_any`** — A is sealed (`auth="deny"` ⇒ `deny_ingress`), B is
  opened with `auth="allow_all"` (⇒ `allow_all_ingress`). A→B forward round-trips B's
  root reflect; B→A is refused on arrival at A with `{reason:"unauthorized"}` — the
  cross-runtime wire-shape guard. (Rule names are manifold-scoped: `auth` takes the
  bare stem, `ingress_rule`/`egress_rule` the scoped name, and `reflect` reports
  the scoped names.)
- **`test_relay_password_group_member`** — both legs `auth="password"`
  (⇒ `password_ingress` + `password_egress`) with the SAME group token in env
  (`FANTASTIC_GROUP_TOKEN`, never persisted): the token is
  presented on each call envelope, survives the relay, is checked on arrival —
  A→B and B→A both round-trip across every pair.
- **`test_relay_password_rejects_outsider`** — A presents a DIFFERENT token than B
  expects → B's `password` gate refuses `unauthorized`. B = each runtime in turn
  (the enforcing receiver).
- **`test_relay_asymmetric_rules`** — A is a hub with the symmetric SPLIT
  (`ingress_rule="deny_ingress"` + `egress_rule="password_egress"`), B a group member.
  A→B round-trips (A's egress presents, B's ingress checks); B→A is denied (A's
  ingress). Independent per-direction rules, identical wire shape everywhere.
- **`test_relay_directory[python|rust]`** — two connectors of the runtime
  both connect, then `list_peers` (addressed to the relay's own `relay` agent,
  `target:"relay"`) returns BOTH peers with `status:"green"`, and
  `watch_directory` acks. (The live event-surfacing onto the connector inbox is
  unit-tested per runtime.)
- **`test_relay_reconnects_after_relay_restart`** (python↔python) — a leg with
  `reconnect=1` forwards, the relay is killed + restarted on the SAME port, the
  leg re-dials with its backoff, and a forward round-trips again over the healed
  connection. The reconnect logic is shared/mirrored across runtimes.
- **`test_relay_kernelgroup_typing[python|rust]`** — a manager peer + an
  owned kernel join; `list_peers` carries each peer's `announce`d attrs blob
  (`attrs.role`/`owner_guid`/`exposes`, stored verbatim by the relay) and
  `set_identity` live-updates it. `owner_guid` is the manager's kernel-instance
  guid (its root reflect's `guid`), not its relay-leg guid.
- **`test_relay_streams_raw_bytes[python|rust]`** (`test_relay_stream.py`) —
  a python A writes raw (non-UTF-8) bytes over a binary web_ws frame → its leg →
  the relay → B's leg → B's `fs`, they land verbatim on B's disk, and `read_stream`
  brings them back byte-identical over the same chain.

## Run it

Heavy + opt-in (kept out of the default `pytest` run); skips cleanly when `relayd`
isn't built or the engine/image is absent.

```sh
# 1. build the relay kernel — once
cd ../fantastic_relay/relaykernel && swift build      # → .build/{debug,release}/relayd
#    (or, for the container target: sh ../fantastic_relay/relaykernel/container/build.sh)

# 2. run (from integration_tests/)
uv run pytest relay_e2e/ -v
#    container target:  FANTASTIC_TARGET=container uv run pytest relay_e2e/ -v
```

## How it works (no mocks)

- `relay_harness.Relay` locates the newest built `relayd` (release preferred), boots
  it on a loopback port with `FANTASTIC_GROUP_TOKEN=<RELAY_PASSWORD>` +
  `RELAY_INGRESS_RULE=password_ingress` (local target), or runs `relay:latest` published on
  the host (container target). It skips cleanly when the binary/image is missing.
- Each daemon boots with `store` + `memory` + `web` + `web_ws` (drivable over WS, no
  auto-booting relay leg) + the `creds` store holding the relay password, all granted
  as in the table above. The test then `create_agent`s (`parent_id` = the root) a
  `relay_connector` leg on **both concurrently** (`asyncio.gather`), polls
  `reflect.connected`, and `forward`s a reflect through the relay.
- The relay binary is parametrizable via the harness; the same suite runs the Swift
  `relayd` locally and the `relay:latest` container under the two targets.
