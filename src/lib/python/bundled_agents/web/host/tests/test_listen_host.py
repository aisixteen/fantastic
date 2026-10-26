"""The listener's address: the record's `bind`, else its `host`, else loopback. The
Host/Origin guard keeps judging by `host` (`test_origin_guard.py`)."""

from __future__ import annotations

from web.app import own_authorities, own_origins
from web.tools import listen_host


def test_the_listener_binds_the_bind_field_and_the_guard_keeps_the_host():
    assert listen_host({}) == "127.0.0.1"
    assert listen_host({"host": "192.168.1.5"}) == "192.168.1.5"
    assert listen_host({"bind": "0.0.0.0"}) == "0.0.0.0"
    # a bind beats a host: the publish is the listener, the host stays the guard's identity
    both = {"bind": "0.0.0.0", "host": "192.168.1.5"}
    assert listen_host(both) == "0.0.0.0"
    assert own_authorities({**both, "port": 8088}) == {"192.168.1.5:8088"}
    # an empty bind is no bind, as python's `or` reads it
    assert listen_host({"bind": "", "host": "0.0.0.0"}) == "0.0.0.0"


def test_a_wildcard_bind_keeps_the_loopback_identity_the_guard_admits():
    rec = {"bind": "0.0.0.0", "port": 8088}
    assert listen_host(rec) == "0.0.0.0"
    assert "localhost:8088" in own_authorities(rec)
    assert "http://localhost:8088" in own_origins(rec)


def test_wildcard_bind_guard_admits_loopback_host():
    # the container's listener: bind 0.0.0.0, and a browser at localhost:<port>
    # (through a 127.0.0.1 port map or ssh -L) is admitted; a LAN name is not
    rec = {"bind": "0.0.0.0", "port": 8088}
    authorities = own_authorities(rec)
    assert {"localhost:8088", "127.0.0.1:8088", "[::1]:8088"} <= authorities
    assert "192.168.1.5:8088" not in authorities
    assert "http://localhost:8088" in own_origins(rec)
