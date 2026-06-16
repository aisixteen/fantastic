# io_bridge — Sealed Edges & the Channel Model (current state)

Branch `kernel_auth` · greenfield (no back-compat, single author).

This is the **state-of-the-IO-layer** doc: what shipped in the io_bridge collapse,
the model it implements, and the work still deferred. The full build rationale lives
in the historical blueprint (`docs/io_bridge_blueprint.md`); this doc is the concise,
current-truth reference. Code + tests are done and green on `kernel_auth`.

---

## 1. One-line model

*The kernel is a trust domain — its INTERIOR is open, its IO EDGES are sealed.* The
interior (`kernel.send` between in-process agents) never consults a rule. Every IO
leg is a **channel** governed by a rule, and a fresh leg **denies by default**
(`resolve_ingress({})` → `DenyInbound`). Opening a leg is a conscious act; the
cli/tty is the temporary, local, unsealed root of trust through which everything
else is bootstrapped. The whole model is learnable by a readme-only LLM via
`reflect` + `readme.md`, and the seal is the forcing function:
discovery-through-denial.

The two gotchas every readme preaches: **G1** — proxy by id, never resolve a path
(content is addressed, not located; it may live on another machine). **G2** — sealed
by default, open consciously (the denial is the locked door, not a bug).

---

## 2. WHAT SHIPPED

### 2.1 The `io_bridge` base — a pure shared library (NOT an agent)

`python/bundled_agents/io/io_bridge/` — `fantastic-io-bridge`, deps `[]`, **no entry
point, never instantiated**. The merge of the former `io_core` (channel model + rule
registries) and `bridge_core` (transport-agnostic engine) into ONE shared library
that every derivation IMPORTS. (An earlier iteration registered it as a reflectable
"keystone agent" to resolve a an empty `see` (discovery via the agent's own readme + the `hint`) denial pointer; that was reverted —
promoting a library to an agent just to serve a readme was a category error. Discovery
is now handled by each derivation's OWN short readme + the denial's inline `hint`; the
`see` field is empty and the keystone-agent boot/seed machinery is gone.)

Package layout (`src/io_bridge/`):

- `tools.py` — the base agent: `make_verbs(build_transport=_memory.build_transport,
  sentence=SENTENCE, reflect_fields=_memory.reflect_fields, default_kind="memory")`
  + handler/on_delete delegating to the engine. The canonical fully-usable case is
  two `Kernel` instances in one process bridged over `memory`.
- `_engine.py` — the transport-agnostic engine (was `bridge_core/core.py`):
  `_BridgeState`, `_read_loop`, `boot`, `_teardown`, `on_delete`, the 6 verbs,
  `make_verbs`, `dispatch`, the shared `_bridges` registry, AND the unified choke
  point (§2.3).
- `_transport.py` — `_BaseTransport` contract, `ConnectionClosed`,
  `MemoryTransport.pair()` (the in-process loopback / test backbone).
- `_memory.py` — the `memory` transport's `build_transport` + `reflect_fields`
  (memory is a first-class build kind, not the old `_test_transport_inject` bypass;
  the injection slot is kept for tests that seat a specific pre-paired half).
- `channel.py` — `Channel(direction, modality, transport, rule, extractor)`,
  `CredentialExtractor` ABC, `EnvelopeExtractor` (message modality).
- `_base.py` — `Action`, `Decision`, `ALLOW`, `IngressRule`/`EgressRule` ABCs,
  `parse_spec`/`construct`/`describe`.
- `ingress_rules/` — `allow_all`, `deny_inbound`, `password` + `resolve_ingress`
  (absent ⇒ `DenyInbound`; unknown ⇒ `ValueError`).
- `egress_rules/` — `silent`, `password` + `resolve_egress` (absent ⇒ `Silent`).
- `readme.md` — **THE KEYSTONE**: the trust-domain model, G1/G2, the five-fact
  channel, the rule table, the discovery-through-denial recipe, and it names the
  shipped derivations.

### 2.2 The derivation model

Every wire transport and inbound web face is a **thin derivation** of the base: it
swaps only the **transport** (a `build_transport` seam fn, or a `get_routes` listener
mount) + the **credential extractor**, and reuses the engine + `rule.authorize`. The
single point of variance is `(transport | listener) + (transport literal) +
(CredentialExtractor binding)`; the authorization decision is identical for all.

| Derivation | Path | Direction | Transport | Extractor |
|---|---|---|---|---|
| `ws_bridge` (was `kernel_bridge`) | `io/ws_bridge/` | outbound dial | ws / ssh+ws / memory | `EnvelopeExtractor` |
| `relay_connector` | `io/relay_connector/` | outbound dial (relay router) | `relay` | `EnvelopeExtractor` |
| `web_ws` | `io/web_ws/` | inbound, 1:N | ws (listener) | `EnvelopeExtractor` |
| `web_rest` | `io/web_rest/` | inbound, 1:N | http (listener) | header (`X-Fantastic-Auth`) |

`ws_bridge` is the WS-only asymmetric client (its inbound `call` path fires only for
memory/relay, never a real WS peer). `relay_connector` (record `transport="relay"`)
dials a relay-KERNEL router (`../fantastic_relay`) at `ws://<host>/<guid>`
(subprotocol `fantastic.relay.v1`, group password in `X-Fantastic-Auth`) and tunnels
the bridge frames to a `partner_guid` — no certs/TLS/token-issuer, the relay auths
the connection and routes by `target`. (It replaced the old `cloud_bridge` zero-trust
relay; the cross-runtime any-to-any matrix moved to a live `relayd`.) The `web` host
is **unchanged** and is **not** a derivation
— it stays the render-only uvicorn host and keeps the duck-typed `get_routes` mount
seam that the inbound faces bind through.

### 2.3 The unified choke point — `gate_inbound` / `stamp_egress`

`_engine.py` ships two shared helpers that BOTH the engine read-loop and the web
inbound legs call, collapsing the two formerly near-identical gate implementations
into one extract+authorize path:

```
gate_inbound(channel, frame) -> Decision:
    token = channel.extractor.extract(frame)   # was the inline frame.get("auth_token")
    return channel.rule.authorize(Action(kind, target, verb, payload, token))

stamp_egress(channel, frame) -> frame:
    tok = channel.rule.credential()
    if tok is not None: frame["auth_token"] = tok
    return frame
```

The bridge client and its `web_ws` server peer now share ONE code-level auth path.
Wire output is byte-identical (the `{error, reason:"unauthorized", hint?, see?}`
denial reply and the egress stamp are unchanged). Gate coverage extends beyond
`call`: `watch` / `state_subscribe` / `emit` route through `gate_inbound` too, so a
sealed leg leaks neither dispatch nor telemetry (teardown verbs stay ungated).

### 2.4 The credential extractor, by modality

The extractor varies by **modality**, not by transport:

- **message** channels (ws_bridge, relay_connector, web_ws, web_rest POST) carry the
  credential on the frame **envelope** (`auth_token`, a sibling of `id`/`target` —
  never inside the dispatched payload, so the target agent never sees it), gated
  **per-frame**. Extractor = `EnvelopeExtractor`.
- **http inbound** (web_rest) reads the credential off a **request header**
  (`X-Fantastic-Auth`) — the carrier is headers, not an envelope, so the extractor
  reads the header instead. Query params are deliberately avoided (they leak into
  logs/referers).

### 2.5 Deny-all by default (the flip)

`resolve_ingress(record)` returns `DenyInbound()` when the record carries no
`ingress_rule`/`auth` (`io_bridge/ingress_rules/__init__.py`). Egress default stays
`Silent`. So **securing a leg happens by NOT setting a rule** — the seal is the
default; opening is the conscious, explicit step. The open interior is unaffected:
`kernel.send` between in-process agents never resolves a rule. `DenyInbound` denies
ALL inbound kinds (not only `call`) and every denial carries a teaching `hint` +
an empty `see` (discovery via the agent's own readme + the `hint`).

Open a leg consciously, e.g.:

```bash
fantastic <web> create_agent handler_module=web_ws.tools ingress_rule=allow_all     # local dev
fantastic <web> create_agent handler_module=web_rest.tools ingress_rule=password    # shared group
```

A freshly-created `web_ws`/`web_rest` with no rule mounts but a browser/REST client
cannot connect until it is opened.

### 2.6 Reachability + the discovery loop

`io_bridge` is a **shared abstract library, not an agent** (§2.1): every leg
(`ws_bridge`/`relay_connector`/`web_ws`/`web_rest`/`file_bridge`) DERIVES from it. So the
discovery-through-denial loop closes via **each derivation's OWN short readme** + the
denial's inline `hint` — there is no keystone agent to reflect:

```
1. try a verb over a sealed edge  → { reason:"unauthorized", hint }   # `see` is empty
2. reflect readme=true on the SEALED AGENT ITSELF → its own short readme (the open recipe)
3. set ingress_rule on the leg    → update_agent ingress_rule=password
4. present the credential          → auth_token ON THE ENVELOPE (or X-Fantastic-Auth for http)
5. the edge opens; the wire works
```

The `see` field is **empty** (the keystone-agent boot/seed machinery was reverted —
promoting a library to an agent just to serve a readme was a category error); the
denial's `hint` carries the inline open recipe.

### 2.7 main.py — the `_build_kernel` seam

`python/main.py` stays single-kernel. The structural change is a carved seam:
`_build_kernel(root_dir=Path('.fantastic')) -> Kernel` constructs + hydrates ONE
kernel. `main_dispatch` calls it once today; a future kernel-LIST launcher would call
it per root and bridge the kernels IN-PROCESS via io_bridge's now-first-class memory
transport (each seats a `MemoryTransport.pair()` half on an io_bridge agent). The
list is **not** built now; two-kernels-in-one-process is exercised in integration
tests only. The PID lock stays per-process.

---

## 3. WHAT STAYED INVARIANT (wire contracts)

The collapse kept the cross-runtime wire byte-stable (gated by `relay_e2e`, the
two-tree federation, the plain WS bridge matrix, and the Swift `ParityHarness`):

- **Bridge frame envelope** (transport-agnostic): `call`/`reply`/`error`/`event`/
  `watch`/`unwatch` shapes; `corr_id = f'{bridge_id}:{counter}'`; `auth_token` is an
  envelope sibling, never inside `payload`; a leg with no egress credential attaches
  no `auth_token` → byte-identical to pre-auth.
- **Frame codec — raw bytes, never base64** (`io_bridge._codec`, shared by web_ws +
  ws_bridge + relay_connector): a frame carrying a raw `bytes` value (a `read_stream`
  chunk; `write_stream` takes `bytes`) serializes as a **binary frame**
  `[4-byte BE header-len | JSON header (the bytes value → null + `_binary_path`) |
  raw body]`; a plain frame is UTF-8 JSON. The text/binary split is carried by the
  transport, not guessed — every transport is WS-based and uses the WS frame TYPE
  (text→`str`, binary→`bytes`); relay_connector tunnels over the relay, which
  forwards the frame kind end-to-end. The stream chunk field is `bytes` (was `b64`) — there
  is no base64 anywhere on the stream path. **Ports (#524/#525/#526) must mirror this
  codec**, else cross-runtime file streaming drifts (the relay matrix would catch it
  once a streaming case is added).
- **The 6 verbs**: `reflect`, `boot` (idempotent), `reconnect`, `forward`,
  `watch_remote`, `unwatch_remote` — same signatures across all runtimes.
- **Auth-denial wire shape**: `{error, reason:"unauthorized"}` (+ optional
  `hint`/`see`). The only intra-Python change is the `see` value `io_core` →
  `io_bridge` (rust/swift mirror on port; the cross-runtime matrix asserts only
  `reason:"unauthorized"`).
- **Record field rename — `file_agent_id` → `file_bridge_id`** (python + ts done
  2026-06-11; **rust/swift ports must mirror**, #524/#525). The meta field a bundle
  (yaml_state / scheduler / ai backends) persists THROUGH — renamed after the
  `file` → `file_bridge` bundle rename. Same with the persistence-sidecar PATHS: now
  **store-relative** (`agents/<id>/…`, not `.fantastic/agents/<id>/…`), so wired to the
  one `.fantastic` store the sidecar lands next to its `agent.json` (no `.fantastic/.fantastic/…`).
- **Reflect self-describes the IO landscape** (python done; ports mirror): each distilled
  **tree node** carries its wiring/posture meta (`ingress_rule`/`egress_rule`/`auth` —
  absent on an io leg ⇒ sealed-by-default; `root`; `file_bridge_id`); the **root** reflect
  adds `persistence: {provider:<id>|null}` (which file_bridge the loader persists through),
  via a duck-typed `reflect_root_extra(agent)` hook. So a client reads the whole gate/wiring
  state in ONE reflect — no per-agent round-trips, no kernel skip-sealed heuristic.
- **Record fields** (`.fantastic`): `transport` (memory|ws|ssh+ws|relay),
  per-leg `ingress_rule`/`egress_rule`/`auth`. `handler_module` strings:
  `relay_connector.tools`, `web.tools`, `web_ws.tools`, `web_rest.tools` stable;
  `kernel_bridge.tools` → `ws_bridge.tools` migrated in lockstep.
- **Relay** (external interface `../fantastic_relay`, a relay-KERNEL router):
  dial `ws://<host>/<guid>`, subprotocol `fantastic.relay.v1`, group password in the
  `X-Fantastic-Auth` header (checked once at the WS upgrade), routed by `target`. No
  TLS/certs/Ed25519/token-issuer — that whole zero-trust `cloud_bridge` model was
  removed when `relay_connector` replaced it.
- **Env-var names**: `FANTASTIC_GROUP_TOKEN`, `FANTASTIC_TARGET`/`FANTASTIC_IMAGE`
  — preserved. (`relay_e2e` no longer uses an opt-in `FANTASTIC_RELAY_E2E` flag; it
  self-skips on absent relay binaries.)

---

## 4. DEFERRED — `http_file` (NOT shipped)

The following is decided-later and is **not** built; do not treat it as present:

- **`http_bridge`** — an outbound-HTTP transport derivation (`build_transport(http)`
  + an http header extractor for symmetry). Not created. The keystone readme names it
  as *planned*.
- **The `/file/` octet concern (mostly SHIPPED — only the capability-URL tier is
  deferred).** The `web` host's octet route `/{agent_id}/file/{path}`
  (`bundled_agents/web/host/src/web/app.py`) is **`read_stream`-only and GATED by the
  served agent's own seal**: it pipes the file chunk-by-chunk over the served agent's
  `read_stream` (the SOURCE verb), so a **sealed `file_bridge` denies → 404** — the
  allowance IS that agent's existing ingress gate (reused, no new mechanism), path
  clamped to its root. There is NO anonymous/ungated read channel. What remains
  DEFERRED is the **minted-alias capability URL tier** — an opaque single-use token
  for one file (gate-at-open via a `StreamGrantExtractor` + a signed capability,
  distinct non-exported signing key). The channel model reserves the
  `modality = stream` slot for it.

These two are jointly "http_file," to be revisited as one later decision. Other
out-of-scope-for-now items: verb-level elevation/privileged-verb policy, the
`terminal_backend` child-env scrub, browser PoP for the message channel, and the
existing-project migration script — all tracked in the historical blueprint, none on
this doc's current-state critical path.

---

## 5. Key paths

- Base: `python/bundled_agents/io/io_bridge/src/io_bridge/`
  (`tools.py`, `_engine.py`, `_transport.py`, `_memory.py`, `channel.py`, `_base.py`,
  `ingress_rules/`, `egress_rules/`, `readme.md`).
- Derivations: `python/bundled_agents/io/{ws_bridge,relay_connector}/`,
  `python/bundled_agents/web/{web_ws,web_rest}/`.
- Render-only host (unchanged): `python/bundled_agents/web/host/`.
- Boot seam: `python/main.py` (`_build_kernel`).
- Blueprint (history/rationale): `docs/io_bridge_blueprint.md`.
