"""Unit tests of the probe's `Watch`: a failed ws watch raises its error, never waits out the bound."""

from __future__ import annotations

import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
import probe  # noqa: E402


class FakeWatcher:
    """A WSWatcher stand-in: `connect` may fail, `events` yields `evs` then raises or ends."""

    connect_error: Exception | None = None
    evs: list = []
    stream_error: Exception | None = None

    def __init__(self, port, aid):
        pass

    async def connect(self):
        if self.connect_error:
            raise self.connect_error

    async def events(self):
        for e in self.evs:
            yield e
        if self.stream_error:
            raise self.stream_error


def watch(**attrs) -> "probe.Watch":
    fake = type("F", (FakeWatcher,), attrs)
    with mock.patch.object(probe, "WSWatcher", fake):
        w = probe.Watch(0, "x")
        w.ready.wait(5)
        w.thread.join(5)
    return w


class WatchTests(unittest.TestCase):
    def test_connect_failure_raises_everywhere(self):
        w = watch(connect_error=OSError("refused"))
        with self.assertRaises(OSError):
            w.ready_or_raise()
        with self.assertRaises(OSError):
            w.events()
        with self.assertRaises(OSError):
            w.wait_for(lambda e: True, timeout=0.01)

    def test_mid_stream_exception_keeps_events_and_raises(self):
        w = watch(evs=[{"type": "a"}], stream_error=RuntimeError("boom"))
        self.assertEqual(w.count(lambda e: True), 1)
        with self.assertRaises(RuntimeError):
            w.events()
        with self.assertRaises(RuntimeError):
            w.wait_for(lambda e: e.get("type") == "never", timeout=0.01)
        self.assertTrue(w.wait_for(lambda e: e.get("type") == "a"))

    def test_stream_end_is_a_connection_error(self):
        w = watch(evs=[{"type": "a"}, {"type": "a"}])
        with self.assertRaises(ConnectionError):
            w.events()
        with self.assertRaises(ConnectionError):
            w.wait_for(lambda e: False, timeout=0.01)
        self.assertTrue(w.wait_count(lambda e: True, 2))
        with self.assertRaises(ConnectionError):
            w.wait_count(lambda e: True, 3, timeout=0.01)


if __name__ == "__main__":
    unittest.main()
