"""A backend's key, read from a credential store at USE time (`ai_core/credentials.py`).

Against a real `credential_store` (file backend under tmp_path): the reader asks AS
the agent, pinned to the endpoint's host, the answer marked secret; nothing is cached,
so a rotated key is used by the next request; no value lands in the ledger."""

from __future__ import annotations

import pytest
from _testkit import ensure_memory

from ai_core.credentials import CredentialError, key_reader

EP = "https://api.example.com/v1/responses"
KEY = "sk-live-0123456789"
KEY2 = "sk-rotated-9876543210"


async def _store_set(k, ref, host, value):
    out = (
        await k.send(
            [
                {
                    "target": "creds",
                    "payload": {
                        "type": "set",
                        "ref": ref,
                        "host": host,
                        "fields": {"api_key": value},
                    },
                    "secret": ["payload.fields"],
                }
            ]
        )
    )[0]
    assert out["status"] == "completed" and out["result"].get("stored"), out


async def _mk(k, tmp_path, *, readers=("ai",), host="api.example.com"):
    k.ctx.update(k.id, guid="g1")
    k.create(
        "credential_store.tools",
        id="creds",
        file=str(tmp_path / "creds.yaml"),
        callable_by={"ai": "*"},
    )
    await _store_set(k, "main", host, KEY)
    refs = k.ctx.agents["creds"].record["refs"]
    refs["main"]["readers"] = list(readers)
    k.ctx.update("creds", refs=refs)
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


async def test_the_provider_reads_the_key_per_request_and_records_none(
    seeded_kernel, tmp_path
):
    from responses_backend.tools import make_provider

    k = await _mk(seeded_kernel, tmp_path)
    prov = make_provider("ai", k)
    assert await prov._key(EP) == KEY
    await _store_set(k, "main", "api.example.com", KEY2)  # rotate
    assert await prov._key(EP) == KEY2, "a rotated key is used by the next request"
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0]
    assert KEY not in repr(ledger) and KEY2 not in repr(ledger)


async def test_a_key_bound_to_another_host_is_refused(seeded_kernel, tmp_path):
    k = await _mk(seeded_kernel, tmp_path)
    read = key_reader("ai", k, "responses_backend", required=True)
    with pytest.raises(CredentialError) as e:
        await read("https://evil.example.net/v1/responses")
    assert "bound to host 'api.example.com'" in str(e.value), e.value


async def test_an_agent_the_ref_does_not_name_reads_nothing(seeded_kernel, tmp_path):
    k = await _mk(seeded_kernel, tmp_path, readers=("someone_else",))
    read = key_reader("ai", k, "responses_backend", required=True)
    with pytest.raises(CredentialError) as e:
        await read(EP)
    assert str(e.value) == (
        "responses_backend: credentials creds/main: credential_store: no such "
        "ref 'main'"
    )


async def test_no_credentials_is_no_key_or_a_loud_refusal(seeded_kernel):
    k = seeded_kernel
    k.create(None, id="bare")
    assert await key_reader("bare", k, "vllm_backend", required=False)(EP) is None
    with pytest.raises(CredentialError) as e:
        await key_reader("bare", k, "responses_backend", required=True)(EP)
    assert "no `credentials` on bare" in str(e.value)
