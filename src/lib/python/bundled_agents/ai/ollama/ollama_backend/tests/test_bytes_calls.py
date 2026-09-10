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


async def test_a_message_from_another_agent_opens_with_the_kernel_vouched_header(
    seeded_kernel, store_agent
):
    """The receiver sees WHO sent it, to WHOM, and under which number — from the
    dispatch context, not the sender's prose."""
    from kernel import _current_sender

    oid = await _make_ollama(seeded_kernel)
    # A REAL peer: the gate is two-sided, so the sender must exist, reach `oid`,
    # and be accepted by it — the operator (sender None) gets no header.
    seeded_kernel.create("", id="solver_1", can_call={oid: "*"})
    rec = seeded_kernel.get(oid) or {}
    await seeded_kernel.send(
        [
            {
                "target": seeded_kernel.id,
                "payload": {
                    "type": "update_agent",
                    "id": oid,
                    "callable_by": {**(rec.get("callable_by") or {}), "solver_1": "*"},
                },
            }
        ]
    )
    fp = _FakeProvider([['<call name="done"></call>']])
    ot._providers[oid] = fp
    token = _current_sender.set("solver_1")
    try:
        out = await seeded_kernel.send(
            [
                {
                    "target": oid,
                    "payload": {
                        "type": "send",
                        "text": "CANDIDATE 586",
                        "to": ["solver_2", oid],
                        "msg": 412,
                        "re": 398,
                    },
                }
            ]
        )
        assert await _until_calls(fp, 1), out
        incoming = fp.requests[0][-1]["content"]
        assert incoming.startswith(
            f"[from solver_1 · to solver_2, {oid} · msg 412 · re 398]\nCANDIDATE 586"
        ), incoming[:200]
    finally:
        _current_sender.reset(token)
        ot._providers.pop(oid, None)
