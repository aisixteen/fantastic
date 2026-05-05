"""Snapshot test for the inlined transport.js IIFE."""

from __future__ import annotations

from webapp._transport_js import TRANSPORT_JS


def test_iife_format():
    s = TRANSPORT_JS
    assert s.startswith("(function ()")
    assert s.rstrip().endswith("})();")


def test_exports_fantastic_transport():
    assert "window.fantastic_transport" in TRANSPORT_JS


def test_has_dispatcher_proxy():
    assert "dispatcher" in TRANSPORT_JS
    assert "Proxy" in TRANSPORT_JS


def test_has_watch_unwatch():
    assert "watch" in TRANSPORT_JS
    assert "unwatch" in TRANSPORT_JS


def test_has_event_listeners():
    assert "on(" in TRANSPORT_JS or "function on" in TRANSPORT_JS


def test_has_auto_reconnect():
    """transport.js must auto-reconnect on ws.onclose with backoff."""
    s = TRANSPORT_JS
    assert "ws.onclose" in s, "must hook onclose for reconnect"
    assert "reconnectDelay" in s, "must track backoff delay"
    assert "MAX_RECONNECT_DELAY" in s, "must cap the backoff"
    assert "setTimeout(connect" in s, "must reschedule connect()"


def test_replays_watch_on_reconnect():
    """On reopen, the new socket must re-issue every standing watch
    so observers don't lose their event flow silently."""
    s = TRANSPORT_JS
    assert "for (var src in watching)" in s
    assert "type: 'watch'" in s


def test_replays_state_subscribe_on_reconnect():
    """On reopen, if there are state subscribers, re-issue the
    state_subscribe frame so the new connection delivers a fresh
    snapshot and new events."""
    s = TRANSPORT_JS
    assert "stateHandlers.length > 0" in s
    assert "type: 'state_subscribe'" in s


def test_pending_calls_reject_on_disconnect():
    """t.call on a closed socket must reject fast (no silent hang)."""
    s = TRANSPORT_JS
    assert "rejectAllPending" in s
    assert "'disconnected'" in s


def test_lifecycle_hook_exposed():
    """onLifecycle is the API surface UIs use to clear stale state."""
    s = TRANSPORT_JS
    assert "onLifecycle" in s
    assert "fireLifecycle" in s
    assert "'connected'" in s
    assert "'disconnected'" in s


def test_call_returns_rejection_when_not_connected():
    """When connected=false, t.call returns a rejected Promise instead
    of trying to send and silently throwing inside the executor."""
    s = TRANSPORT_JS
    assert "Promise.reject(new Error('disconnected'))" in s
