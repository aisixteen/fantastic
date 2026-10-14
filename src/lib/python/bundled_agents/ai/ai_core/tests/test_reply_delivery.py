"""Every call in a turn gets its reply — delivered TOGETHER, as one message.

Two defects pinned here:

replies delivered SEQUENTIALLY, each delivery blocked on a whole model turn — so
calls go unanswered when the chain ends first;

one reply per turn — a turn that made 7 calls spends the next 7 turns collecting
its own answers (`1 of 7` … `7 of 7`), and a question asked in between is answered
by something older.

The contract: ALL of one turn's results arrive as ONE delivered message
per `reply_to` destination — one `[reply to …]` block per call, in emission
order. The model never sees a partial batch, so it never re-asks.
"""

from __future__ import annotations

import asyncio

from ai_core.core import _route_reply
from ai_core.tool_parse import render_call


def _call(target: str, verb: str = "reflect", reply_to: str = "self") -> str:
    return render_call(
        "send",
        {"target_id": target, "payload": {"type": verb}, "reply_to": reply_to},
    )


class _Kernel:
    """Array-shaped stub: one outcome per item, pid minted at dispatch. `sent`
    records deliveries (the `type: send` handoffs to the AI agent); `batches`
    records each kernel batch as the list of its targets."""

    def __init__(self):
        self.sent: list[tuple[str, dict]] = []
        self.batches: list[list[str]] = []
        # the Agent's `.ctx` is the Kernel; this fake plays both
        self.ctx = self

    def get(self, _id):
        return {}

    def _verb_docs(self, payload):
        """The kernel's `docs`, read in process by routing: this fake documents
        no verb, so no routing field clashes."""
        return {"of": payload.get("of"), "verbs": {}}

    def spawn_task(self, _agent_id, _verb, coro):
        """Deliveries are FORKED through the kernel (ai_core has one path, no
        unregistered fallback) — run the work as a task, like the kernel does."""
        self._tasks = getattr(self, "_tasks", [])
        self._tasks.append(asyncio.get_running_loop().create_task(coro))
        return len(self._tasks)

    async def send(self, items):
        outs = []
        if any(i["payload"].get("type") != "send" for i in items):
            self.batches.append([i["target"] for i in items])
        for it in items:
            if it["payload"].get("type") == "send":
                self.sent.append((it["target"], it["payload"]))
                outs.append({"pid": None, "status": "completed", "result": None})
            else:
                outs.append(
                    {
                        "pid": f"{it['target']}:pid",
                        "status": "completed",
                        "result": {"ok": it["target"]},
                    }
                )
        return outs


async def _settle(k: _Kernel, want: int, timeout: float = 2.0) -> None:
    """Deliveries are spawned, not awaited — wait for them to land."""
    began = asyncio.get_event_loop().time()
    while len(k.sent) < want:
        if asyncio.get_event_loop().time() - began > timeout:
            raise AssertionError(f"delivered {len(k.sent)}, wanted {want}")
        await asyncio.sleep(0.005)


async def test_a_turn_of_four_calls_is_answered_in_one_message():
    """Four calls out → ONE delivered message holding four tagged blocks, in
    the model's own emission order."""
    k = _Kernel()
    text = _call("kernel") + _call("readings") + _call("totals") + _call("runner")
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 1)

    assert k.batches == [["kernel", "readings", "totals", "runner"]], (
        "all four calls must dispatch in ONE kernel batch"
    )
    assert len(k.sent) == 1, "one destination -> exactly one delivered message"
    target, payload = k.sent[0]
    assert target == "ai"
    blocks = payload["text"].split("\n\n")
    assert len(blocks) == 4
    for n, (block, agent) in enumerate(
        zip(blocks, ["kernel", "readings", "totals", "runner"]), 1
    ):
        assert block.startswith(f"[reply to {agent}:pid · {agent} reflect · {n} of 4")


async def test_two_destinations_get_two_messages_with_their_own_counts():
    """Per-call `reply_to` splits the batch: each destination's M counts only
    what IT receives; both messages share the turn's batch token."""
    k = _Kernel()
    text = _call("kernel") + _call("totals", reply_to="scribe") + _call("readings")
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 2)

    by_dest = {t: p["text"] for t, p in k.sent}
    assert set(by_dest) == {"ai", "scribe"}
    assert " · 1 of 2 in batch " in by_dest["ai"]
    assert " · 2 of 2 in batch " in by_dest["ai"]
    assert "in batch" not in by_dest["scribe"], "a lone block carries no suffix"
    token = by_dest["ai"].split("in batch ")[1][:4]
    assert token  # the same turn token names the round on the multi-block side


async def test_fire_and_forget_still_dispatches_but_is_not_counted():
    """`reply_to:null` runs the call and discards the answer — it appears in
    the kernel batch and in NO delivered message, and M ignores it."""
    k = _Kernel()
    text = _call("kernel") + _call("totals", reply_to="null")
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 1)

    assert k.batches == [["kernel", "totals"]], "the discarded call still ran"
    assert len(k.sent) == 1
    body = k.sent[0][1]["text"]
    assert "totals" not in body
    assert "in batch" not in body, "one awaited call -> no batch suffix"


async def test_the_layer_default_timeout_rides_every_item():
    """The AI layer ALWAYS supplies a per-call deadline (the kernel defaults
    nothing), and the model's own `timeout` arg overrides it for that call."""
    seen: list[tuple[str, object]] = []

    class _K(_Kernel):
        async def send(self, items):
            for it in items:
                if it["payload"].get("type") != "send":
                    seen.append((it["target"], it.get("timeout")))
            return await super().send(items)

    k = _K()
    with_own = render_call(
        "send",
        {
            "target_id": "slowpoke",
            "payload": {"type": "crunch"},
            "reply_to": "self",
            "timeout": 5,
        },
    )
    await _route_reply("ai", k, _call("kernel") + with_own, "cli", None, None, 42.0, "")
    await _settle(k, 1)
    assert seen == [("kernel", 42.0), ("slowpoke", 5)]


async def test_a_failed_outcome_is_rendered_not_hidden():
    """A call that failed or timed out occupies its slot in M with its error
    body — a partial batch is the one thing the model must never see."""

    class _K(_Kernel):
        async def send(self, items):
            outs = []
            for it in items:
                if it["payload"].get("type") == "send":
                    self.sent.append((it["target"], it["payload"]))
                    outs.append({"pid": None, "status": "completed", "result": None})
                elif it["target"] == "hung":
                    outs.append(
                        {
                            "pid": "hung:1",
                            "status": "timed_out",
                            "result": {
                                "error": "no reply after 42s",
                                "reason": "timeout",
                            },
                        }
                    )
                else:
                    outs.append(
                        {
                            "pid": f"{it['target']}:pid",
                            "status": "completed",
                            "result": {"ok": True},
                        }
                    )
            return outs

    k = _K()
    await _route_reply(
        "ai", k, _call("hung") + _call("kernel"), "cli", None, None, 42.0, ""
    )
    await _settle(k, 1)
    body = k.sent[0][1]["text"]
    blocks = body.split("\n\n")
    assert len(blocks) == 2
    assert blocks[0].startswith("[reply to hung:1 · hung reflect · 1 of 2")
    assert "no reply after 42s" in blocks[0]
    assert blocks[1].startswith("[reply to kernel:pid · kernel reflect · 2 of 2")


async def test_the_models_exec_id_rides_the_item_as_its_id():
    """`<exec_id>` is the call-level execution NAME; core hands it to the kernel as
    the batch item's `id` (a bare `<id>` would collide with payload ids)."""

    class _K(_Kernel):
        def __init__(self):
            super().__init__()
            self.items = []

        async def send(self, batch):
            self.items.extend(batch)
            return await super().send(batch)

    k = _K()
    named = render_call(
        "send",
        {
            "target_id": "vault",
            "payload": {"type": "withdraw"},
            "reply_to": "self",
            "exec_id": "job-alpha",
            "retries": 2,
        },
    )
    await _route_reply("ai", k, named, "cli", None, None, 42.0, "")
    assert k.items[0]["id"] == "job-alpha" and k.items[0]["retries"] == 2
    assert k.items[0]["payload"] == {"type": "withdraw"}


async def test_to_fans_out_one_copy_per_recipient_carrying_the_list_and_msg():
    """`<to>` becomes N kernel items; every copy carries the whole list, the sending
    turn's id (`msg`) and the answered message (`re`)."""

    class _K(_Kernel):
        def __init__(self):
            super().__init__()
            self.items = []

        async def send(self, batch):
            self.items.extend(batch)
            return await super().send(batch)

    k = _K()
    call = render_call(
        "send",
        {
            "to": ["solver_2", "solver_3"],
            "re": 398,
            "reply_to": None,
            "payload": {"type": "send", "text": "CANDIDATE 586"},
        },
    )
    await _route_reply("ai", k, call, "cli", None, None, 42.0, "", message_id=412)
    peers = [i for i in k.items if i["target"] in ("solver_2", "solver_3")]
    assert [i["target"] for i in peers] == ["solver_2", "solver_3"]
    for i in peers:
        assert i["payload"]["to"] == ["solver_2", "solver_3"]
        assert i["payload"]["msg"] == 412 and i["payload"]["re"] == 398
        assert i["payload"]["text"] == "CANDIDATE 586"


class _SizedKernel(_Kernel):
    """Each call answers `{"text": <n chars>}`; the agent's window is small, so its
    inline cap is a few hundred tokens."""

    def __init__(self, chars: int):
        super().__init__()
        self.chars = chars

    def get(self, _id):
        return {"context_window": 4500, "output_reserve": 0}

    async def send(self, items):
        outs = await super().send(items)
        for n, (it, o) in enumerate(zip(items, outs)):
            if it["payload"].get("type") != "send":
                o["pid"] = (
                    900 + n
                )  # an int pid, as the kernel mints — a body is held by it
                o["result"] = {"text": it["target"][0] * self.chars}
        return outs


async def test_the_inline_cap_bounds_the_whole_batch_not_each_reply():
    """Two replies, each under the cap, arrive as ONE message the next turn may not
    fit. The replies of a batch share the cap: the first fits and is inline, the one
    that would push the message past it is held, like an oversized reply."""
    from ai_core.context import estimate_one
    from ai_core.core import _inline_cap

    cap = _inline_cap({"context_window": 4500, "output_reserve": 0})
    k = _SizedKernel(chars=int(cap * 4 * 0.6))  # each reply ~60% of the cap
    await _route_reply(
        "ai", k, _call("alpha") + _call("beta"), "cli", None, None, 180.0, ""
    )
    await _settle(k, 1)
    first, second = k.sent[0][1]["text"].split("\n\n")
    assert "a" * 100 in first and "NOT inlined" not in first, "the first reply fits"
    assert "NOT inlined" in second and "b" * 100 not in second, "the second is held"
    assert estimate_one({"role": "user", "content": k.sent[0][1]["text"]}) <= cap + 64


async def test_replies_that_fit_together_are_all_inline():
    from ai_core.core import _inline_cap

    cap = _inline_cap({"context_window": 4500, "output_reserve": 0})
    k = _SizedKernel(chars=int(cap * 4 * 0.3))  # two together ~60% of the cap
    await _route_reply(
        "ai", k, _call("alpha") + _call("beta"), "cli", None, None, 180.0, ""
    )
    await _settle(k, 1)
    assert "NOT inlined" not in k.sent[0][1]["text"]


class _KnownKernel(_Kernel):
    """Knows only the ids it is given — `get` answers None for anything else."""

    def __init__(self, ids):
        super().__init__()
        self.ids = set(ids)

    def get(self, _id):
        return {} if _id in self.ids else None


async def test_a_reply_to_naming_no_agent_dispatches_nothing_and_says_so():
    """`<reply_to>1001</reply_to>` meaning a chat message id would send the call's
    reply to agent "1001", which does not exist, and the model would never see it.
    That call is NOT dispatched and the
    model is told why, in the same delivered message as the turn's other replies."""
    k = _KnownKernel({"ai", "chat", "kernel"})
    text = _call("chat", "post", reply_to="1001") + _call("kernel")
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 1)

    assert k.batches == [["kernel"]], "the call with a dead reply_to must not run"
    assert len(k.sent) == 1 and k.sent[0][0] == "ai"
    body = k.sent[0][1]["text"]
    assert "reply_to='1001' names no agent" in body
    assert "nothing was dispatched to 'chat'" in body
    assert body.count("[reply to ") == 2, "both calls answered, in one message"


async def test_a_reply_to_naming_a_live_agent_or_a_peer_still_routes():
    """A known agent, the reserved `kernel` address and a peer address pass."""
    k = _KnownKernel({"ai", "chat", "scribe"})
    text = (
        _call("chat", reply_to="scribe")
        + _call("chat", reply_to="kernel")
        + _call("chat", reply_to="g1|scribe")
    )
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 3)

    assert k.batches == [["chat", "chat", "chat"]]
    assert {t for t, _ in k.sent} == {"scribe", "kernel", "g1|scribe"}


async def test_a_numeric_reply_to_is_an_id_not_a_discard():
    """The grammar types `<reply_to>1001</reply_to>` as the NUMBER 1001, which
    must not read as "discard the reply". A number is an id: here it names an
    agent that exists, so the reply goes there."""
    k = _KnownKernel({"ai", "chat", "1001"})
    text = render_call(
        "send", {"target_id": "chat", "payload": {"type": "post"}, "reply_to": 1001}
    )
    await _route_reply("ai", k, text, "cli", None, None, 180.0, "")
    await _settle(k, 1)

    assert k.batches == [["chat"]]
    assert k.sent[0][0] == "1001"
