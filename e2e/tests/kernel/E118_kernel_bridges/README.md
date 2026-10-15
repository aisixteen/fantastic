# `kernel_bridges` (E118) — two kernels, bridged

Free: no model. Two PACKAGED kernels, both spawned and driven over `web_rest`: the
host of `RUNTIME` and a peer (python; rust when `RUNTIME=python`). Each holds a
`ws_bridge` (`br`) dialing the other's `web_ws` leg (`/ws/ws`) and a `file_bridge`
(`fb`, root `data/`), so both directions run — the runtime's bridge as the client
and the runtime's `web_ws` as the server.

What it proves, per direction: the bridge is up on create; `forward` reflect returns
the REMOTE root (its guid); `forward write` lands on the remote disk; `read_stream`
pulled to `eof` across the bridge is byte-equal (raw bytes, REST octet leg); a
BINARY ws frame calling `br forward write_stream` lands its raw body on the remote
disk byte-equal; `watch_remote` re-emits the remote file_bridge's `changed` event on
the local bridge with `src`; an agent the remote ws leg holds no grant for is refused
`unauthorized` there.

Then the runtime's bridge alone: a bridge to a dead port fails `boot` bounded and
refuses `forward` `not_connected`; killing the peer flips `connected:false` and a
`forward` is refused promptly; the peer back on the same port stays unconnected (the
readme: no auto-reconnect) until `reconnect`, after which `forward` reaches it.

`relay_connector`: both kernels' connectors dial a stub relay (`stub_relay.py` — the
relay's `send`→`event` routing, text and binary frames kept) with the password read
from their `creds` store (`X-Fantastic-Auth`) and the `fantastic.relay.v1`
subprotocol; `forward` runs both ways; a `read_stream` chunk crosses as a binary relay
frame; sealing the serving connector (`ingress_rule: deny_ingress`) refuses the
tunneled call `unauthorized` live, and reopening serves again.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id kernel_bridges --lanes 3   # all three
RUNTIME=swift uv run python ../../../e2e/tests/kernel/E118_kernel_bridges/probe.py
```
