# io_bridge — IO Layer Collapse Design Blueprint

Branch `kernel_auth` · greenfield · no back-compat · single author · 2026-06-09

> **⚠️ SUPERSEDED — historical design record, NOT current state.** What shipped diverged
> from this blueprint on its two biggest open questions; for the current truth see
> [`io_bridge_spec.md`](io_bridge_spec.md). Notably: (1) **io_bridge is a pure shared
> LIBRARY, not an agent** — the "single reflectable base agent" / keystone-agent idea
> below was reverted (the `see` field is empty; discovery is via each derivation's own
> readme + the denial `hint`). (2) **Deny-all-by-default SHIPPED** — the blueprint's §6 Q7
> recommends keeping permissive AllowAll defaults; that was overridden (io legs + the fs
> edge SEAL by default). (3) `io_core`/`bridge_core` were merged and removed. Read this for
> rationale only.

This document is the build blueprint for collapsing the Fantastic kernel IO layer onto a single reflectable base agent, **io_bridge**, with all wire transports and web faces as thin derivations. It is written to the ratified decision; it does not relitigate it. Verified against the live tree on `kernel_auth` (the string `io_bridge` appears nowhere yet — this is net-new).

---

## 1. CURRENT MAP — the IO surface today

The IO layer is four cooperating Python packages plus the boot path. Three of the four are *almost* the target already; the collapse is mostly formalization plus one real refactor (the credential extractor) plus one real fix (keystone reachability).

### 1.1 io_core — the auth/channel library (entry-point-LESS)

`python/bundled_agents/io_core/` — `fantastic-io-core`, **zero runtime deps, NO `[project.entry-points."fantastic.bundles"]`** (verified). The shared substrate both the bridge family and the web faces lean on.

- `src/io_core/_base.py` — the decision surface. `Action(kind, target, verb, payload, token)` (kind ∈ `call`(gated)/`watch`/`state_subscribe`/`emit`/`open`); `Decision(allowed, reason, hint, see)`; `ALLOW = Decision(True)`; `IngressRule.authorize(action)->Decision` (inbound FILTER) + `EgressRule.credential()->str|None` (outbound DECORATOR) ABCs; `parse_spec`/`construct`/`rule_name`; `describe(record)->{ingress, egress, sealed, io_core}` (line 99; `sealed = ingress != "allow_all"`).
- `src/io_core/channel.py` — the channel model. `Direction = ingress|egress|duplex`, `Modality = message|stream`, `Transport = ws|http|cloud|memory|cli` (line 32-34); `CredentialExtractor` ABC + `EnvelopeExtractor` (reads `carrier["auth_token"]` when dict + str else None, line 47-57); frozen `Channel(direction, modality, transport, rule, extractor)` (line 59).
- `src/io_core/ingress_rules/` — `__init__.py` `REGISTRY {allow_all, deny_inbound, password}` + `resolve_ingress(record)` (absent→AllowAll, unknown→ValueError); `allow_all.py` (interior default), `deny_inbound.py` (THE SEAL — denies all kinds, `hint`+`see='io_core'`), `password.py` (envelope-token HMAC, fail-closed, non-`call` passes).
- `src/io_core/egress_rules/` — `__init__.py` `REGISTRY {silent, allow_all→Silent, deny_inbound→Silent, password}` + `resolve_egress(record)` (absent→Silent); `silent.py` (presents None), `password.py` (presents env group token).
- `src/io_core/readme.md` — **the keystone discovery doc** (trust domain, G1/G2 gotchas, the five-fact channel, the rule table, the 5-step discovery-through-denial recipe). Lives *inside the package*, not at a bundle root.

### 1.2 bridge_core — the transport-agnostic engine (entry-point-LESS)

`python/bundled_agents/bridge/bridge_core/` — `fantastic-bridge-core`, depends on `fantastic-io-core`, no entry point, **no readme of its own** (its `core.py` module docstring is its de-facto self-description).

- `src/bridge_core/core.py` — `_BridgeState` (transport, transport_kind, read_task, `pending:{corr_id:Future}`, corr_counter, cleanup, extra, ingress, egress); module dicts `_bridges` (shared across ALL bridge bundles, keyed by agent id, line 92) and `_test_transport_inject` (test-only, line 96); `_read_loop` (single inbound consumer + the **ingress choke point** at line 146-156, building `Action('call', target, payload['type'], payload, token=frame.get("auth_token"))`); `boot` (line 229, idempotent, `kind=='memory'` pops from `_test_transport_inject` at 247-249, else `build_transport`; then `resolve_ingress/egress` at 267-268); `_forward` (line ~297, **egress stamp** `frame["auth_token"]=st.egress.credential()` at 301-303); `_teardown`/`on_delete`; the 6 transport-agnostic verbs; `make_verbs(*, build_transport, sentence, reflect_fields, default_kind='ws')` (line 352) + `dispatch` (line 414).
- `src/bridge_core/_transport.py` — `_BaseTransport` contract (`send/recv/close/closed`) + `ConnectionClosed` + the in-process `MemoryTransport.pair()` (the test backbone + canonical loopback).

### 1.3 Wire transports — derivations of bridge_core

- `bridge/kernel_bridge/` — `fantastic-kernel-bridge`, entry point `kernel_bridge = kernel_bridge.tools` (verified line 16-17). ~56-line `tools.py` shim = `make_verbs(build_transport=_ws.build_transport, sentence=SENTENCE, reflect_fields=_ws.reflect_fields, default_kind="ws")` + re-exports `_bridges/_next_corr/_state/_test_transport_inject`. `_ws.py` = `WSTransport` (JSON-text, `max_size=2**24`) + ssh-tunnel helpers + `build_transport`(`ws`/`ssh+ws`) + `reflect_fields`. Pure asymmetric WS client — its inbound `call` path fires only for memory/relay, never for a real WS peer (peer is web_ws). Readme is REACHABLE; its claim that rules live in `bridge_core.ingress_rules` is STALE (they're in io_core).
- `bridge/cloud_bridge/` — `fantastic-cloud-bridge`, entry point `cloud_bridge = cloud_bridge.tools`. `tools.py` = same `make_verbs` factory (`default_kind="cloud_bridge"`) + the TransportSeam + the TokenSource seam (`_resolve_token`). `_transport.py` `CloudBridgeTransport` (dial-out WSS relay + memory-BIO TLS 1.3 mutual-auth, length-delimited `>I` frames). `_tls.py` (Ed25519 self-signed cert, pubkey-pinning `set_verify`). `_token.py` (relay CONTRACT-v1 token codec). **Gap:** `pyproject.toml` declares no direct `fantastic-io-core` dep though its tests import `io_core.ingress_rules` (transitive-only coupling). Readme is REACHABLE but says "engine defaults are permissive (back-compat)".

### 1.4 Web host + inbound faces

- `web/host/` — `fantastic-web`, entry point `web = web.tools`. `tools.py` owns the uvicorn lifecycle + the duck-typed mount seam: `_mount_surface` pulls `{routes:[{kind, path, endpoint, method?}]}` from each child via `get_routes` and calls `app.add_api_websocket_route` / `app.add_api_route` (verified line 73-76); `routes_by_child` tracks for hot unmount; **boot ordering invariant** = build app → `_mount_all_surfaces` → `_start_serving` (port never accepts before its full route table exists). `app.py` bakes ONLY render/static routes (`/`, favicon, `/{agent_id}/file/{path}`). The host imports NO io_core and does NO auth.
- `web/web_ws/` — `fantastic-web-ws`, child of `web`. `tools.py` declares `get_routes -> {kind:'websocket', path:'/{host_id}/ws', endpoint}` (verified) and surfaces `io_core.describe(...)` in `reflect`. `_proxy.py` owns the frame codec (text + the 4-byte-BE-prefixed binary format) and the **per-frame ingress gate** `_gate()` = `resolve_ingress(kernel.get(web_agent_id)).authorize(Action(kind, addr, verb, inner, token=EnvelopeExtractor().extract(frame)))`. Gates `call`/`emit`/`watch`/`state_subscribe`; teaching denial `{error, reason:'unauthorized', hint, see:'io_core'}`. THE live discovery-through-denial choke point.
- `web/web_rest/` — `fantastic-web-rest`, child of `web`. `get_routes` → `POST /<self>/{target}` + two `GET /_reflect` shortcuts. **UNGATED today** — no io_core import, no gate, no auth posture in reflect.

### 1.5 Boot path

`python/main.py` `main_dispatch()` = `_load_dotenv()` → `Kernel()` (ONE instance, line 52) → `_bootstrap(kernel)` (`read_tree('.fantastic')` → `kernel.load(records)` → compose `Cli` if tty) → `atexit` → `asyncio.run(dispatch_argv(kernel, argv))`. `kernel/modes/longrun.py` `_default` blocks if a web agent is persisted or tty; `_boot_all_agents` sends `{type:'boot'}` per agent. `kernel/_lock.py` = ONE PID lock per *process* (not per kernel). No transport, no bridge, no auth is constructed in main.py — a single in-process kernel is one open trust domain.

### 1.6 Constraints that frame everything

- **Reserved verbs** (`kernel/_agent.py:62` `_SYSTEM_VERBS`): `create_agent, delete_agent, update_agent, list_agents, shutdown_kernel`; plus `get` is reserved (returns the agent record), and `reflect`/`boot`/`shutdown` are native. A read verb must be `read`, never `get`.
- **Root-id asymmetry** (BY DESIGN): python root id = `kernel_state`; rust/swift = `core`; ts its own. The `kernel` alias resolves to root for dispatch (`call`/`forward`); `watch`/`emit` need the literal id.
- **Readme reachability mechanism** (verified): `kernel_state._seed_readme` copies `importlib.resources.files(handler_module.split('.')[0]) / "readme.md"` into the agent dir on first persist; `Agent._read_readme` returns `<self._root_path>/readme.md` on `reflect readme=true`. **An entry-point-LESS library is never any record's `handler_module` top package → its readme is never seeded → never wire-reachable.** This is why `see:'io_core'` dangles today.

---

## 2. TARGET ARCHITECTURE

### 2.0 The shape in one sentence

`io_bridge` is a **real, reflectable bundle agent** whose package is the merge of today's `io_core` + `bridge_core`: it ships the channel model, the rule registries, the keystone readme, the transport-agnostic engine, AND the `MemoryTransport`; its canonical fully-usable case is two `Kernel` instances in one process bridged over memory; every wire transport and web face is a thin derivation that swaps only the transport + the credential extractor and reuses `rule.authorize`.

### 2.1 Module / package layout

One new package, three recast bundles, one stable host.

```
python/bundled_agents/io/
  io_bridge/                         # NEW — the BASE bundle (entry point: io_bridge = io_bridge.tools)
    pyproject.toml                   # fantastic-io-bridge; deps = []  (NO websockets/fastapi — base stays dependency-free)
    src/io_bridge/
      __init__.py                    # public surface re-export (the portable API)
      readme.md                      # THE KEYSTONE (was io_core/readme.md) — now wire-reachable via this agent
      tools.py                       # the base agent: make_verbs(build_transport=_memory.build_transport,
                                      #   sentence=SENTENCE, reflect_fields=_memory.reflect_fields, default_kind="memory")
                                      #   + handler/on_delete delegating to engine
      _engine.py                     # was bridge_core/core.py — _BridgeState, _read_loop, boot, _teardown,
                                      #   the 6 verbs, make_verbs, dispatch, _bridges, the gate/forward HELPERS (§2.6)
      _transport.py                  # was bridge_core/_transport.py — _BaseTransport, ConnectionClosed, MemoryTransport.pair()
      _memory.py                     # NEW — the memory transport's build_transport + reflect_fields (§2.7: memory becomes
                                      #   a first-class build_transport kind, not the _test_transport_inject bypass)
      channel.py                     # was io_core/channel.py — Channel, CredentialExtractor, EnvelopeExtractor
      _base.py                       # was io_core/_base.py — Action, Decision, rule ABCs, parse_spec/construct/describe
      ingress_rules/                 # was io_core/ingress_rules/ — allow_all, deny_inbound, password + resolve_ingress
      egress_rules/                  # was io_core/egress_rules/ — silent, password + resolve_egress

  ws_bridge/                         # was kernel_bridge (RENAMED)
    pyproject.toml                   # fantastic-ws-bridge; deps = [fantastic-io-bridge, websockets>=12,<17]
    src/ws_bridge/
      tools.py                       # ~50-line shim: make_verbs(build_transport=_ws.build_transport, default_kind="ws")
      _ws.py                         # WSTransport + ssh tunnel + build_transport(ws|ssh+ws) + reflect_fields
      readme.md                      # THIN — points back at io_bridge keystone

  http_bridge/                       # NEW outbound-HTTP derivation (folds in web_rest's transport concern; §2.8)
    src/http_bridge/...              # build_transport(http) + an HttpHeaderExtractor for inbound; thin

  cloud_bridge/                      # KEEPS its name (cross-runtime wire + relay CONTRACT depend on it)
    pyproject.toml                   # fantastic-cloud-bridge; deps = [fantastic-io-bridge, websockets, cryptography, pyopenssl]
                                      #   ^ EXPLICIT io dep now (fixes the transitive-only fragility)
    src/cloud_bridge/                # CloudBridgeTransport, _tls.py, _token.py, TokenSource seam, _derive_role, reflect_fields

python/bundled_agents/web/
  host/                              # UNCHANGED in role — render-only uvicorn host; keeps get_routes mount seam
  web_ws/  -> recast as the WS INBOUND derivation (1:N listener leg)   (§2.8)
  web_rest/-> recast as the HTTP INBOUND derivation (1:N listener leg) (§2.8)
```

**Why `io/` not `bridge/`:** the base now spans both the engine and the channel model, and the web faces are derivations of it too. The directory name signals "the IO layer," not "the cross-kernel bridge."

### 2.2 Class / protocol hierarchy

```
io_bridge (base bundle agent)
├── Engine (transport-agnostic)            [_engine.py — was bridge_core.core]
│     _BridgeState · _read_loop · boot · _teardown · on_delete
│     make_verbs(build_transport, sentence, reflect_fields, default_kind) -> 6 verbs
│     dispatch(verbs, id, payload, kernel)
│     gate_inbound(channel, frame) -> Decision           [NEW shared helper, §2.6]
│     stamp_egress(channel, frame) -> frame              [NEW shared helper, §2.6]
│
├── Channel model                          [channel.py]
│     Channel(direction, modality, transport, rule, extractor)
│     CredentialExtractor (ABC) ── EnvelopeExtractor (message)
│                                ── HttpHeaderExtractor (http inbound, §2.8) [NEW]
│                                ── StreamGrantExtractor (stream, gate-at-open) [DEFERRED, §6]
│
├── Decision surface                       [_base.py]
│     Action · Decision · ALLOW · IngressRule(ABC) · EgressRule(ABC) · parse_spec · describe
│
├── Rule registries                        [ingress_rules/ · egress_rules/]
│     IngressRule: AllowAll · DenyInbound · Password
│     EgressRule:  Silent · Password
│
└── Transport contract + reference impl    [_transport.py · _memory.py]
      _BaseTransport(ABC) · ConnectionClosed · MemoryTransport.pair()

Derivations (each = TransportSeam + ExtractorBinding, REUSE the engine + rules):
  ws_bridge      : build_transport(ws|ssh+ws),  modality=message, extractor=EnvelopeExtractor   [OUTBOUND dial]
  http_bridge    : build_transport(http),         modality=message, extractor=EnvelopeExtractor   [OUTBOUND dial]
  cloud_bridge   : build_transport(cloud),        modality=message, extractor=EnvelopeExtractor   [OUTBOUND dial, +TLS]
  web_ws (inbound): listener leg via get_routes,   modality=message, extractor=EnvelopeExtractor   [INBOUND, 1:N]
  web_rest(inbound):listener leg via get_routes,   modality=message, extractor=HttpHeaderExtractor [INBOUND, 1:N]
```

**The single point of variance** across all derivations: `(build_transport | listener mount)` + `(transport literal)` + `(CredentialExtractor binding)`. The `rule.authorize` decision is identical for all of them — extractor varies by modality (message = `auth_token` on the envelope, gated per-frame; stream/octet = gate at open).

### 2.3 What lives in the BASE vs each DERIVATION

| Concern | BASE (io_bridge) | DERIVATION |
|---|---|---|
| Engine (`_read_loop`, `boot`, `_teardown`, 6 verbs, `dispatch`, `_bridges`) | ✅ | — |
| `make_verbs` factory + the build_transport seam | ✅ (factory) | supplies the seam fn |
| Decision surface, rule ABCs, rule registries (`allow_all/deny_inbound/password/silent`) | ✅ | — (reused) |
| Channel model + `EnvelopeExtractor` | ✅ | supplies which extractor it binds |
| `MemoryTransport` + `_BaseTransport` contract | ✅ (canonical loopback) | implements the contract |
| Shared `gate_inbound`/`stamp_egress` helpers (§2.6) | ✅ | — (called) |
| `WSTransport` + ssh tunnel | — | ws_bridge |
| `CloudBridgeTransport` + `_tls` + `_token` + TokenSource | — | cloud_bridge |
| `HttpHeaderExtractor` + HTTP token-placement convention | ✅ (extractor in base) | http_bridge/web_rest bind it |
| WS frame codec (text + binary `_binary_path`), state-stream bridge, call-as-cancellable-task | — | web_ws (inbound-ws derivation) |
| `get_routes` route specs + `sender_context` tagging | — | web_ws/web_rest |
| uvicorn lifecycle, render/static routes, mount/unmount registry | — | `web` host (NOT a derivation; stays as-is) |

### 2.4 Bundle/agent naming — with reserved-verb + root-id constraints

| Today | Target | Entry point / handler_module | Notes |
|---|---|---|---|
| `io_core` (lib, no entry point) | **`io_bridge`** (bundle) | `io_bridge = io_bridge.tools` | NEW: the base becomes a reflectable agent. This is the keystone fix (§2.9). |
| `bridge_core` (lib) | merged into `io_bridge` engine | (none) | engine moves; no separate bundle. |
| `kernel_bridge` | **`ws_bridge`** | `ws_bridge = ws_bridge.tools` | **wire/record-visible rename** — `handler_module='kernel_bridge.tools'` is persisted in `.fantastic` records, in `integration_tests/helpers/seeding.py`, in TS/itest fixtures, and asserted across runtimes. Migrate in lockstep (§3 step 6). |
| `cloud_bridge` | **`cloud_bridge`** (unchanged) | `cloud_bridge = cloud_bridge.tools` | keep — the relay CONTRACT, the `__cloud-cert` subcommand, and the cross-runtime any-to-any matrix pin this name. |
| — | **`http_bridge`** (NEW) | `http_bridge = http_bridge.tools` | outbound HTTP derivation; absorbs the `/file/` octet route + REST dial concern. |
| `web` host | `web` (unchanged) | `web = web.tools` | stays the render-only listener host. |
| `web_ws` | `web_ws` (recast as ws INBOUND derivation of io_bridge) | `web_ws = web_ws.tools` | keep the name (the cross-runtime tests assert `bundles` includes `web_ws`); change its *internals* to derive from the base. |
| `web_rest` | `web_rest` (recast as http INBOUND derivation) | `web_rest = web_rest.tools` | keep the name; gains an ingress gate (§2.8). |

**Reserved-verb constraint:** the io_bridge agent's verb table is exactly the 6 from `make_verbs` (`reflect, boot, reconnect, forward, watch_remote, unwatch_remote`). None collide with `_SYSTEM_VERBS` or `get`. Any new verb (e.g. a future stream-grant verb) MUST avoid `{create_agent, delete_agent, update_agent, list_agents, shutdown_kernel, get}`.

**Root-id constraint:** the base must NOT hardcode a root id. `forward(target=...)` and the `kernel` alias remain the dispatch path; nothing in io_bridge may assume `kernel_state` vs `core`. Keep `root_id()`-style discovery for any internal need.

**Transport-name reconciliation:** the engine keys transports by the record's `transport` string while `channel.Transport` uses the shorter literal. Reconcile: the cloud derivation's record `transport` value should be `cloud` (matching the channel literal), not `cloud_bridge`. **This is wire/cross-runtime-visible** (`_cb_meta` and rust/swift read `transport="cloud_bridge"`) → either migrate all runtimes in lockstep or keep `cloud_bridge` as the record value and map it to the `cloud` channel literal internally. **Recommendation:** keep `transport="cloud_bridge"` in the record (least churn, cross-runtime-safe) and treat the channel literal as a derived view. (Open question §6.)

### 2.5 Channel / extractor wiring per modality

Per the ratified decision, the extractor varies by **modality**:

- **message channel** (ws_bridge, http_bridge, cloud_bridge, web_ws, web_rest-POST): credential on the **frame envelope** (`auth_token`), gated **per frame**. Extractor = `EnvelopeExtractor` for the WS/cloud frame protocol; `HttpHeaderExtractor` (NEW) for inbound HTTP where the carrier is request headers (token placement = a header, e.g. `Authorization`/`X-Fantastic-Auth`; convention TBD §6). The engine builds `Action(kind, target, verb, payload, token=channel.extractor.extract(carrier))` and calls `channel.rule.authorize(action)`.
- **stream / octet channel** (the `/file/` proxy, future octet endpoints): gate **at the open**, not per frame. Extractor = `StreamGrantExtractor` (DEFERRED §6) — pulls a grant from the open handshake, authorizes once, then the byte stream flows ungated. Today `/file/` is unguarded read-only static; under the base it becomes an http_bridge octet channel whose default rule keeps it open (back-compat) until a stream rule is configured.

Per-leg record fields are unchanged and now uniform across every derivation: `ingress_rule` / `egress_rule` (spec = bare name or `{type, env}`) / `auth` shorthand (sets both). Resolution via `resolve_ingress`/`resolve_egress` from the base registries.

### 2.6 The one real refactor: unify the choke point

Today there are **two near-identical gate implementations** — `bridge_core._read_loop` (hardcodes `frame.get("auth_token")`, line 152) and `web_ws._proxy._gate` (uses `EnvelopeExtractor`). The base introduces two shared helpers in `_engine.py` that BOTH the engine read-loop and the web inbound legs call:

```
gate_inbound(channel, frame) -> Decision:
    token = channel.extractor.extract(frame)          # was: frame.get("auth_token") inline
    return channel.rule.authorize(Action(channel-derived kind, target, verb, payload, token))

stamp_egress(channel, frame) -> frame:
    tok = channel.rule.credential()                   # egress rule
    if tok is not None: frame["auth_token"] = tok
    return frame
```

The engine's `_read_loop` constructs an ingress `Channel{direction:ingress, modality:message, transport:<kind>, rule:st.ingress, extractor:EnvelopeExtractor()}` once at boot and routes every inbound frame through `gate_inbound`. This makes the bridge client and its web_ws server peer share ONE extract+authorize path — auth-symmetric at the code level, not just the wire. **Wire output is byte-identical** (the denial reply `{error, reason:'unauthorized', hint?, see?}` and the egress stamp are unchanged).

**Gate-coverage decision (the seal-completeness fix):** today the engine routes only `kind=='call'` through `authorize`; `watch`/`unwatch`/`event` are ungated, so a `deny_inbound` leg still silently processes inbound watch/event frames even though `DenyInbound` is documented as sealing all kinds. The base SHOULD route `watch`/`state_subscribe`/`emit` through `gate_inbound` too (matching web_ws, which already gates them and leaks no telemetry on a sealed leg). `unwatch`/`state_unsubscribe` stay ungated (teardown only). **This is a behavior change for memory/relay inbound legs** — gated by the bridge_core tests (§3 step 4, flagged risky).

### 2.7 Memory transport: first-class, not test-injected

Today `transport=='memory'` bypasses `build_transport` and pops from `_test_transport_inject`. The base makes memory a **first-class** `build_transport` kind via `_memory.py`, because the canonical fully-usable case (two Kernel in one process) needs a real, non-test construction path. The `_test_transport_inject` slot is **kept** for unit tests that need to seat a specific pre-paired half (the existing 20-test suite + cloud_bridge tests depend on it), but `boot` first checks the injection slot, then falls back to `build_transport(kind='memory')` which can mint a pair on demand from a base-owned registry. This is the one bridge_core gap the kernel-list upgrade needs closed (§2.10) and is closed now in the base.

### 2.8 How web_ws / web_rest fold in; what happens to get_routes

The web faces become the **inbound, 1:N listener legs** of io_bridge. Three load-bearing facts preserved:

1. **The `web` host stays the listener host and the `get_routes` mount seam is unchanged.** The host still pulls `{routes:[{kind, path, endpoint, method?}]}` from each child and binds via `add_api_websocket_route`/`add_api_route`; the boot ordering invariant (mount-all-then-serve) survives; `routes_by_child` hot-unmount survives. The collapse consolidates the *implementation behind the endpoint*, not the mount contract. **`web` does not become a derivation** — its render/static routes (`/`, favicon, `/{id}/file/{path}`) have no bridge analog and are the least-collapsible part.
2. **web_ws** keeps its WS-specific code (the text/binary frame codec, the `_binary_path` scheme, the state-stream bridge, the call-as-cancellable-task disconnect discipline) — these are ws-inbound concerns, NOT hoisted to the base. What moves to the base: the per-frame gate (now `gate_inbound` with the `EnvelopeExtractor` binding) and the `sender_context` tagging pattern. web_ws's gate reads the rule off its **own leg record** — preserved (each inbound face = its own channel/leg with its own `ingress_rule`; many faces coexist under one `web` with independent posture).
3. **web_rest** gains an ingress gate where it has none — the http INBOUND derivation. It binds `HttpHeaderExtractor` and routes its POST (and the two GET shortcuts) through `gate_inbound`, and surfaces `describe(record)` in reflect. **Default MUST stay AllowAll/inert** to preserve today's open REST behavior (`web_rest/tests/test_web_rest.py` POSTs without auth). Net-new: the `HttpHeaderExtractor` + an HTTP token-placement convention (§6).

The standalone `get_routes` mounting is therefore **retained verbatim** as the boundary between the listener host and the inbound derivations — it is the clean seam that lets the faces collapse without the host changing.

### 2.9 README / discovery hierarchy — and the keystone fix

**The headline behavioral gain.** Today `see:'io_core'` (emitted by every `deny_inbound` Decision and every leg's reflect descriptor) dangles: `io_core` is entry-point-LESS, so its readme is never seeded and `reflect readme=true io_core` resolves to `{error:'no agent io_core'}`. The discovery-through-denial loop dead-ends at step 2.

**Fix:** because `io_bridge` is now a **real bundle with an entry point**, a client (or LLM) can create/address an `io_bridge` agent and the recipe `reflect readme=true` works verbatim — `kernel_state._seed_readme` seeds `io_bridge/readme.md` (the keystone) into the agent dir, and `Agent._read_readme` serves it. The denial pointer should be updated to `see:'io_bridge'` (and `describe()`'s `io_core` field renamed to `io_bridge`). **Recommendation:** rename the pointer to `io_bridge` everywhere; this is the cleanest closure of the loop and matches "io_bridge ships the keystone."

The hierarchy:

- **`io_bridge/readme.md` = THE KEYSTONE** (the relocated `io_core/readme.md`). Teaches the trust-domain model, G1 (proxy-by-id, never resolve a path) + G2 (sealed-by-default, open consciously), the five-fact channel, the rule registry table, the 5-step discovery-through-denial recipe, AND now NAMES the transport-impl derivations (`ws_bridge`, `http_bridge`, `cloud_bridge`, the inbound web faces) so an LLM can discover them from the base. It also documents the canonical case (two-kernels-in-one-process over memory).
- **Derivation readmes go THIN** and point back at the keystone: `ws_bridge` (WS-only asymmetric client + transports memory/ws/ssh+ws → "auth: see io_bridge"), `cloud_bridge` (relay + TLS + TokenSource → "dispatch auth: see io_bridge"), `web_ws`/`web_rest` (inbound listener legs → "auth: see io_bridge"). Drop every restatement of the rule model; drop the stale "rules live in bridge_core" and "engine defaults permissive" lines.
- **`see` points at `io_bridge`.** Every `deny_inbound` Decision's `see` field + every leg reflect descriptor's pointer field name → `io_bridge`.
- **`reflect` surfaces the keystone** verbatim: `reflect readme=true` on an io_bridge agent returns the keystone; the leg descriptor `describe()` returns `{ingress, egress, sealed, io_bridge}` so a denied client follows `io_bridge` → `reflect readme=true` → learns to set `ingress_rule` → presents `auth_token` on the envelope. The loop closes.
- **Root `kernel_state/readme.md`** gains a short channel-model preamble + a pointer to the keystone, so the model is discoverable from the protocol root, not only on denial.

### 2.10 main.py: single-kernel now, kernel-list-ready

main.py STAYS single-kernel. The only structural change is to carve the seam (do NOT build the list):

```
def _build_kernel(root_dir=Path('.fantastic')) -> Kernel:
    k = Kernel(); _bootstrap(k); return k

# today:  kernel = _build_kernel()
# future: kernels = [_build_kernel(d) for d in roots]   (NOT built now)
```

`dispatch_argv(kernel, argv)` keeps its single-kernel signature; a future kernel-list adds a thin `dispatch_argv_multi(kernels, argv)` wrapper that picks a primary. The PID lock stays per-process (one lock for the whole process regardless of kernel count — already correct). `atexit`/`shutdown` may store the kernel in a one-element holder so the closure iterates rather than references a single name (cheap; optional). The in-process peering primitive is `io_bridge`'s now-first-class memory transport (§2.7): a kernel-list mints `MemoryTransport.pair()` and seats each half on an `io_bridge` agent with `transport='memory'` in each kernel. **Two-kernels-in-one-process is exercised in INTEGRATION TESTS only**, not in main.py.

---

## 3. MIGRATION PLAN — build the base, then iteratively recast each derivation

Ordered, minimal-churn, cross-kernel wire shape kept byte-stable throughout. Each step lists what changes, files, and which tests must stay green. **🔴 = riskiest.**

**Step 0 — branch hygiene.** Already on `kernel_auth`. Snapshot the green test baseline: `io_core/tests/`, `bridge_core/tests/`, `kernel_bridge/tests/`, `cloud_bridge/tests/`, `web_ws/tests/`, `web_rest/tests/`, `web/host/tests/`. These are the regression gate for every subsequent step.

**Step 1 — create the `io_bridge` package skeleton (no behavior change).** Move `io_core/*` (channel, _base, ingress_rules, egress_rules, readme.md) and `bridge_core/{core.py→_engine.py, _transport.py}` into `io/io_bridge/src/io_bridge/`. Add `pyproject.toml` (`fantastic-io-bridge`, deps `[]`, entry point `io_bridge = io_bridge.tools`). Add `tools.py` (the base agent shim) + `_memory.py` (memory build_transport + reflect_fields). Keep the public API names identical (`Action`, `Decision`, `IngressRule`, `EgressRule`, `resolve_ingress`, `resolve_egress`, `Channel`, `CredentialExtractor`, `EnvelopeExtractor`, `parse_spec`, `describe`, `make_verbs`, `dispatch`, `MemoryTransport`, `_bridges`, `_test_transport_inject`). **Tests:** the moved `io_core/tests/` + `bridge_core/tests/` run against the new import path (`from io_bridge import ...`); rewrite their imports only. Root workspace `pyproject.toml` member list updated.

**Step 2 — unify the choke point via shared helpers (the one real refactor).** Add `gate_inbound`/`stamp_egress` to `_engine.py`; route `_read_loop` through `gate_inbound` with an `EnvelopeExtractor`-bound ingress Channel (replaces the inline `frame.get("auth_token")` at line 152). **Wire output byte-identical.** **Tests green:** `bridge_core` engine tests (denial reply shape, egress stamp), `io_core` channel/rules tests.

**Step 3 🔴 — make `io_bridge` reachable + fix the keystone pointer.** Relocate the keystone readme into the package; rename the `see`/descriptor field from `io_core` to `io_bridge` in `deny_inbound.py` and `describe()`. **Tests:** `web_ws/tests/test_gate.py` asserts `see=='io_core'` literally → update to `io_bridge`; same for any rule test asserting `see`. **Risk:** this is a wire-shape change to the denial envelope's `see`/descriptor value. It is intra-Python-only (the cross-runtime tests assert `reason:'unauthorized'`, not `see`), but rust/swift must mirror the rename when they port (§ parity). Verify the seed→reflect loop end-to-end with a throwaway `io_bridge` agent.

**Step 4 🔴 — extend gate coverage to watch/emit/state_subscribe on inbound legs.** Route those kinds through `gate_inbound` in `_engine.py` (matching web_ws). **Behavior change** for memory/relay inbound legs: a `deny_inbound` leg now refuses inbound watch/event, not just call. **Tests:** `bridge_core`/`kernel_bridge` watch/event framing tests must be re-baselined for sealed legs; `web_ws` already gates these. **Risk:** this is the seal-completeness fix; it changes what a sealed memory/relay leg processes. Land it behind the base so all derivations inherit it uniformly, then re-baseline the affected assertions.

**Step 5 — recast `cloud_bridge` onto the base (no rename).** Repoint imports from `bridge_core`/`io_core` to `io_bridge`; add the explicit `fantastic-io-bridge` dep (fixes the transitive-only fragility). Keep `transport="cloud_bridge"` record value (cross-runtime-safe). **Tests green:** `cloud_bridge/tests/test_cloud_bridge.py` (token codec, mTLS over FakeWS, pubkey-pin, TokenSource seam, engine-forward over memory). **Keep byte-stable:** the relay subprotocol `fantastic.relay.v1`, the `>I` frame framing, the `__cloud-cert` subcommand, the `_cb_meta` field names — the relay repo + relay_e2e matrix are external frozen interfaces.

**Step 6 🔴 — rename `kernel_bridge` → `ws_bridge`.** Rename package, entry point (`ws_bridge = ws_bridge.tools`), and the persisted `handler_module='kernel_bridge.tools'` string. **Files in lockstep:** `ws_bridge/pyproject.toml`, `tools.py`, `_ws.py`, `readme.md`; `integration_tests/helpers/seeding.py` (`seed_bridge_ws`); `integration_tests/py_ts/bridge.itest.ts` + `two_tree.itest.ts`; `integration_tests/relay_e2e/test_relay_matrix.py` `_HANDLER_MODULE` map; any `.fantastic` fixture records; CLAUDE.md mentions; **and the rust/swift `kernel_bridge` mirrors** (their `handler_module` is `kernel_bridge.tools` even for cloud on those runtimes). **Risk: this is the most wire/record-visible change in the whole plan.** Either keep `kernel_bridge.tools` as an alias for one release, or migrate every seed site + cross-runtime map atomically. The plain WS bridge matrix (`integration_tests/bridge/test_bridge_*_ws.py`) and the relay matrix are the gates. **Recommendation:** alias `kernel_bridge.tools` → `ws_bridge.tools` at the bundle resolver for one release to decouple the record migration from the code rename.

**Step 7 — recast `web_ws` as the ws INBOUND derivation.** Repoint to `io_bridge`; replace `_proxy._gate`'s inline construction with the base `gate_inbound(channel-with-EnvelopeExtractor, frame)`; keep the WS frame codec, state-stream, and cancellation discipline local. **Tests green:** `web_ws/tests/{test_gate,test_binary_protocol,test_proxy,test_state_subscribe,test_web_ws}.py`; the `get_routes` shape unchanged so the `web` host needs no edit.

**Step 8 🔴 — recast `web_rest` as the http INBOUND derivation (adds a gate where none exists).** Add the `fantastic-io-bridge` dep, the `HttpHeaderExtractor`, the gate on POST + the GET shortcuts, and `describe()` in reflect. **Default MUST stay AllowAll/inert.** **Tests green:** `web_rest/tests/test_web_rest.py` (POST without auth must still succeed). **Risk:** collapsing onto a base that could be sealed-by-default would silently seal the REST diagnostic surface — keep the permissive default and add an explicit `allow_all` opt-in path. New `HttpHeaderExtractor` needs its own unit test + an HTTP token-placement convention decision (§6).

**Step 9 — introduce `http_bridge` (outbound HTTP derivation) + fold the `/file/` octet route.** NEW bundle; thin. **Lowest priority** of the derivations; can land after the renames settle. **Tests:** new `http_bridge` round-trip over memory + the `web/host/tests/test_app.py` file-proxy tests stay green (the `/file/` route keeps its open default until a stream rule is set).

**Step 10 — main.py seam (§2.10) + docs.** Carve `_build_kernel`; do NOT build the list. Rewrite `docs/io_core_spec.md` (rename to `io_bridge_spec.md`) from "plan" to "state + remaining work"; reconcile the reflect-field naming drift (spec §6.1 `{direction,sealed,channels,io_core_readme}` vs shipped `{ingress,egress,sealed,io_bridge}` — pick the shipped names, update the conformance fixture); add the channel vocabulary to `python/CLAUDE.md`. **Tests:** the Swift `ParityHarness` byte-diff stays green (no reflect identity/order change); add a kernel-list-over-memory integration test (two `Kernel`, one process, bridged via the now-first-class memory transport) as the *only* exercise of the future upgrade.

**Riskiest steps, ranked:** Step 6 (wire/record-visible rename, cross-runtime) > Step 4 (seal-completeness behavior change) > Step 8 (adds a gate to a currently-open surface) > Step 3 (denial `see` pointer rename).

---

## 4. WIRE-SHAPE CONTRACTS TO PRESERVE

Invariants the collapse must NOT break. Gated by `relay_e2e` (any-to-any python/rust/swift), the two-tree federation, the plain WS bridge matrix, and the Swift `ParityHarness` (which byte-diffs including **key insertion order**).

**Bridge frame envelope (transport-agnostic):**
- call = `{type:'call', id:<corr>, target, payload, auth_token?}`
- reply = `{type:'reply', id:<corr>, data}`
- error = `{type:'error', id, error}`
- event = `{type:'event', payload}`
- watch = `{type:'watch', src}` · unwatch = `{type:'unwatch', src}`
- `corr_id = f'{bridge_id}:{counter}'`, echoed as `frame.id`
- **`auth_token` is an ENVELOPE sibling of id/target — NEVER inside `payload`** (the target agent never sees it). `EnvelopeExtractor` enforces this (asserted in `test_channel.py`).
- A leg with no egress credential attaches NO `auth_token` field → wire byte-identical to pre-auth.

**Verb signatures (all 6, all runtimes):** `reflect`; `boot` (idempotent, `{already:true}` if connected); `reconnect` (teardown+boot, no auto-reconnect); `forward(target, payload, timeout=30)` → unwrapped reply `data`; `watch_remote(target)` → `{ok:true, watching:<target>}`; `unwatch_remote(target)` → `{ok:true, unwatched:<target>}`.

**Auth-denial wire shape (cross-runtime invariant):** a refused inbound call returns `{error:<reason>, reason:'unauthorized'}` (+ optional `hint`/`see` for discovery). Byte-identical for `deny_inbound`, password-mismatch, and ingress-deny across every py/rust/swift pair. The `see` value changes `io_core`→`io_bridge` (intra-Python wire change; rust/swift mirror on port).

**Reflect output:** identity = `{id, sentence, parent_id, handler_module, display_name, description?, ...flat meta}`; flags append `tree`/`bundles`/`readme`. **Root-only** `{runtime, env, version}` in that exact key order. Bridge reflect adds `{transport, connected, pending_count, ingress, egress, auth(=ingress name), ...reflect_fields, verbs, emits}`. cloud_bridge adds `{relay_url, tenant_id, peer_id, rendezvous, partner_peer_id, verified_partner}`. **No secrets ever surfaced** (env var names, id_key, token, password). Leg descriptor `describe()` = `{ingress, egress, sealed, io_bridge}` (field renamed from `io_core`).

**Record fields (persisted in `.fantastic`):** `transport` (memory|ws|ssh+ws|cloud_bridge), `peer_id`, `host`, `local_port`, `remote_port`; cloud: `relay_url, tenant_id, peer_id, partner_peer_id, rendezvous, id_key, approved_peer_certs, issue_url, provider, password, tls_role, heartbeat`. Auth: `ingress_rule`, `egress_rule`, `auth` shorthand; spec normalization `env→token_env`, `policy→type`. `handler_module` strings: keep `cloud_bridge.tools`, `web.tools`, `web_ws.tools`, `web_rest.tools` stable; `kernel_bridge.tools`→`ws_bridge.tools` migrated in lockstep (Step 6).

**Relay / TLS (external frozen interface — `../fantastic_relay` CONTRACT v1):** subprotocol `fantastic.relay.v1`; claims `{tenant_id, peer_id, rendezvous, partner_peer_id, aud='fantastic.relay', iat, nbf, exp, jti}` with `exp-iat ≤ 60s`; pairing by `(tenant_id, rendezvous)`; opaque-frame forwarding; peer↔peer TLS 1.3 mutual-auth pinned by **Ed25519 PUBLIC KEY**; `>I` length-prefixed frames, `MAX_FRAME=16MiB`; `__cloud-cert <id_key>` subcommand. The collapse must not touch any of these (the relay repo is versioned independently).

**WS endpoint + dispatch:** path `ws://<host>:<port>/<peer_id>/ws`; `kernel` alias resolves to root for `call`/`forward`; `watch`/`emit` use the literal root id. Root readme byte-identical across runtimes, prefix `# This is a Fantastic kernel.`

**Two-tree federation:** on-disk `.fantastic/web/agents/<canvas>/agents/<child>/agent.json` with `{id, handler_module, display_name, meta}`; `web_loader` proxy contract; `load_tree`→`kernel.load` rehydrates `handler_module`+`meta` byte-for-byte.

**Env-var names:** `FANTASTIC_GROUP_TOKEN` (egress credential), `FANTASTIC_RELAY_E2E` (opt-in), `FANTASTIC_TARGET`/`FANTASTIC_IMAGE` — preserve all.

---

## 5. EMERGENCE IMPACT

**The keystone is now reflectable — the discovery-through-denial loop closes.** Today the dual-LLM validation has a dead second hop: a readme-only agent that hits a sealed edge gets `{reason:'unauthorized', see:'io_core'}`, follows the pointer with `reflect readme=true io_core`, and receives `{error:'no agent io_core'}`. With `io_bridge` a real bundle agent, the recipe works verbatim: the LLM creates/addresses an `io_bridge` agent, `reflect readme=true` serves the keystone (seeded by `kernel_state._seed_readme` because `io_bridge` IS a record's `handler_module` top package), and the recipe (try → denial+see → reflect readme=true → set `ingress_rule` → present `auth_token` on the envelope) completes end-to-end. **This is the single biggest behavioral gain.**

**The seal still lives on the inbound legs.** The collapse does NOT move the gate to the base in a way that centralizes it — each inbound leg (web_ws, web_rest, memory/relay bridge inbound) keeps its OWN per-leg `ingress_rule` resolved off its OWN record. The base ships the *mechanism* (`gate_inbound` + the rules + the extractor); the *posture* stays per-leg. A sealed leg refuses dispatch AND (after Step 4) telemetry, and teaches. Discovery-through-denial validation continues to test exactly this: present a sealed edge, confirm the LLM is taught the door, confirm it opens it consciously (G2).

**What the bare-host emergence harness needs:**
- The `e2e/` readme-only builder agent must be able to follow `see:'io_bridge'` → it needs the `io_bridge` bundle installed in the host so an `io_bridge` agent is creatable and its readme seedable. (Today it would dead-end.)
- The keystone readme must NAME the derivations (`ws_bridge`, `http_bridge`, `cloud_bridge`, the inbound web faces) so an LLM weaving cross-kernel wiring discovers the transport agents from the base — supporting the north-star (LLM weaves durable cross-kernel agent wiring from the system's own send/reflect + self-description).
- The root `kernel_state/readme.md` channel-model preamble (Step 10) lets the harness discover the seal model from the protocol root, not only on denial — strengthening the readme-only-agent path.
- Keep the validation driven from a **spawned readme-only sub-agent** (curl/CLI), not the in-kernel paid LLM backend (per the established cost discipline). Teardown promptly (recurring LLM billing).
- Since `web_rest` becomes gate-capable (Step 8) but defaults open, the harness's existing ungated REST probes keep working; a new emergence case can validate sealing the REST leg and re-discovering it via the same keystone.

---

## 6. OPEN QUESTIONS (lead with recommendation)

1. **Rename the denial/descriptor pointer `io_core` → `io_bridge`?** **Recommend YES.** The keystone now ships in the `io_bridge` bundle and the loop must resolve to a reachable agent. It is an intra-Python wire change (`deny_inbound.see`, `describe()` field) that rust/swift mirror on port; the cross-runtime matrix only asserts `reason:'unauthorized'`, so the matrix is unaffected. Confirm before Step 3.

2. **`kernel_bridge` → `ws_bridge`: alias for one release, or atomic migration?** **Recommend ALIAS** (`kernel_bridge.tools` → `ws_bridge.tools` at the bundle resolver) for one release, then drop. This decouples the code rename from migrating every persisted `.fantastic` record + the cross-runtime `_HANDLER_MODULE` maps, and lets Step 6 land without a flag-day across three runtimes + TS fixtures. Ratify the alias-vs-atomic call.

3. **Record `transport` value for cloud: keep `"cloud_bridge"` or reconcile to the channel literal `"cloud"`?** **Recommend KEEP `"cloud_bridge"`** in the persisted record (cross-runtime-safe; `_cb_meta` + rust/swift read it) and treat the short `channel.Transport="cloud"` literal as an internal derived view. Reconciling the record value is wire-visible churn for no functional gain.

4. **Memory transport: first-class `build_transport` kind AND keep `_test_transport_inject`?** **Recommend BOTH** (§2.7): make memory first-class so the canonical two-kernels-in-one-process case has a real construction path, but keep the injection slot for unit tests that seat a specific pre-paired half (20-test suite + cloud_bridge tests depend on it). Confirm the dual path is acceptable rather than forcing all tests onto the new kind.

5. **HTTP inbound credential placement (web_rest / http_bridge `HttpHeaderExtractor`).** **Recommend a header** — `X-Fantastic-Auth` (or `Authorization: Bearer`) — over a query param (query strings leak into logs/referers). This convention is net-new and undefined today; it must be ratified before Step 8 and documented in the keystone so the message-modality story is uniform (envelope for frames, header for HTTP).

6. **Extend gate coverage to watch/emit/state_subscribe on memory/relay inbound legs (seal-completeness)?** **Recommend YES** (§2.6/Step 4) — it makes `deny_inbound` actually seal all inbound kinds (matching the docs and web_ws) and unifies behavior across derivations. It is a behavior change for memory/relay inbound legs; ratify that the re-baselined bridge tests are acceptable.

7. **Sealed-by-default flip (the deferred `#9`/§2.5 deny-all) — in scope for this collapse?** **Recommend NO, keep AllowAll/Silent defaults** for this collapse. The decision ratified is the *structural* collapse (base + derivations + reachable keystone), not the default-posture flip. Flipping to deny-all is a separate, larger behavior change that touches every existing test's open assumption and inverts `test_resolve_ingress_absent_is_allow_all` across runtimes. Land the collapse green first; flip the default as a follow-up. Confirm this scope edge.

8. **`http_bridge` scope: ship now or defer?** **Recommend ship a THIN stub now** (the outbound HTTP derivation + the `/file/` octet fold) since the directive is "full scope, built as the base then iterated, not carved for a later foundation update" — but it is the lowest-priority derivation and can be the last step (Step 9). Confirm whether a minimal http_bridge is required for "full scope" or whether folding `/file/` under the base later is acceptable.

---

**Key file paths for the build:** new base at `/Users/oleksandr/Projects/fantastic_canvas/python/bundled_agents/io/io_bridge/` (merge of `/Users/oleksandr/Projects/fantastic_canvas/python/bundled_agents/io_core/` + `/Users/oleksandr/Projects/fantastic_canvas/python/bundled_agents/bridge/bridge_core/`); renames at `/Users/oleksandr/Projects/fantastic_canvas/python/bundled_agents/bridge/kernel_bridge/` → `ws_bridge`; recasts at `/Users/oleksandr/Projects/fantastic_canvas/python/bundled_agents/bridge/cloud_bridge/` and `/Users/oleksandr/Projects/fantastic_canvas/python/bundled_agents/web/{web_ws,web_rest}/`; host unchanged at `/Users/oleksandr/Projects/fantastic_canvas/python/bundled_agents/web/host/`; boot seam at `/Users/oleksandr/Projects/fantastic_canvas/python/main.py`; doc rewrite at `/Users/oleksandr/Projects/fantastic_canvas/docs/io_core_spec.md` → `io_bridge_spec.md`. Cross-runtime lockstep targets: `/Users/oleksandr/Projects/fantastic_canvas/integration_tests/helpers/seeding.py`, `/Users/oleksandr/Projects/fantastic_canvas/integration_tests/relay_e2e/test_relay_matrix.py`, `/Users/oleksandr/Projects/fantastic_canvas/rust/crates/bundles/fantastic-kernel-bridge/`, `/Users/oleksandr/Projects/fantastic_canvas/swift/Sources/FantasticKernelBridge/`.