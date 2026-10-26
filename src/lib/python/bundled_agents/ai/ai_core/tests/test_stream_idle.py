"""The read clock on a provider stream (#177): its default, the record's override,
the refusal of a bad value, and the stalled-stream failure."""

from __future__ import annotations

import pytest
from ai_core import ProviderError, stalled_error, stream_idle_of


def test_the_idle_window_defaults_and_takes_a_positive_number():
    assert stream_idle_of({}) == 60.0
    assert stream_idle_of({"stream_idle_seconds": None}) == 60.0
    assert stream_idle_of({"stream_idle_seconds": 2.5}) == 2.5
    assert stream_idle_of({"stream_idle_seconds": 3}) == 3.0


@pytest.mark.parametrize("bad", [0, -1, "x", True, float("nan")])
def test_a_window_that_is_not_a_positive_number_is_refused_by_name(bad):
    with pytest.raises(ProviderError) as e:
        stream_idle_of({"stream_idle_seconds": bad})
    assert str(e.value).startswith(
        "stream_idle_seconds must be a positive number of seconds"
    )
    assert e.value.transport is False


def test_a_stall_is_a_raise_naming_the_seconds_and_the_endpoint():
    err = stalled_error("x_backend", "http://e/v1", 0.2)
    msg = str(err)
    assert "stream_stalled" in msg and "http://e/v1" in msg
    assert "0.2s" in msg and "this turn is NOT complete" in msg
    assert msg.startswith("x_backend: stream_stalled")
    assert err.transport is True


def test_a_stalled_read_is_a_transport_failure():
    err = stalled_error("x", "http://e", 5.0)
    assert isinstance(err, ProviderError)
    assert err.transport is True
    assert "stream_stalled" in str(err)
