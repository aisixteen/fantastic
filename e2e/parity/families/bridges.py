"""bridges — `ws_bridge` and `relay_connector` against a LIVE peer and a LIVE relay.

Both rounds pointed every bridge at `ws://127.0.0.1:9/nowhere` and measured one
thing: connect refused. Everything past a successful boot was source-only. Here
`stubs/ws_peer.py` and `stubs/relay.py` answer, so the capture reaches the parts
that only exist once a socket is up — the correlation id shape (`<bridge_id>-<n>`),
a reply that is an `error` with no error in it, a malformed frame followed by a
valid one, a peer that goes silent under a bounded forward, a peer that closes
mid-forward, an inbound call arriving FROM the peer, the relay's announce, a
`watch` reply with no data, a heartbeat, and a drop that has to show up in
`reflect` as `connected:false`.

Every frame on both sides is recorded, which is the point: a bridge is a codec
plus a correlation table, and neither is visible from the verb's reply alone.
"""

from __future__ import annotations

import time

from families import BASE, OPEN, call, save_kernel_log
from stubs.relay import Relay
from stubs.ws_peer import WSPeer

NAME = "bridges"


def run(ctx) -> None:  # noqa: C901
    rec, ports = ctx.rec, ctx.ports

    peer = WSPeer(rec, "bridges", ports.take("peer"), peer_id="peer")
    peer.set_script([
        {"do": "reply", "data": {"ok": 1, "note": "first forward"}},
        # A7.34 — an `error` frame with NO error field.
        {"do": "error_no_err"},
        # A7.19 — a malformed frame, then a valid reply on the same socket.
        {"do": "malformed", "data": {"ok": 2}},
        # A7.15/A7.26 — silence under a `timeout:0.5` forward.
        {"do": "silent"},
        # A7.3/A7.4 — an event with a null payload and an inbound call, pushed
        # before the reply so both arrive while a forward is outstanding.
        {"do": "reply", "data": {"ok": 3}, "push_before": [
            {"type": "event", "src": "peer", "payload": None},
            {"type": "call", "id": "peer-call-1", "target": "readings",
             "payload": {"type": "reflect"}},
        ]},
        # A7.32 — an inbound call aimed at the BRIDGE itself.
        {"do": "reply", "data": {"ok": 4}, "push_before": [
            {"type": "call", "id": "peer-call-2", "target": "wsb",
             "payload": {"type": "forward", "target": "readings",
                         "payload": {"type": "reflect"}}},
        ]},
        # A7.17/A7.30/A7.10 — the peer closes under a pending forward.
        {"do": "close", "code": 1001, "reason": "peer going away"},
    ])
    peer.start()

    relay = Relay(rec, "bridges", ports.take("relay"))
    relay.start()

    reach = ["readings", "wsb", "wsb_mem", "wsb_pass", "relay", "relay_nopartner",
             "web", "rest", "ws"]
    k = ctx.boot(reach=reach, port=ports.take("main"))
    cb = {**BASE, "rest": "*", "web": "*", "ws": "*"}
    try:
        (ctx.work / "a.txt").write_text("a\n")
        rec.json("setup/readings", k.create(parent_id=k.root_id, id="readings",
                                            handler_module="file_bridge.tools", root=".",
                                            **OPEN, callable_by=cb))
        # The live peer, both field spellings, exactly as `wire` writes them.
        rec.json("setup/wsb", k.create(
            parent_id=k.root_id, id="wsb", handler_module="ws_bridge.tools", **OPEN,
            callable_by=cb, can_call={**BASE, "readings": "*"},
            transport="ws", url=peer.url(), host="127.0.0.1", remote_port=peer.port,
            peer_id="peer"))
        # A7.x — `memory` transport has no dialer outside a test injection.
        rec.json("setup/wsb_mem", k.create(
            parent_id=k.root_id, id="wsb_mem", handler_module="ws_bridge.tools", **OPEN,
            callable_by=cb, transport="memory", peer_id="peer"))
        # A7.28 — a password egress rule, so the forward carries an auth token.
        rec.json("setup/wsb_pass", k.create(
            parent_id=k.root_id, id="wsb_pass", handler_module="ws_bridge.tools", **OPEN,
            callable_by=cb, can_call={**BASE, "readings": "*"},
            transport="ws", url=peer.url(), host="127.0.0.1", remote_port=peer.port,
            peer_id="peer", egress_rule="password", auth_token="parity-peer-token"))
        rec.json("setup/relay", k.create(
            parent_id=k.root_id, id="relay", handler_module="relay_connector.tools", **OPEN,
            callable_by=cb, can_call={**BASE, "readings": "*"},
            url=relay.url(), relay_url=relay.url(), guid="parity0000000000000000000000guid",
            partner_guid="stub-partner", heartbeat=1, reconnect=3,
            identity={"role": "parity"}))
        # A7.13/A7.14 — a relay with NO partner, then the directory verbs on it.
        rec.json("setup/relay_nopartner", k.create(
            parent_id=k.root_id, id="relay_nopartner", handler_module="relay_connector.tools",
            **OPEN, callable_by=cb, url=relay.url(), relay_url=relay.url()))
        time.sleep(2.0)

        # ── ws_bridge, against a peer that answers ──
        cases = [
            ("forward_ok", "wsb", {"type": "forward", "target": "readings",
                                   "payload": {"type": "reflect"}}),
            ("forward_error_no_error_field", "wsb", {"type": "forward", "target": "readings",
                                                     "payload": {"type": "reflect"}}),
            ("forward_after_malformed", "wsb", {"type": "forward", "target": "readings",
                                                "payload": {"type": "reflect"}}),
            ("forward_silent_timeout", "wsb", {"type": "forward", "target": "readings",
                                               "payload": {"type": "reflect"}, "timeout": 0.5}),
            ("forward_with_pushed_event_and_call", "wsb",
             {"type": "forward", "target": "readings", "payload": {"type": "reflect"}}),
            ("forward_with_inbound_bridge_call", "wsb",
             {"type": "forward", "target": "readings", "payload": {"type": "reflect"}}),
            ("forward_peer_closes", "wsb", {"type": "forward", "target": "readings",
                                            "payload": {"type": "reflect"}, "timeout": 5}),
            ("reflect_after_close", "wsb", {"type": "reflect"}),
            ("reconnect", "wsb", {"type": "reconnect"}),
            ("reflect_after_reconnect", "wsb", {"type": "reflect"}),
            ("forward_missing_target", "wsb", {"type": "forward",
                                               "payload": {"type": "reflect"}}),
            ("forward_payload_not_object", "wsb", {"type": "forward", "target": "readings",
                                                   "payload": "reflect"}),
            ("watch_remote", "wsb", {"type": "watch_remote", "target": "readings"}),
            ("unwatch_remote", "wsb", {"type": "unwatch_remote", "target": "readings"}),
            ("unknown_verb", "wsb", {"type": "nope"}),
            # A7.2/A7.5 — the DIRECTORY verbs, which belong to the relay, on a ws bridge.
            ("list_peers_on_ws_bridge", "wsb", {"type": "list_peers"}),
            ("watch_directory_on_ws_bridge", "wsb", {"type": "watch_directory"}),
            ("set_identity_on_ws_bridge", "wsb", {"type": "set_identity", "role": "x"}),
            ("boot_twice_1", "wsb", {"type": "boot"}),
            ("boot_twice_2", "wsb", {"type": "boot"}),
            ("memory_transport_boot", "wsb_mem", {"type": "boot"}),
            ("memory_transport_forward", "wsb_mem", {"type": "forward", "target": "readings",
                                                     "payload": {"type": "reflect"}}),
            ("password_egress_forward", "wsb_pass", {"type": "forward", "target": "readings",
                                                     "payload": {"type": "reflect"}}),
            ("password_egress_reflect", "wsb_pass", {"type": "reflect"}),
        ]
        for case, target, payload in cases:
            rec.json(f"ws_bridge/{case}", {"target": target, "payload": payload,
                                           "reply": call(k, target, payload, timeout=40)})
            time.sleep(0.3)

        # A7.33 — delete the bridge with a forward still pending.
        rec.json("ws_bridge/delete_with_pending", call(k, k.root_id,
                                                       {"type": "delete_agent", "id": "wsb_pass"}))
        rec.json("ws_bridge/send_after_delete", call(k, "wsb_pass", {"type": "reflect"}))

        # ── relay_connector, against a relay that answers ──
        rcases = [
            ("reflect_connected", "relay", {"type": "reflect"}),
            ("list_peers", "relay", {"type": "list_peers"}),
            ("watch_directory", "relay", {"type": "watch_directory"}),
            ("unwatch_directory", "relay", {"type": "unwatch_directory"}),
            ("set_identity_role_x", "relay", {"type": "set_identity", "role": "x"}),
            ("set_identity_attrs", "relay", {"type": "set_identity",
                                             "identity": {"role": "parity2"}}),
            ("forward_to_partner", "relay", {"type": "forward", "target": "readings",
                                             "payload": {"type": "reflect"}, "timeout": 5}),
            ("reflect_after_forward", "relay", {"type": "reflect"}),
            ("unknown_verb", "relay", {"type": "nope"}),
            ("boot_twice_1", "relay", {"type": "boot"}),
            ("boot_twice_2", "relay", {"type": "boot"}),
            # A7.13/A7.14 — no partner_guid at all.
            ("nopartner_reflect", "relay_nopartner", {"type": "reflect"}),
            ("nopartner_list_agents", "relay_nopartner", {"type": "list_agents"}),
            ("nopartner_forward", "relay_nopartner", {"type": "forward", "target": "readings",
                                                      "payload": {"type": "reflect"},
                                                      "timeout": 2}),
            # A7.15/A7.25 — a forward BEFORE boot, and unwatch before boot.
            ("nopartner_unwatch_before_boot", "relay_nopartner", {"type": "unwatch_directory"}),
        ]
        for case, target, payload in rcases:
            rec.json(f"relay/{case}", {"target": target, "payload": payload,
                                       "reply": call(k, target, payload, timeout=40)})
            time.sleep(0.3)

        # A7.18/A7.31 — let the 1 s heartbeat run, then A7.22: drop the socket and
        # ask reflect what it says about a connection that is gone.
        time.sleep(4.0)
        rec.json("relay/reflect_after_heartbeats", call(k, "relay", {"type": "reflect"}))
        relay.drop_after = 1
        rec.json("relay/forward_into_drop", call(k, "relay",
                                                 {"type": "forward", "target": "readings",
                                                  "payload": {"type": "reflect"}, "timeout": 3},
                                                 timeout=40))
        time.sleep(2.0)
        rec.json("relay/reflect_after_drop", call(k, "relay", {"type": "reflect"}))
        rec.json("relay/list_agents_after_drop", call(k, "kernel", {"type": "list_agents"}))
        rec.json("ws_peer_partner_events", relay.partner_events)
    finally:
        peer.flush()
        relay.flush()
        rec.json("peer_meta", {"connections": peer.connections, "relay_connections":
                               relay.connections})
        rec.http("http", k.http)
        k.close()
        peer.stop()
        relay.stop()
        save_kernel_log(rec, k)
