from ai_core import core as ot
from test_send_to import _FakeProvider, _make_ollama, _until_calls  # noqa


async def test_a_bytes_call_does_not_break_the_turn(seeded_kernel, store_agent):
    oid = await _make_ollama(
        seeded_kernel, grants={store_agent: ["write_stream", "read_stream"]}
    )
    call = (
        f'<call name="send"><target_id>{store_agent}</target_id><type>write_stream</type>'
        '<path>p.py</path><truncate>true</truncate><bytes>print("hi")\n</bytes></call>'
    )
    fp = _FakeProvider([[call], ['<call name="done"></call>']])
    ot._providers[oid] = fp
    try:
        r = (
            await seeded_kernel.send(
                [{"target": oid, "payload": {"type": "send", "text": "go"}}]
            )
        )[0]
        assert await _until_calls(fp, 2), r
        print("TURN2:", str(fp.requests[1])[-600:])
        assert "written" in str(fp.requests[1]) or "size" in str(fp.requests[1]), str(
            fp.requests[1]
        )[-600:]
    finally:
        ot._providers.pop(oid, None)


async def test_a_read_stream_reply_with_bytes_renders(seeded_kernel, store_agent):
    oid = await _make_ollama(
        seeded_kernel, grants={store_agent: ["write_stream", "read_stream"]}
    )
    w = (
        f'<call name="send"><target_id>{store_agent}</target_id><type>write_stream</type>'
        "<path>q.txt</path><truncate>true</truncate><bytes>hello bytes\n</bytes></call>"
    )
    r = (
        f'<call name="send"><target_id>{store_agent}</target_id><type>read_stream</type>'
        "<path>q.txt</path><offset>0</offset></call>"
    )
    fp = _FakeProvider([[w], [r], ['<call name="done"></call>']])
    ot._providers[oid] = fp
    try:
        await seeded_kernel.send(
            [{"target": oid, "payload": {"type": "send", "text": "go"}}]
        )
        assert await _until_calls(fp, 3), fp.calls
        t3 = str(fp.requests[2])
        print("TURN3:", t3[-700:])
        assert "hello bytes" in t3, t3[-700:]
    finally:
        ot._providers.pop(oid, None)
