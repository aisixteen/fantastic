"""io_bridge channel model — the typed descriptor of a sealed IO edge.

A CHANNEL is a sealed edge described by five orthogonal facts:

    direction : ingress (inbound) | egress (outbound) | duplex
    modality  : message (framed)  | stream (octet)
    transport : ws | http | cloud | memory | cli | fs
    rule      : IngressRule (ingress) / EgressRule (egress) — the authz DECISION
    extractor : CredentialExtractor — pulls the credential off the frame/request

The decision (`rule.authorize`) is identical for every channel; only the credential
EXTRACTOR varies, and it varies by MODALITY, not direction:

  - MESSAGE channels carry the credential on the frame ENVELOPE (`auth_token`),
    gated PER-FRAME. The default extractor is `EnvelopeExtractor`.
  - STREAM/octet channels gate the OPEN (a signed capability / header), then bytes
    flow ungated. Their extractor reads the open request, not each byte.

Add an endpoint type = supply its transport + a `CredentialExtractor`; the rule and
the registry are reused untouched. This module declares the contract; the message
default ships here, and the http/stream/cli extractors land as they are wired.
"""

from __future__ import annotations

from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Literal

from io_bridge._base import EgressRule, IngressRule

Direction = Literal["ingress", "egress", "duplex"]
Modality = Literal["message", "stream"]
Transport = Literal["ws", "http", "cloud", "memory", "cli", "fs"]


class CredentialExtractor(ABC):
    """Pulls the credential a peer presented off the modality-specific carrier — the
    frame envelope (message), the request header/URL (http/stream), or the transport
    itself (cli locality). Externalizes the hardcoded `frame.get("auth_token")` so a
    new endpoint type only supplies its own extractor; the rule never changes."""

    @abstractmethod
    def extract(self, carrier) -> str | None: ...


class EnvelopeExtractor(CredentialExtractor):
    """Message default: the credential rides the frame ENVELOPE (a sibling of
    `id`/`target`), NEVER the dispatched payload — so the target agent never sees it.
    `carrier` is the inbound frame dict."""

    def extract(self, carrier) -> str | None:
        if isinstance(carrier, dict):
            tok = carrier.get("auth_token")
            return tok if isinstance(tok, str) else None
        return None


@dataclass(frozen=True)
class Channel:
    """A sealed edge: a transport face bound to its per-direction rule + the extractor
    that reads credentials off that transport's carrier. The leg consults
    `rule.authorize` (ingress) / `rule.credential` (egress) at the choke point; the
    `extractor` turns a raw frame/request into the `Action.token` the rule checks."""

    direction: Direction
    modality: Modality
    transport: Transport
    rule: IngressRule | EgressRule
    extractor: CredentialExtractor
