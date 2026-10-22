"""A stopped turn stores its partial answer and the stop notice APART.

The model's visible partial text is an assistant row with no stop marker in it;
the notice naming who stopped the turn is a separate `stopped` user row."""

import asyncio

from _testkit import memory_meta

from ai_core import core


class _Streamer:
    """Streams a partial answer, then stalls so the turn can be cut mid-answer."""

    def __init__(self, partial: str):
        self.partial = partial
        self.calls = 0

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        yield self.partial
        await asyncio.sleep(5)
        yield " never seen"


async def test_stopped_turn_stores_partial_answer_and_notice_apart(
    seeded_kernel, fake_providers
):
    aid = "stop_rows_a"
    fp = _Streamer("half an answer")
    seeded_kernel.create(
        "ollama_backend.tools",
        id=aid,
        context_window=32768,
        **memory_meta(seeded_kernel, aid, {"cli": "*"}),
    )
    fake_providers[aid] = fp
    try:
        turn = asyncio.ensure_future(
            seeded_kernel.send(
                [{"target": aid, "payload": {"type": "send", "text": "the question", "client_id": "tab9"}}]
            )
        )
        for _ in range(100):
            if fp.calls:
                break
            await asyncio.sleep(0.02)
        await asyncio.sleep(0.15)
        # stop the in-flight turn through the normal stop verb (not a host drain)
        await seeded_kernel.send([{"target": aid, "payload": {"type": "stop"}}])
        await asyncio.wait_for(turn, 5)
        rows = await core._load_history(aid, seeded_kernel)
        # the question, the partial answer as the MODEL's row, then the notice as a USER row
        assert [r["role"] for r in rows[-3:]] == ["user", "assistant", "user"], rows
        question, answer, notice = rows[-3], rows[-2], rows[-1]
        assert question["content"] == "the question", question
        assert answer["content"] == "half an answer", answer
        assert "stopped" not in answer["content"], answer
        assert notice.get("kind") == "stopped", notice
        assert notice["role"] == "user", notice
        assert "stopped by" in notice["content"], notice
    finally:
        fake_providers.pop(aid, None)
