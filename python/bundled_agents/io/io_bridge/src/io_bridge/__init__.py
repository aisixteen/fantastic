"""io_bridge — shared library: channel model + rule registries + bridge engine.

Pure shared code imported by every io_bridge derivation (ws_bridge, relay_connector,
web_ws, web_rest, file_bridge). NOT a registered bundle — no entry point, no agent
instance. The derivations are the agents; this is the code they share.
"""

from __future__ import annotations

from io_bridge._base import (
    ALLOW,
    Action,
    Decision,
    EgressRule,
    IngressRule,
    construct,
    describe,
    parse_spec,
    rule_name,
)
from io_bridge.channel import (
    Channel,
    CredentialExtractor,
    Direction,
    EnvelopeExtractor,
    Modality,
    Transport,
)
from io_bridge.egress_rules import Silent, resolve_egress
from io_bridge.ingress_rules import AllowAll, DenyInbound, resolve_ingress
from io_bridge._engine import (
    BuildTransport,
    _BridgeState,
    _bridges,
    _next_corr,
    _state,
    _test_transport_inject,
    boot,
    dispatch,
    gate_inbound,
    make_verbs,
    on_delete,
    stamp_egress,
)
from io_bridge._transport import ConnectionClosed, MemoryTransport, _BaseTransport
from io_bridge._codec import decode_frame, encode_frame, find_bytes_path, set_path

__all__ = [
    # decision surface
    "Action",
    "Decision",
    "ALLOW",
    "IngressRule",
    "EgressRule",
    "parse_spec",
    "construct",
    "rule_name",
    "describe",
    # channel model
    "Channel",
    "CredentialExtractor",
    "EnvelopeExtractor",
    "Direction",
    "Modality",
    "Transport",
    # resolution + the non-colliding default rules (Password lives per-package)
    "resolve_ingress",
    "resolve_egress",
    "AllowAll",
    "DenyInbound",
    "Silent",
    # engine
    "make_verbs",
    "dispatch",
    "boot",
    "on_delete",
    "gate_inbound",
    "stamp_egress",
    "BuildTransport",
    "_BridgeState",
    "_bridges",
    "_state",
    "_next_corr",
    "_test_transport_inject",
    # transport contract + reference impl
    "MemoryTransport",
    "_BaseTransport",
    "ConnectionClosed",
    # binary-safe frame codec (shared by every transport)
    "encode_frame",
    "decode_frame",
    "find_bytes_path",
    "set_path",
]
