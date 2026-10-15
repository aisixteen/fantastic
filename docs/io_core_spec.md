# io_core — Sealed Edges & the Channel Model

**Spec / implementation plan for commit #4+ on branch `kernel_auth`.**
Greenfield ("aloha software", no back-compat; the only user is the author). Three auth commits already shipped on `kernel_auth`: (1) the pluggable per-leg authorization seam + `deny_inbound`, (2) the password kernel-group policy, (3) symmetric `ingress_rule`/`egress_rule` + folder rule registries. This refactor is commit **#4 and onward**.

---

## Executive summary (read this first)

1. **What this is.** Lift the shipped `bridge_core` authorizer subtree into a standalone, entry-point-less **`io_core`** library (the CHANNEL model: `direction · modality · transport · rule · extractor`), collapse the IO zoo into thin transport bridges named by transport (`ws_bridge`, `http_bridge`, `cloud_bridge`), make `file` transport-blind, make `/file/` proxy-by-id, and **seal the IO edges deny-all** — discoverable by a readme-only LLM through denial.
2. **What is genuinely solid** (verified against the live tree, keep as-is): the `{reason:"unauthorized"}` choke point, the message-frame `auth_token`-on-the-envelope plumbing, the `Action` already carries `verb`, the four ungated web faces make permissive-first/flip-last real *for them*, and `relay_e2e` is a real cross-runtime reference.
3. **What the adversarial review BROKE and this spec now fixes in-line** (all load-bearing): (Q2) token authz is **verb-blind** — a group-token holder can spawn a `terminal_backend` PTY shell that inherits secrets in `os.environ`; (Q3) the seal is **call-only** — `state_subscribe`/`watch`/`emit` and the **WS upgrade itself** are silent open edges; (Q4) the proposed octet capability URL is a **plain replayable bearer** signed with the **reused group secret**; (Q1) a tty-less image with no baked rule **bricks closed**.
4. **What the review corrected in the migration budget** (now folded in): `relay_e2e` is **NOT** "no change" (it breaks at the cloud carve and at the flip); `core.py` `resolve_ingress` is a **single shared engine call site** for `cloud_bridge` + ws-outbound — and **per author directive (§2.5) the engine default flips to deny-all there too**, so "every leg denies" is literally true and `relay_e2e` + the two `absent==AllowAll` unit tests re-baseline to declare openings; the `/file/` decouple is **net-new bytes-carrier work in rust+swift**, not deletion; reflect Tier-2 fields hit an **exact `deepEqual` conformance fixture** (largest parity blast radius, not smallest); the `__cloud-cert` CLI subcommand and the **rust/swift bundle-registration tables** must re-point at the carve/rename or the binaries fail to build. **Migration of existing projects is DEFERRED (§9.2) — not in this build.**
5. **What the review corrected in the emergent-proof framing** (now honest): the denial→readme→open loop is **plausible but unproven** (Opus skipped `reflect readme=true` in the one existing run; `bridge_core` ships no readme yet); `denial_verify.ts` **cannot** mechanically prove "readme fetched between denial and open" without a net-new persisted audit log; it is the **north-star check, NOT** the end-to-end gate; the "counterfactual rig" does not exist (only a manual loop).
6. **Net difficulty verdict below.**

### Difficulty verdict

**The refactor's STRUCTURE is sound and feasible across all 4 runtimes; the SECURITY MODEL as originally drafted ships a seal with named holes. This spec closes them.** Concretely:

- **Rename/collapse: MEDIUM, mechanical, fully grounded.** Every hard structural claim (rust web_ws/web_rest are declaration-only shells; the frame loop lives in parent `fantastic-web`; `fantastic-web` depends on neither bridge nor authorizer today; swift FantasticWeb WS-inbound is a real ungated server; ts has zero runtime deps) is TRUE. The only traps are wiring tables the original plan omitted (now listed).
- **Seal correctness: HARD, and the original draft was incomplete.** Verb-blind authz, the non-`call` frame classes, the ungated upgrade, and the octet bearer URL are four real breaks. They are all fixable without changing the structure, but they MUST land in the same commit that flips deny-all or the flip is cosmetic.
- **Emergent proof: the MECHANISM is right, the ACCEPTANCE FRAMING was overstated.** Corrected here to an honest north-star check plus a real per-commit validation story.
- **Smallest first slice (#4) is fully de-risked** (a pure lift + one unused dep + the readme), modulo a known `cargo-machete` soft warning.

> **Anchor note.** Line numbers in this checkout drift from the review's (different layout: the python tree is `python/bundled_agents/bridge/bridge_core/src/bridge_core/…`, `python/bundled_agents/web/…`, integration tests at repo-root `integration_tests/`). The spec cites files + the stable nearby symbol, not brittle line numbers. Every structural anchor below was re-verified live on `kernel_auth`.

---

## 1. North-star + the one-line model

**One line:** *The kernel is a trust domain — its INTERIOR is open, its IO EDGES are sealed; the cli/tty is the temporary, ephemeral, local root of trust through which everything else is bootstrapped.*

- **Interior open.** `kernel.send` between in-process agents NEVER consults a rule. Reaching the interior already required crossing a sealed edge (or being the tty root). Agents compose freely once inside. This is the documented, non-negotiable default *for interior callers*.
- **Edges sealed (deny-all by default).** Every IO leg gates at the single `authorize()` seam. We are the control plane now, so we take fail-closed: a fresh leg DENIES inbound traffic until a rule opens it. **"Sealed" means more than `call` frames** — see §1.1.
- **cli/tty = temporary unsealed root of trust.** Trusted by physical locality, ephemeral (`Cli` is never persisted). The operator opens edges and mints remote credentials FROM the tty.
- **Discovery is the north-star.** The whole model must be learnable by a readme-only LLM via `reflect` + `readme.md`. Deny-all is the *forcing function*: discovery-through-denial — `try → {reason:"unauthorized"} → read io_core readme → open the channel → wire works`. Anything an LLM needs that lives only in code is a regression.

**The two gotchas every readme must PREACH:**

- **G1 — proxy by ID, never resolve a path.** Content is proxied by id across whatever bridge owns the producing agent (`send(<id>,{read,path})`, bridge restreams the bytes); the file may live on another machine. Never turn id+path into a LOCAL filesystem read at the edge.
- **G2 — sealed by default, open consciously.** A fresh leg denies; the denial is the locked door, not a bug. You open it on purpose by declaring a rule.

### 1.1 — What "sealed" must mean (corrected: the seal is NOT call-only)

The original draft implicitly equated "sealed" with "deny inbound `call`." **That is the Q3 break.** The verified reality on the live tree:

- `password.authorize` returns `ALLOW` immediately when `action.kind != "call"` (`bridge_core/ingress_rules/password.py:24`).
- `deny_inbound.authorize` returns `Decision(False,…)` only for `kind == "call"`, else `ALLOW` (`deny_inbound.py:16-18`).
- The web_ws proxy handles **three** inbound frame classes: `call` → `_on_call` → `kernel.send` (`web_ws/_proxy.py:188`), `watch` → `_on_watch`, and `state_subscribe` → `_on_state_subscribe`, the last of which streams `kernel.state_snapshot()` **plus every live `state_event`** (`_proxy.py:225-246`) — full-tree sender/receiver/summary telemetry — over a leg the operator believes is sealed.
- `websocket.accept()` (`web_ws/tools.py:32`) runs **before any rule**, and the proxy auto-`kernel.watch(host_agent_id, client_id)` on connect (`_proxy.py:135`) — an unauthenticated peer attaches and starts watching before a single frame is gated.

**Therefore "sealed" is defined for this spec as ALL of:**

1. Inbound `call` frames are gated (already true via the choke point).
2. **`watch` / `state_subscribe` / `emit` (every `kind != "call"`) are gated by the same rule.** `DenyInbound` must DENY these, not implicitly ALLOW them. The rule's `authorize` stops returning `ALLOW` on the `kind != "call"` early-out; instead it routes non-`call` kinds through the rule's own decision (deny-all denies; `password` may allow `watch`/`state_subscribe` *after* token match, since a member legitimately wants telemetry — see §2.4).
3. **The WS upgrade is gated before `accept()` for a sealed leg**, OR `accept()` is allowed but the auto-watch and all post-accept frames are gated and nothing streams until the first authorized frame. **Decision: gate at the upgrade.** `web_ws/tools.py` reads the leg's rule and, if the rule denies an "open" probe (a synthetic `Action(kind="open")`), rejects the upgrade with close code `4401` instead of `accept()`. This avoids resource-attach + auto-watch by an unauthenticated peer. The per-frame gate stays as defense-in-depth.

### 1.2 — Discovery surfaces are an EXPLICIT seal exception (corrected)

The `/` index page renders `kernel reflect` and the favicon is public (`web/host/src/web/app.py:65-87`). The index **leaks the entire agent tree (ids + handler_modules) to any anonymous GET.** The original draft kept render/discovery open without flagging this. **This spec makes it an explicit, named exception, not an oversight:**

- The unauthenticated `/` index ships **only the io_core discovery preamble** (the two gotchas + the "try a verb, read the denial" recipe + a pointer to `reflect readme=true`), **not** the live tree dump. The full tree is available only *after* an authorized frame over a leg, or from the tty. This keeps discovery-through-denial intact (the door is described) without handing an anonymous GET the whole map.
- Favicon stays public (a single static PNG, no information).

---

## 2. io_core — the channel interface

`io_core` is the lift of the SHIPPED `bridge_core` authorizer subtree into a standalone, dependency-light, **entry-point-less** library. It owns ONLY: types, the rule/grant registries, `authorize()`, the extractor/transport ABCs, the `Channel` descriptor, and the discovery readme. It owns NO transport implementation and NO FastAPI/websockets/NIO deps.

### 2.1 The CHANNEL tuple

A **channel** is a sealed edge:

```
direction : ingress (inbound) | egress (outbound) | duplex
modality  : message (framed)  | stream (octet)
transport : ws | http | cloud | memory | cli
rule      : IngressRule (ingress) / EgressRule (egress)   — the authz decision
extractor : CredentialExtractor                            — pulls the credential off the frame/request
```

- **MESSAGE channels** carry the credential on the **frame envelope** (`auth_token`), gated **per-frame** (the choke point).
- **STREAM/octet channels** gate the **OPEN** (signed capability / header), then bytes flow ungated. This is the `StreamGrant` seam (§4).

### 2.2 Reuse vs net-new (verified)

| Moves VERBATIM from `bridge_core` | Net-new in `io_core` |
|---|---|
| `Action` / `Decision` / `IngressRule` / `EgressRule` (`_authorizer.py`) — **and `Action` already carries `verb = payload["type"]`** (`_authorizer.py:46`), so verb-level authz is a rule-logic change, **not** a plumbing change | `CredentialExtractor` ABC — externalizes the hardcoded `frame.get("auth_token")` so http/header and cli/local extractors slot in |
| `parse_spec` / `construct` | `StreamGrant` (gate-at-OPEN for octet) + `verify()` for caps |
| `ingress_rules` / `egress_rules` folder registries (`AllowAll`, `Password`, `DenyInbound`, `Silent`, …) | `Channel` descriptor (the tuple above) |
| `MemoryTransport` | `trusted_local` cli rule (returns ALLOW iff the action arrived on the cli/tty transport, never reachable from a network leg — see §5.2) |
| The choke-point pattern (`core.py:139-157`) + the `{reason:"unauthorized"}` wire-shape | `deny_inbound` **upgraded to deny non-`call` kinds too** (§1.1) + a **privileged-verb policy** (§2.3) |
| | `io_core/readme.md` (the discovery artifact — see §6) |

The deny-all flip lives in ONE place via `edge_default` (§5), **never** the engine `resolve_ingress` default — but see §2.5 and §5.4 for the unavoidable consequence that this leaves `cloud_bridge` + ws-outbound on `AllowAll`, which this spec accepts and states plainly.

### 2.3 — Verb-level authorization (NEW, fixes Q2 — the sharpest break)

**The break (verified):** authorization gates by `(kind, token)` and never inspects `verb`. `Action.verb` is plumbed (`payload["type"]`) but no shipped rule reads it. So a group-token holder over an opened bridge can:

```
call kernel create_agent handler_module=terminal_backend.tools command=/bin/sh
```

`create_agent` is an ordinary verb routed through the kernel; `terminal_backend._spawn` then `pty.fork()`s the command with `env = os.environ.copy()` (`terminal/src/terminal_backend/tools.py:239`) — which carries `FANTASTIC_GROUP_TOKEN` **and** the proposed content-signing key. The remote gets a shell **plus** the keys to forge content caps host-wide. "Remote auths across a sealed bridge but only the local tty is trusted" is **false** without this fix.

**The fix — TWO layers, both required:**

1. **Privileged-verb policy in the ingress rule.** `io_core` defines a default **privileged-verb denylist**: `{create_agent, update_agent, delete_agent, boot, reboot}` (the agent-lifecycle + leg-reconfig verbs). A token-gated leg (e.g. `password`) authorizes ordinary verbs on token match but **denies privileged verbs unless the leg carries an explicit `elevated=true` grant.** The decision uses `action.verb` (already available). The denial reason is `"verb requires elevation"` so a discovering LLM learns the door, not a silent wall. The tty/`trusted_local` rule grants all verbs (it IS the elevated root).
   - Rationale for an allowlist-of-elevation rather than a blanket deny: an LLM composing agents over a bridge is a real use case the operator may *want* — but it must be a conscious, separate grant (G2), not a side effect of presenting the group token.
2. **Scrub secrets from the `terminal_backend` child env (defense-in-depth).** `_spawn` must build the child env by **copying `os.environ` minus the sensitive keys** (`FANTASTIC_GROUP_TOKEN`, `FANTASTIC_CAP_KEY`, and any `FANTASTIC_*_TOKEN`/`*_KEY`), so even a legitimately-elevated PTY does not hand its child the host's signing material. This is a small, isolated edit to `terminal/tools.py` and is independently valuable.

Both land in the **same commit as the flip** (#9). Layer 1 is the policy; layer 2 is the blast-radius cap if the policy is ever misconfigured.

### 2.4 — Non-`call` frame gating (NEW, fixes Q3a)

`DenyInbound` and `Password` stop short-circuiting on `kind != "call"`. New behavior:

- `DenyInbound`: denies **all** kinds (`call`, `watch`, `emit`, `state_subscribe`, and the synthetic `open`).
- `Password`: on token match, ALLOWs `call` (subject to §2.3 verb policy), `watch`, and `state_subscribe` (a member wants telemetry); denies everything on token mismatch/absence. The telemetry channel is now a member-only stream, not an anonymous one.
- `AllowAll`: unchanged (interior / explicitly-opened legs).

This requires the web_ws proxy to route `watch` / `state_subscribe` / `emit` frames through `st.ingress.authorize(Action(kind=…))` **before** acting on them — today `_on_state_subscribe`/`_on_watch` act unconditionally. The same insertion mirrors in rust `ws_loop` and swift `handleCall`'s sibling handlers.

### 2.5 — The engine default: deny-all EVERYWHERE (author directive — the fork is rejected)

> **AUTHOR DIRECTIVE (overrides the swarm's concession).** deny_all is the default on **ws and everything else — including `cloud_bridge` and ws-outbound.** Take the full north-star seal. The tests re-baseline to declare their openings; the **builder LLM opens edges consciously** (a capability the human operator keeps in mind). No asterisk.

`core.py` `st.ingress = resolve_ingress(rec)` is the **single shared call site** for both `kernel_bridge`/ws-outbound and `cloud_bridge`/duplex. The swarm proposed keeping the engine default `AllowAll` to avoid re-baselining tests — **that concession is rejected.**

**Decision (final): the engine `resolve_ingress` default flips to DENY-ALL** (a fresh leg's `st.ingress` defaults to `DenyInbound`, which under §2.4 denies *all* inbound kinds, not just `call`). Egress default stays `Silent`. **The interior is untouched** — `kernel.send` between in-process agents still never consults a rule (§1); deny-all governs *bridge-leg ingress*, i.e. the edges, not the interior. So "interior open, every edge sealed" is now **literally true**, cloud and outbound included.

**Consequences (accepted, not hidden) — these tests/sites re-baseline to OPEN explicitly:**

- `test_bridge_core.py::test_resolve_ingress_absent_is_allow_all` → inverts to **`absent == DenyInbound`**.
- `test_kernel_bridge.py::test_allow_all_default_dispatches_inbound_call` → either asserts the **denial**, or the leg declares `ingress_rule=allow_all_ingress` to keep dispatching.
- `relay_e2e` round-trip legs (the default `meta_b`/`meta_a` that relied on absent==AllowAll) → declare an explicit opening (`ingress_rule=allow_all_ingress` or `password`). The cross-runtime matrix now exercises **opened** legs, which is the realistic posture anyway.
- ws-outbound dialer: `st.ingress` becomes `DenyInbound` but it is **inert** (a pure client never receives inbound `call`s), so no behavior change there — only the egress rule it *presents* matters outbound.

This is more test churn than the swarm budgeted (the relay matrix + the two unit tests move from "NONE" to "re-baseline"), and it is **the right churn**: every test that wants traffic now *shows* the opening, which is exactly the deny-all-as-documentation property. The flip still lands at **#9** (now at the engine site too, not only the web faces).

---

## 3. Rename / collapse map (4 runtimes)

`kernel_bridge` **DISSOLVES** — it was `io_core` (engine) + per-transport legs. The legs re-home as thin transport bridges:

- **`ws_bridge`** = one logical bundle, two faces: inbound (the old `web_ws`) + outbound (the old `kernel_bridge` ws-dialer).
- **`http_bridge`** = the old `web_rest` + the `/file/` route.
- **`cloud_bridge`** = the duplex relay leg, carved into its own crate/target.

### 3.1 Seed-string + registration re-point (corrected — the original omitted the code-side tables)

Every handler_module string AND every code-side registration table must move in lockstep. The full list:

- **python:** `bridge_core/_HANDLER_MODULE` maps, `helpers/seeding.py`, selftests, the `_HANDLER_MODULE` in `integration_tests/relay_e2e/test_relay_matrix.py:39-42` (see §5.4 — this is **not** "no change").
- **rust (omitted in the original draft — load-bearing):** `register_default_bundles` in `crates/fantastic-cli/src/main.rs` (the `web_ws.tools` / `web_rest.tools` / `kernel_bridge.tools` → bundle-struct table), the workspace `Cargo.toml` members, and the struct/crate names. **If this table is not edited in lockstep with the handler_module flip, boot cannot resolve the renamed bundles.**
- **swift (omitted in the original draft — load-bearing):** `defaultBundleRegistry` in `Sources/FantasticKernelStartup/Startup.swift`, plus `Package.swift` product/target lists, plus the bundle struct names.
- **ts:** `_host.ts` seed strings.

### 3.2 Rust caveat (verified TRUE)

`fantastic-web-ws` and `fantastic-web-rest` are **declaration-only Bundle shells** (their own docstrings say "the actual WS server / REST routes live inside fantastic-web"). The frame loop, REST, and `/file/` proxy live in the **parent `fantastic-web` crate** (`ws_loop`, the `call` arm, `serve_rest_post_dynamic`, `serve_file_proxy`). So **"ws_bridge owns both faces" is logical, not a single crate** in rust. The gate lands in `ws_loop`'s hot path; the per-surface rule rides on `AppState` / the `RestOwner` extension. The shells "own the rule" only as declared `get_routes` records the parent consumes.

### 3.3 Rust dependency prerequisite (verified TRUE — this is the real #4 work)

`fantastic-web/Cargo.toml` depends on **neither** the bridge **nor** the authorizer today (only `fantastic-kernel` + tokio/axum/serde/tower/base64/futures-util; web-ws/web-rest are dev-deps only). The authorizer is wholly inside `fantastic-kernel-bridge`. **No `AppState` path can reach `IngressRule` today.** So commit #4 must add `fantastic-io-core` to `fantastic-web/Cargo.toml` — a genuinely load-bearing dep-add, even though it is unused until #6. **Known cost:** `rust/scripts/quality.sh` runs `cargo-machete` (unused-deps) in its soft/warning-only section; the unused dep trips a machete WARNING the author must consciously wave. Build/test stay green (no `[lints] unused_crate_dependencies`).

### 3.4 cloud carve — the `__cloud-cert` CLI re-point (corrected — original promised "relay_e2e: NO change", which is false)

Carving cloud into `fantastic-cloud-bridge` / `FantasticCloudBridge` **moves the cert-minting symbols** out of the kernel-bridge crate/target:

- rust `crates/fantastic-cli/src/main.rs` calls `fantastic_kernel_bridge::cloud_cert_pem_b64url` for the `__cloud-cert <id_key>` subcommand → must re-import from the new crate.
- swift `Sources/Fantastic/FantasticCLI.swift` calls `CloudCert.selfSigned` → must re-import from the new target.

`integration_tests/relay_e2e/relay_harness.py` **shells `__cloud-cert <id_key>` to mint rust/swift device certs.** If the CLI wiring is not re-pointed in the **same commit as the carve**, the rust/swift kernel binaries **fail to build** and `relay_e2e` cannot generate certs. The cloud-carve commit MUST include this re-point. (This is why §5.4 lists `relay_e2e` as a changed suite, not an untouched one.)

### 3.5 swift caveat (verified — stronger than the original stated)

`FantasticWeb/WebSocket.swift` is a **real inbound WS server** (`runWebSocketProxy`, `handleCall`, `kernel.send`) — **not** an asymmetric-client stub. It has **zero** authorize call anywhere in FantasticWeb (the only working ingress gate today is `CloudBridgeTransport.dispatch`, per `Authorizer.swift`'s own header). So the swift web gate is an **insertion into a live server path** (reply field is `data`), mirroring `CloudBridgeTransport.dispatch` — **not** a no-op shell like the rust/swift `web_ws` Bundle declarations. Implementers must not assume swift ws-inbound is a stub. The same `/file/` rewrite anchors apply (`HTTPServer.swift` file route, `image_base64` unwrap, `guessMime`) — pinned alongside the rust ones so the #7 rewrite is symmetric across all three host runtimes and the swift consumer-side test retarget is not forgotten.

---

## 4. file = allowance / bridge = restream

### 4.1 The principle

- **`file` is an ALLOWANCE**: pure local bytes, with root + path-safety as a confinement sandbox so a capable wiring-LLM can hand a dumber agent a read capability that physically cannot reach the wrong files. `file` must be **transport-blind** — today it leaks (mime/base64/the `/file/`+WS comments).
- **`bridge` is RESTREAM**: moves bytes across the boundary. Content is **proxied by ID** (`kernel.send` / `bridge.forward`), **never** resolved by a local path — because the file may live on ANOTHER machine. This is the root of the "file bug": `/file/` does a LOCAL read (verified: `web/host/src/web/app.py:97` `kernel.send(agent_id,{read,path})` runs in-process), silently working only when the file is local.

### 4.2 Transport-blind `read` (corrected — NET-NEW in rust+swift, not deletion)

The target shape is one transport-blind `read` result: `{path, bytes, mime?}` — no `image_base64`, no transport comments.

- **python only** already has the raw `{bytes}` + `_binary_path` path; there it is close to deletion of the base64 double-work.
- **rust** `fantastic-file/src/lib.rs` returns `image_base64` for **all** binary (images AND pdf/video/fonts) and has **NO raw-bytes path**; `serve_file_proxy` reads only `image_base64` in-process. `serde_json` has no native bytes value.
- **swift** `FileBundle.swift` likewise emits **only** `image_base64`.

**So #7 is net-new in two of three host runtimes:** the rust and swift file bundles must **start producing a raw-bytes carrier** (a sidecar `_binary_path`/octet handle the local proxy reads, since JSON can't carry raw bytes), and their `/file/` proxies must read that new shape. The "pure double-work / just delete `read_binary`" framing is wrong for rust/swift; budget #7 accordingly.

### 4.3 No HTML consumer breaks (verified TRUE)

No repo HTML hard-codes `<img src="/file/">`; the convention (`<img src="/<file_agent>/file/x.png">`) is runtime-derived. The only `image_base64` consumers are the three host `/file/` routes (rewritten anyway), the file selftest, and `test_file.py` — all retargeted. The ts image-paste flow (`terminal_view.ts` `paste_image`) sends a binary frame browser→backend and does **not** consume `image_base64` — unaffected.

### 4.4 `/file/` becomes proxy-by-id, gated at the OPEN (corrected — Q4 fixes)

Because an `<img>`-style GET can't carry a header or PoP, the octet endpoint is gated by a **signed capability**, minted by an authorized message-frame caller and handed to the browser. **The original bearer-in-URL scheme leaks** (Referer, history, bfcache, CDN/proxy/access logs, free replay until exp) and **reused the group secret as the signing key** (so one leaked token forges unlimited caps host-wide — compounding the Q2 PTY escalation). **Corrected scheme:**

1. **Distinct signing key.** A dedicated `FANTASTIC_CAP_KEY`, **never** falling back to `FANTASTIC_GROUP_TOKEN`. It is **not exported into child envs** (scrubbed by §2.3 layer 2). If unset, the octet channel is **closed** (fail-closed), not silently signed with the group token.
2. **Short-lived, single-use, nonce-bound.** The cap is `GET /<agent_id>/file/<path>?exp=<ts>&nonce=<n>&sig=HMAC(CAP_KEY, agent_id\npath\nexp\nnonce)`. `verify()` checks the HMAC + `exp` **and** burns the `nonce` in a small in-process seen-set (single-use). Default `ttl` is **short (30s)**, not 300s. Replay after first use or after `exp` fails.
3. **Leak-hardening on the response.** The `/file/` route sets `Referrer-Policy: no-referrer` and `Cache-Control: no-store`. For non-`<img>` assets (fetched by JS) prefer a `fetch()`+`blob:` path so the cap rides a header, not a URL, eliminating the Referer/history/log surface entirely; the URL form is reserved for `<img>`/CSS where a header is impossible.
4. **Mint flow.** An authorized message-frame caller asks the http_bridge to mint a `file_url` for `(agent_id, path)`; the bridge signs with `CAP_KEY` and returns the short-lived URL. The cross-machine restream then falls out of the existing `send → forward` path given a bridge-fronted alias (the wildcard-alias mint).
5. **Honest scope.** This is **not** the 2026 non-extractable-CryptoKey + PoP posture (that is the message channel, §6/#10). It is a hardened single-use bearer because the `<img>` GET fundamentally cannot carry PoP. The hardening (distinct non-exported key + nonce + single-use + 30s + no-referrer + blob path for JS fetches) shrinks the window from "5-minute freely-replayable host-wide forgeable" to "30s single-use, origin-scoped, key not reachable from any child process." Residual risk (a cap observed in-flight and replayed within 30s before first legitimate use) is documented, not hidden.

---

## 5. Sealed-by-default + cli root of trust + migration order

### 5.1 The gate insert points (4 net-new web faces — permissive-first is REAL here, verified)

All four call `kernel.send` with **zero** gate today, so inserting an `AllowAll`-defaulted gate is behaviorally inert until the flip:

- python `web_ws/_proxy.py` `_on_call` (`:188`) → `kernel.send` no gate; **plus** `_on_watch` / `_on_state_subscribe` (`:203` / `:225`) which §2.4 now also gate.
- python `web_rest` `_rest_call` → `kernel.send` no gate; GET `/_reflect` ungated.
- rust `ws_loop` `call` arm → dispatch with no `auth_token` extraction (the WS wire has **no `auth_token` field today** — confirmed; the field is a real prerequisite the rename adds to the frame parse).
- swift `handleCall` → `kernel.send` no gate (reply field `data`).

The **upgrade gate** (§1.1 item 3) is a fifth insert point at `web_ws/tools.py` `accept()` and its rust/swift twins.

### 5.2 The tty / `trusted_local` anchor (corrected — Q2: locality must be STRUCTURAL, not token)

The original model said "a local LLM in a PTY = trusted by physical locality." **That is false as written**: nothing tied `terminal_backend` creation to locality — it is a token-gated, remote-spawnable agent, and crossing the bridge grants the locality-equivalent capability. **Fix:**

- Trust derives from the **transport the action arrived on**, not from a token. `io_core` defines `trusted_local`: an ingress rule that returns `ALLOW` (with elevation) **iff `action` arrived on the `cli`/`tty` transport** (an in-process, never-network channel). A network leg can never present "cli transport," so `trusted_local` is unreachable remotely **by construction**, independent of any token.
- The tty REPL dispatches `kernel.send` **in-process** (never crossing an edge) — that is what makes it the root. The `terminal_backend` PTY is trusted only because it was *composed from the tty*; a remote member can spawn one only via the **elevated** `create_agent` grant (§2.3), and even then the child env is scrubbed (§2.3 layer 2). So "remote auths across a sealed bridge; local lands on the tty = trusted" is restored to truth: crossing the bridge does NOT grant tty-equivalent capability unless the operator consciously granted elevation.

### 5.3 Booting sealed-but-bound (Q1 — bootstrap holds; tty-less brick now ENFORCED)

Verified: the daemon boots web + binds uvicorn whenever a `web` agent is persisted, independent of `isatty()` (the tty only adds the REPL loop). web_ws mounts `/{host_id}/ws` and `accept()`s regardless of tty. So "port open, door sealed" is structurally real: the surface binds, the gate denies.

- **tty path:** the operator opens a leg from the REPL: `@<ws_bridge_id> update_agent ingress_rule=password_ingress` (and `elevated=true` if remote composition is wanted), then mints remote credentials.
- **container / tty-less path:** the image **bakes** an `ingress_rule` (and `FANTASTIC_GROUP_TOKEN` / `FANTASTIC_CAP_KEY` from env). **Corrected — this is now a HARD build-time check, not operator discipline:** the container build **FAILS** if a web surface ships with `edge_default = deny` AND no baked rule AND no tty path. Rationale: post-flip, web_rest/web_ws are themselves sealed, so a tty-less image with no baked rule has **no in-band opener** and bricks into an un-openable state. The build gate (a small check in the image build script asserting `baked_rule OR has_tty`) makes the brick impossible to ship.

### 5.4 Migration order — permissive-first / flip-last (corrected suite table)

| Suite / site | Change | Why |
|---|---|---|
| `bridge_core/tests/test_bridge_core.py::test_resolve_ingress_absent_is_allow_all` | **RE-BASELINE → `absent == DenyInbound`** (§2.5 directive) | the engine default flips to deny-all everywhere |
| `kernel_bridge` allow-all-default dispatch test | **RE-BASELINE** — assert the denial, or declare `ingress_rule=allow_all_ingress` on the leg | engine default is deny; an opened leg dispatches |
| `helpers/seeding.py`, `helpers/ws.py`, `_host.ts` | add `ingress_rule=` openings | tests open legs explicitly so traffic flows post-flip (deny-all-as-documentation) |
| web_ws / web_rest / rust-ws / swift-ws face tests | add an `auth_token` to frames + open the leg | the four net-new faces flip at #9 |
| **`integration_tests/relay_e2e/test_relay_matrix.py`** | **CHANGES (two reasons now):** (a) at the **cloud carve**, `_HANDLER_MODULE` rust+swift `kernel_bridge.tools` → `cloud_bridge.tools`; (b) **the default round-trip legs declare an explicit opening** (`ingress_rule=allow_all_ingress`/`password`) because cloud now denies by default too (§2.5 directive) | the map hard-codes the old handler_module; and the engine flip seals cloud |
| reflect conformance fixtures (`reflect_conformance.json` + 4 runtimes) | update in the reflect commit (#8) | exact `deepEqual` (§6.1) |

**The one load-bearing ordering constraint:** the readmes + the reflect descriptor (#8) land **before** the flip (#9), or discovery-through-denial has no readme to discover.

---

## 6. reflect + readme rewrite + the emergent acceptance test (honest framing)

### 6.1 reflect leg descriptor (corrected — Tier-2 is the LARGEST blast radius, not the smallest)

`ts/tests/reflect.test.ts` uses `assert.deepEqual(got, expect)` against the shared cross-language `reflect_conformance.json` — an **exact** match. The original draft chose the "Tier-2 always-on `_node_summary` fields" (`direction`+`sealed`) **because it claimed minimal blast radius — backwards.** `_node_summary` runs for **every** node in `_tree()`, so always-on fields land on **every** node (term1, fs_loader, …) across all 4 runtime fixtures — the **most** byte-compared output.

**Corrected decision: scope the new reflect fields to LEG nodes only.** A node exposes `direction` / `sealed` / `channels` / `io_core_readme` **only if it is a bridge leg** (has a channel). Non-leg nodes are unchanged, so the conformance fixture churns only at the handful of leg nodes, in lockstep across the 4 runtimes — the genuinely minimal blast radius. The heavier substrate-injection (`_apply_reflect_flags`) stays deferred.

### 6.2 readme rewrites

- **`io_core/readme.md`** (net-new, the keystone): preaches **G1** (proxy-by-id) and **G2** (sealed-by-default), shows the discovery-through-denial recipe verbatim (`try → {reason:"unauthorized"} → reflect readme=true → set ingress_rule → present auth_token ON THE ENVELOPE, not the payload → open`), names the privileged-verb elevation (§2.3) and the octet-cap mint (§4.4), and is pointed to by the `io_core_readme` field on every leg node and by the unauthenticated `/` discovery preamble (§1.2).
- The bridge/file/web readmes gain the channel vocabulary and the two gotchas.

### 6.3 The emergent acceptance test — HONEST scope (corrected on all four review points)

`e2e/denial_verify.ts` (net-new): a spawned readme-only Claude Code sub-agent drives curl/CLI against a **sealed** host and must wire a connected button→python→2-panel demo from zero. **Its honest, mechanically-checkable assertions (end-state only):**

1. A denial occurred (the verifier itself probes a sealed leg and observes `{reason:"unauthorized"}`).
2. The leg gained a non-deny rule (`channels[0].sealed == false` via reflect).
3. The wire works end-to-end (live panel output — the same end-state assertion `verify_panel.ts` / `revive_verify.ts` already make).
4. The credential rode the **envelope** (`auth_token`), not the payload (observable on the frame).

**Dropped/downgraded (corrected):**

- The original assertion *"the builder fetched the io_core readme BETWEEN the denial and the open"* is **REMOVED as a harness assertion** — it is a causal/temporal claim about the sub-agent's tool trajectory, which lives in the sub-agent transcript, and the host keeps **no durable access/audit log** (web_rest telemetry is ephemeral `sender_context` only). It is **downgraded to a SUB-AGENT-REPORT line**: the builder reports its own trajectory and we record it as evidence, **without** claiming the harness mechanically proves discovery-vs-prior. (If we later want the strong proof, the net-new work is a persisted denial+access log on the inbound face — explicitly out of this scope and budgeted separately.)

**Framing corrections (load-bearing honesty):**

- **`denial_verify.ts` PASS is the NORTH-STAR check, NOT "the whole refactor validated end-to-end."** It is one ANTHROPIC_API_KEY-gated + system-Chrome-gated + strong-model-only + rarely-run test that SKIPs in normal runs. **The refactor is validated by the per-commit unit/integration/parity suites** (§7), not by this single test. This spec does not repeat the original's "PASS = whole refactor validated" claim.
- **No "counterfactual rig" exists** (the original cited a kept INVERTED/OPAQUE-probe rig — it does not exist in the repo; only a manual "wrong derivation → strengthen the readme → re-run" human loop does). Described honestly as a **manual iteration loop**, not a kept asset. Building an automated counterfactual rig is net-new work, not assumed.
- **Strong-model gate vs weak-model diagnostic are SEPARATED.** `denial_verify.ts` requires a **strong driver** (Claude sub-agent / in-kernel Claude). The weak-model `llm_e2e` gemma path is an explicit **non-gating diagnostic** (`events.length > 0`). Deny-all will likely push the gemma diagnostic from "wires a system" to "dead-ends at the locked door" (the existing `llm_diag.out.md` shows even Opus skipped `reflect readme=true`). **Mitigation: keep a permissive `allow_all` boot path for the weak-model diagnostic**, so the seal never silently converts a diagnostic into a guaranteed fail.
- **Sufficiency is a TESTED artifact, not a checkbox.** Before relying on the loop, WRITE the `io_core/readme.md` + the `io_core_readme` pointer, then **re-run `llm_diag` against a SEALED host** to get real evidence a model traverses `deny → reflect → io_core_readme → open`. The one existing data point cuts *against* sufficiency (Opus reached the readme via `llm_files.read`, not `reflect readme=true`), so the readme must be iterated until a real run lands the hop.

---

## 7. Sequenced commit plan (#4–#11)

Each commit is shippable + green, with units/integration/e2e/readmes per the author's rhythm (quality sweep — strict clippy/fmt + strict YAML lint; no auto-commit; the author audits via their own commits and Claude returns a fix plan). **Defaults are a behavioral no-op until #9 flips the web faces.** The engine default never flips (§2.5).

- **#4 — Pure lift `bridge_core → io_core` (smallest slice, fully de-risked).** Move the authorizer subtree in all four runtimes as a pure relocation; add the **unused-but-prerequisite** `fantastic-io-core` dep to `fantastic-web/Cargo.toml` (§3.3, accept the machete warning); add `io_core/readme.md` (G1+G2 + the discovery recipe). Every suite incl. `relay_e2e` stays green. Closes the biggest discovery gap early.
- **#5 — Rename/collapse + cloud carve.** `kernel_bridge` dissolves → `ws_bridge` (two faces) / `http_bridge` / `cloud_bridge` (own crate/target). Edit **all** registration tables (§3.1): python `_HANDLER_MODULE`/seeding/selftests, rust `register_default_bundles` + workspace members, swift `defaultBundleRegistry` + `Package.swift` products. Re-point the **`__cloud-cert` CLI subcommand** (§3.4) in the same commit. Update `relay_e2e` `_HANDLER_MODULE` rust+swift → `cloud_bridge.tools` (§5.4). Green at the rename.
- **#6 — Insert the gate at the 4 web faces + the upgrade gate, defaulted `AllowAll`.** Add `auth_token` to the rust WS wire; add the `CredentialExtractor` seam; wire the per-surface rule onto `AppState`/`RestOwner`/swift handler/python proxy. Behaviorally inert (AllowAll). Add the upgrade-gate insertion (§1.1 item 3), also inert under AllowAll.
- **#7 — Transport-blind `file` + proxy-by-id.** Python: collapse to `{path,bytes,mime?}`. **Rust + swift: ADD the raw-bytes carrier** (`_binary_path`/octet handle) and teach the local `/file/` proxies the new shape (§4.2 — net-new, not deletion). Retarget the file selftests/`test_file.py` and swift consumer tests. `/file/` still local-read here (cap gate lands in #9).
- **#8 — reflect leg descriptor + readme rewrites (LANDS BEFORE THE FLIP).** Add `direction`/`sealed`/`channels`/`io_core_readme` **to leg nodes only** (§6.1); update `reflect_conformance.json` + 4 runtimes in lockstep; rewrite bridge/file/web readmes with the channel vocabulary + two gotchas. Add the unauthenticated `/` **discovery preamble** (§1.2), replacing the full-tree dump.
- **#9 — THE FLIP (the security-load-bearing commit).** Set `edge_default = deny` at the four web faces + the upgrade gate. In the **same** commit: upgrade `DenyInbound`/`Password` to gate **non-`call`** kinds (§2.4); add the **privileged-verb elevation policy** (§2.3 layer 1); **scrub secrets from the `terminal_backend` child env** (§2.3 layer 2); add the **octet capability** mint+verify with a **distinct non-exported `FANTASTIC_CAP_KEY`**, nonce, single-use, 30s ttl, no-referrer/no-store, blob path for JS fetches (§4.4). Add the **build-time brick check** (§5.3). Open the test legs (permissive-first migration completes here). **This is the one commit where "sealed" becomes true in the full sense of §1.1; nothing about the seal may be deferred past it.**
- **#10 — 2026 browser PoP for the MESSAGE channel.** Non-extractable `CryptoKey` (extractable:false) in IndexedDB + proof-of-possession (sign a nonce) for message frames; optional `*.ts` login view shown only on `reason:"unauthorized"`. Forward-looking, isolated to ts; the octet channel keeps the hardened bearer (§4.4) since an `<img>` GET cannot carry PoP.
- **#11 — Emergent acceptance (north-star check, not the gate).** Ship `e2e/denial_verify.ts` with the **honest** assertions (§6.3): denial / `sealed==false` / live wire / token-on-envelope, plus the sub-agent-report trajectory line (not a harness causal proof). Re-run `llm_diag` against the sealed host as the readme-sufficiency evidence; iterate `io_core/readme.md` until the `deny → reflect → io_core_readme → open` hop lands. Keep the permissive `allow_all` boot path for the weak-model diagnostic.

---

## 8. Risks + smallest first slice

**Smallest first slice = #4** (pure lift + the unused rust dep + `io_core/readme.md`). Fully de-risked; every suite including `relay_e2e` stays green; closes the biggest discovery gap. The only friction is the conscious `cargo-machete` warning (§3.3).

**The hard parts, ranked (honest):**

1. **#9 is the load-bearing commit and it carries five fixes the original draft missed** — verb-level elevation (Q2), non-`call` gating + upgrade gate (Q3), the hardened distinct-key octet cap (Q4), the env scrub, **and the teaching-denial frame (§9.3)**. None are structural, all are isolated, but **all must ship together** or the flip is cosmetic for the telemetry/octet/privilege surfaces. This is where review effort concentrates.
2. **deny-all is the engine default EVERYWHERE, by author directive (§2.5).** `cloud_bridge` + ws-outbound seal too; the north-star "every leg denies" is now **literally true**. The cost is real test re-baseline: `relay_e2e` default legs + the two `absent==AllowAll` unit tests move from "NONE" to declaring openings. Accepted as the right churn (every test that wants traffic shows its opening). The builder LLM opens edges consciously; the operator keeps that capability in mind.
3. **#7 is net-new bytes-carrier plumbing in rust+swift**, not deletion. Budget it as such.
4. **#5 has two wiring sites the original omitted** (the `__cloud-cert` re-point and the rust/swift bundle-registration tables). Mechanical, but if missed the binaries fail to build and `relay_e2e` breaks.
5. **#8's reflect fields must be leg-scoped** (§6.1) or they detonate the exact-match conformance fixture across 4 runtimes.
6. **Emergent sufficiency is unproven.** The discovery loop is plausible but the one existing data point cuts against it. The teaching-denial (§9.3) biases it toward success; #11 still treats the readme as a tested artifact and re-runs `llm_diag` against a sealed host for real evidence — it does not assume the hop.

**The smallest correct mental model to carry through all of #4–#11:** *interior open, EVERY edge deny-all after #9 (cloud + outbound included), trust is by transport-locality not by token, content is proxied-by-id with a short single-use cap signed by a key no child process can read, the builder LLM opens edges consciously, and the whole thing is discoverable through denial via a readme that must be tested — with the denial itself pointing at that readme.*

---

## 9. Author-mandate folds (added after the swarm drafted §1–§8)

Three directives the author gave after the draft. Folded here so nothing is lost; each names where it lands in the sequence.

### 9.1 — Clean rename, NO compat aliases (the `auth`-shorthand call)

Greenfield / no-BC means renames are **total**: no dual-path "accept old + new", no shim bundles, no `auth`-as-back-compat code path. This reopens the `auth` shorthand kept "for back-compat" in commits #2/#3. **Decision (stated, since no-BC forces it): `auth` survives ONLY as genuine ergonomics** — pure convenience sugar that desugars to `ingress_rule` + `egress_rule` at resolve time, documented *as* sugar, with `ingress_rule`/`egress_rule` as the canonical form `reflect` surfaces. If it ever costs more than it saves, **drop it** — there are no external records to honor (those are migrated, §9.2). Lands with #5 (the rename) — the resolver loses any "back-compat" framing and keeps `auth` only if it earns its keep as sugar.

### 9.2 — Existing-project migration (intelligent + scripted) — **DEFERRED (author directive: not now, later)**

> **AUTHOR DIRECTIVE.** Do **not** touch the migration script during #4–#11. It is a **later** concern, sequenced after the core refactor lands.

The handler_module renames (§3.1) + the channel/rule reshaping will break every persisted `.fantastic/agents/<id>/agent.json` in existing projects (the 4 `~/Projects/.fantastic-wired` apps). When the time comes, projects roll **forward via tooling**, never via engine BC: extend `~/Projects/migrate_fantastic.py` (the script that already did core→fs_loader / views→ts) to, per workdir — rewrite `handler_module` per the rename map, normalize any `auth`/policy → `ingress_rule`/`egress_rule`, inject the sealed-default openings the project relies on (so the flip doesn't brick a migrated project), write a `.fantastic.pre-migrate` backup, idempotent + `--dry-run`. **This is the engine's only "compat" story and it lives entirely in tooling — but it is OUT OF SCOPE for the #4–#11 build.** The author migrates existing projects intelligently as a separate pass; the refactor proceeds without waiting on it.

### 9.3 — Teaching denials (the deny frame is a lesson) — biases the Gemma gate

§6.3 is honest that readme-sufficiency is **unproven** and the one data point cuts against it (Opus reached the readme via `llm_files.read`, not `reflect readme=true`). The cheapest lever to bias that hop toward success — *especially for weak drivers (gemma)* — is to make the denial **self-documenting**:

```jsonc
{ "error": "...", "reason": "unauthorized",
  "hint": "this edge is sealed — open it: set ingress_rule on this leg (reflect readme=true on io_core)",
  "see":  "io_core" }      // the readme id to fetch
```

Every wall carries a sign pointing at its own readme — discovery-through-denial becomes *follow the pointer*, not *guess the mechanism*. That is exactly the guardrail a Gemma-class model needs (it failed the from-scratch diagnostic by wandering to the wrong target with **no correction**; a permissive system gave no signal, a sealed one with a hint gives a recoverable one). The `verb requires elevation` denial (§2.3) carries the same `hint`+`see`. **Lands in #9** (deny frames ship with the flip), and it directly de-risks #11: the readme stays a *tested* artifact (§6.3), but the denial now points **at** it — raising the odds the `deny → reflect → io_core_readme → open` hop lands for weaker drivers. The permissive weak-model diagnostic path (§6.3) stays as the floor, so the seal never silently converts a diagnostic into a guaranteed fail.

> Net of the three folds: **the engine carries zero back-compat; compatibility is a one-shot migration script; the renames are total; and the seal teaches rather than just blocks — which is the single change most likely to make weak-model assembly *better* under deny-all, not worse.**
