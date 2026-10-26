"""A backend's key, read from a credential store at USE time (`ai_core/credentials.py`).

Against a real `credential_store` (file backend under tmp_path): the reader asks AS
the agent, pinned to the endpoint's host, the answer marked secret; nothing is cached,
so a rotated key is used by the next request; no value lands in the ledger."""

from __future__ import annotations

from _testkit import as_agent

import pytest
from _testkit import ensure_memory

from ai_core.credentials import CredentialError, key_reader

EP = "https://api.example.com/v1/responses"
KEY = "sk-live-0123456789"
KEY2 = "sk-rotated-9876543210"


async def _store_set(k, ref, value):
    out = (
        await k.send(
            [
                {
                    "target": "creds",
                    "payload": {
                        "type": "set",
                        "ref": ref,
                        "fields": {"api_key": value},
                    },
                    "secret": ["payload.fields"],
                }
            ]
        )
    )[0]
    assert out["status"] == "completed" and out["result"].get("stored"), out


async def _mk(k, tmp_path, *, accept=("ai",), host="api.example.com"):
    k.ctx.update(k.id, guid="g1")
    k.create(
        "credential_store.tools",
        id="creds",
        file=str(tmp_path / "creds.yaml"),
        refs={"main": {"host": host}},  # declared
        callable_by={a: "*" for a in accept},
    )
    await _store_set(k, "main", KEY)
    rec = k.create(
        "responses_backend.tools",
        id="ai",
        endpoint=EP,
        model="m",
        memory_store_id=ensure_memory(k),
        credentials={"store": "creds", "ref": "main"},
        can_call={"creds": "*"},
    )
    assert rec.get("id") == "ai", rec
    return k


async def test_the_reader_reads_the_key_per_request_and_records_none(
    seeded_kernel, tmp_path
):
    from responses_backend.tools import make_provider

    k = await _mk(seeded_kernel, tmp_path)
    prov = make_provider("ai", k)
    assert await prov._key(EP) == KEY
    await _store_set(k, "main", KEY2)  # rotate
    assert await prov._key(EP) == KEY2, "a rotated key is used by the next request"
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert KEY not in repr(ledger) and KEY2 not in repr(ledger)


async def test_a_key_bound_to_another_host_is_refused(seeded_kernel, tmp_path):
    k = await _mk(seeded_kernel, tmp_path)
    read = key_reader("ai", k, "responses_backend", required=True)
    with pytest.raises(CredentialError) as e:
        await read("https://evil.example.net/v1/responses")
    assert "bound to host 'api.example.com'" in str(e.value), e.value


async def test_an_agent_the_store_does_not_accept_reads_nothing(
    seeded_kernel, tmp_path
):
    k = await _mk(seeded_kernel, tmp_path, accept=("someone_else",))
    read = key_reader("ai", k, "responses_backend", required=True)
    # the read runs in the agent's own turn — the test stands in for it
    with pytest.raises(CredentialError) as e, as_agent(k, "ai"):
        await read(EP)
    assert "creds does not accept 'get' from ai" in str(e.value), e.value


async def test_no_credentials_is_no_key_or_a_loud_refusal(seeded_kernel):
    k = seeded_kernel
    k.create(None, id="bare")
    assert await key_reader("bare", k, "vllm_backend", required=False)(EP) is None
    with pytest.raises(CredentialError) as e:
        await key_reader("bare", k, "responses_backend", required=True)(EP)
    assert "no `credentials` on bare" in str(e.value)


@pytest.mark.parametrize(
    "bad",
    [
        KEY + "\n",
        KEY + "\r\nX-Probe: y",
        KEY + " ",
        " " + KEY,
        KEY + "\t",
        KEY[:4] + "\x00" + KEY[4:],
        KEY[:4] + "\x7f" + KEY[4:],
        KEY + "é",
    ],
)
async def test_a_key_that_is_not_a_legal_header_value_is_refused_by_its_ref(
    seeded_kernel, tmp_path, bad
):
    """An HTTP client QUOTES a header it refuses (h11: `Illegal header
    value b'Bearer <key>\\n'`), so a key pasted with a trailing newline would ride
    that text into the caller's reply and the logs. The reader refuses it first,
    naming the ref — never the value — and the store keeps whatever it was given
    (a field may be a multi-line PEM)."""
    k = await _mk(seeded_kernel, tmp_path)
    await _store_set(k, "main", bad)
    read = key_reader("ai", k, "responses_backend", required=True)
    with pytest.raises(CredentialError) as e:
        await read(EP)
    assert str(e.value) == (
        "responses_backend: credential 'main' in 'creds': 'api_key' is not a legal "
        "HTTP header value (a control or non-ASCII character, or whitespace at an "
        "edge) — store it again without"
    )
    assert KEY[4:] not in str(e.value)


async def test_a_key_with_inner_spaces_is_a_legal_header_value(seeded_kernel, tmp_path):
    k = await _mk(seeded_kernel, tmp_path)
    await _store_set(k, "main", "sk a\tb")
    assert await key_reader("ai", k, "responses_backend", required=True)(EP) == (
        "sk a\tb"
    )


async def test_a_malformed_record_or_a_missing_field_is_named(seeded_kernel, tmp_path):
    k = await _mk(seeded_kernel, tmp_path)
    read = key_reader("ai", k, "responses_backend", required=True)
    # a record without `ref`
    k.ctx.update("ai", credentials={"store": "creds"})
    with pytest.raises(CredentialError) as e:
        await read(EP)
    assert str(e.value) == (
        "responses_backend: credentials must be {store: <credential_store id>, ref: <ref>}"
    )
    # a ref with no `api_key` field: declared by the operator, then stored
    k.ctx.update("ai", credentials={"store": "creds", "ref": "other"})
    k.ctx.update(
        "creds",
        refs={
            "main": {"host": "api.example.com"},
            "other": {"host": "api.example.com"},
        },
    )
    out = (
        await k.send(
            [
                {
                    "target": "creds",
                    "payload": {
                        "type": "set",
                        "ref": "other",
                        "fields": {"token": "t"},
                    },
                    "secret": ["payload.fields"],
                }
            ]
        )
    )[0]
    assert out["result"].get("stored"), out
    with pytest.raises(CredentialError) as e:
        await read(EP)
    assert str(e.value) == (
        "responses_backend: credential 'other' in 'creds' has no 'api_key' field"
    )
