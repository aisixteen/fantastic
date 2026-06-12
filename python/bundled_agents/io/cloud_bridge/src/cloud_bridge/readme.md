# cloud_bridge — zero-trust relay bridge

Both peers dial OUT to a relay; they run peer-to-peer TLS 1.3 mutual-auth (Ed25519 pubkey-pinned).
**Sealed by default** — open: `update_agent <id> ingress_rule=allow_all`.
Same verbs/auth surface as `ws_bridge`. Transport: `cloud_bridge`.

Record fields: `relay_url` · `tenant_id` · `peer_id` · `partner_peer_id` · `rendezvous` · `id_key` · `approved_peer_certs`
Token on frame envelope (`auth_token`). Hub-spoke topology: set `ingress_rule=deny_inbound` on the spoke.
