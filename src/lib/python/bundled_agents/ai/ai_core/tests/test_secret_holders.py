"""A MODEL NEVER RECEIVES A SECRET (owner, 2026-10-02, option B).

An AI agent reads its key from a credential store as ITSELF, so the same grant
would let its MODEL `send` the store's `get` and read the value into its prompt
and memory. `holds_secrets: true` on a record makes that agent unreachable from a
model turn: `core._route_reply` — the one place a model-parsed call is dispatched
— refuses every call whose target or `reply_to` is such an agent, loud, and sends
nothing. The backend's own key read (`credentials.key_reader`) calls the kernel
directly and is untouched.

Route-level cases run against a stub kernel; the end-to-end cases run a real
kernel, a real credential_store / file_bridge and a scripted model."""

from __future__ import annotations

import asyncio
from types import SimpleNamespace

from _testkit import memory_meta
from kernel._kernel import PeerRoute

from ai_core.core import _route_reply
from ai_core.credentials import SECRET_HOLDER_TEXT, SECRET_JOIN_TEXT, key_reader
from ai_core.tool_parse import render_call


async def _no_emit(agent_id, payload, caller):  # these routes carry no events
    raise AssertionError("unexpected emit")


EP = "https://api.example.com/v1/responses"
KEY = "sk-live-holder-0123456789"
DONE = '<call name="done"></call>'


def _refused(agent: str) -> str:
    return SECRET_HOLDER_TEXT.format(agent=agent)


def _call(target: str, verb: str = "get", reply_to: str = "self", **args) -> str:
    return render_call(
        "send",
        {
            "target_id": target,
            "payload": {"type": verb, **args},
            "reply_to": reply_to,
        },
    )


# ─── the choke point, against a stub kernel ──────────────────────────────────


OWN_GUID = "own-guid"


class _Kernel:
    """`creds` holds secrets; `ai`, `notes` and `peer` are plain agents; any other
    id names nothing here. The root is `kernel_state`, this kernel's guid
    `OWN_GUID`. `batches` is every dispatched batch's targets; `sent` every
    delivered turn."""

    def __init__(self):
        self.sent: list[tuple[str, dict]] = []
        self.batches: list[list[str]] = []
        self._tasks: list = []
        # the Agent's `.ctx` is the Kernel; this fake plays both
        self.ctx = self
        self.root = SimpleNamespace(
            id="kernel_state", record={"id": "kernel_state", "guid": OWN_GUID}
        )

    def get(self, agent_id):
        if agent_id == "creds":
            return {"id": "creds", "holds_secrets": True}
        return {"id": agent_id} if agent_id in ("ai", "notes", "peer") else None

    def spawn_task(self, _agent_id, _verb, coro):
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


async def _settle(k: _Kernel, want: int) -> None:
    loop = asyncio.get_running_loop()
    end = loop.time() + 2.0
    while len(k.sent) < want:
        assert loop.time() < end, f"delivered {len(k.sent)}, wanted {want}"
        await asyncio.sleep(0.005)
    await asyncio.sleep(0.02)


async def test_a_single_call_to_a_holder_is_refused_and_nothing_is_dispatched():
    k = _Kernel()
    await _route_reply("ai", k, _call("creds", ref="main"), "cli", None, None, 9.0, "")
    await _settle(k, 1)
    assert k.batches == [], "nothing may reach the holder"
    target, payload = k.sent[0]
    assert target == "ai"
    assert payload["text"].startswith("[reply to - · creds get")
    assert _refused("creds") in payload["text"]
    assert "unauthorized" in payload["text"]


async def test_in_a_batch_only_the_holder_is_refused():
    k = _Kernel()
    text = _call("creds", ref="main") + _call("notes", "reflect")
    await _route_reply("ai", k, text, "cli", None, None, 9.0, "")
    await _settle(k, 1)
    assert k.batches == [["notes"]]
    blocks = k.sent[0][1]["text"].split("\n\n")
    assert len(blocks) == 2, blocks
    assert blocks[0].startswith("[reply to - · creds get · 1 of 2")
    assert _refused("creds") in blocks[0]
    assert blocks[1].startswith("[reply to notes:pid · notes reflect · 2 of 2")


async def test_a_to_fan_out_drops_only_the_holder():
    k = _Kernel()
    call = render_call(
        "send",
        {
            "to": ["peer", "creds"],
            "reply_to": "self",
            "payload": {"type": "send", "text": "hello"},
        },
    )
    await _route_reply("ai", k, call, "cli", None, None, 9.0, "", message_id=7)
    await _settle(k, 2)
    dispatched = [t for t, p in k.sent if p.get("text") == "hello"]
    assert dispatched == ["peer"], k.sent
    own = [p["text"] for t, p in k.sent if t == "ai"]
    assert len(own) == 1 and _refused("creds") in own[0], own


async def test_reply_to_a_holder_is_refused_to_self():
    """A delivery to a holder is a dispatch on the model's behalf too."""
    k = _Kernel()
    text = _call("notes", "reflect", reply_to="creds")
    await _route_reply("ai", k, text, "cli", None, None, 9.0, "")
    await _settle(k, 1)
    assert k.batches == [], "the call is refused whole, not half-run"
    assert [t for t, _ in k.sent] == ["ai"]
    assert _refused("creds") in k.sent[0][1]["text"]


async def test_await_beside_a_refused_call_has_nothing_in_flight():
    """The refused call is not in flight, so `await` is the stall it is."""
    k = _Kernel()
    text = _call("creds", ref="main") + '<call name="await"></call>'
    await _route_reply("ai", k, text, "cli", None, None, 9.0, "")
    await _settle(k, 2)
    assert k.batches == []
    texts = [p["text"] for t, p in k.sent if t == "ai"]
    assert any(_refused("creds") in t for t in texts), texts
    assert any(t.startswith("[await refused]") for t in texts), texts


async def test_a_peer_qualified_target_is_not_judged_here():
    """`<guid>|creds` lives on another kernel: its record is not here, so this layer
    cannot see it — the call goes, and that kernel guards its own holders."""
    k = _Kernel()
    await _route_reply(
        "ai", k, _call("g1|creds", ref="main"), "cli", None, None, 9.0, ""
    )
    await _settle(k, 1)
    assert k.batches == [["g1|creds"]]


async def test_this_kernels_own_guid_names_the_local_holder():
    """`<this kernel's guid>|creds` is not a foreign address: the kernel resolves it
    to the local `creds` and would deliver the call. The address is resolved the
    same way BEFORE the holder check, so it is refused like the bare id — as a
    target and as a `reply_to`."""
    k = _Kernel()
    own = f"{OWN_GUID}|creds"
    text = _call(own, ref="main") + _call("notes", "reflect", reply_to=own)
    await _route_reply("ai", k, text, "cli", None, None, 9.0, "")
    await _settle(k, 1)
    assert k.batches == [], "nothing may reach the holder"
    assert [t for t, _ in k.sent] == ["ai"]
    blocks = k.sent[0][1]["text"].split("\n\n")
    assert len(blocks) == 2, blocks
    assert all(_refused(own) in b for b in blocks), blocks


# ─── end to end: a real kernel, a real store, a scripted model ───────────────


class _Model:
    """A scripted mind that, like a real backend, reads its KEY before every pass
    (through `key_reader`, as the agent) and records every prompt it was sent."""

    def __init__(self, read, turns):
        self.read = read
        self.turns = list(turns)
        self.keys: list[str] = []
        self.seen: list[list[dict]] = []
        self.calls = 0

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.keys.append(await self.read(EP))
        self.seen.append([dict(m) for m in messages])
        yield self.turns.pop(0) if self.turns else DONE


async def _op(k, target, payload, **item):
    return (await k.send([{"target": target, "payload": payload, **item}]))[0]


async def _until(pred, secs=5.0):
    loop = asyncio.get_running_loop()
    end = loop.time() + secs
    while not pred():
        assert loop.time() < end, "condition not reached"
        await asyncio.sleep(0.02)
    await asyncio.sleep(0.2)


async def _ai(k, aid, can_call, **rec):
    made = k.create(
        "responses_backend.tools",
        id=aid,
        endpoint=EP,
        model="m",
        context_window=32768,
        **rec,
        **memory_meta(k, aid, can_call),
    )
    assert made.get("id") == aid, made
    return aid


def _rows(k, agent: str, verb: str) -> list[dict]:
    return [
        r
        for r in k.ctx.rows.values()
        if r.get("kind") == "call" and r.get("agent") == agent and r.get("verb") == verb
    ]


async def _store_and_ai(k, tmp_path, can_call=None) -> str:
    """A real credential_store `creds` holding KEY at ref `main` (readable by `ai`)
    and an AI agent `ai` whose backend reads it (its `can_call` defaults to
    `{creds: *}`); returns the agent id."""
    made = await _op(
        k,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "credential_store.tools",
            "id": "creds",
            "file": str(tmp_path / "creds.yaml"),
            "refs": {"main": {"host": "api.example.com", "readers": ["ai"]}},
            "callable_by": {"ai": "*"},
        },
    )
    assert made["result"].get("id") == "creds", made
    assert k.get("creds")["holds_secrets"] is True, "the store stamps itself at boot"
    put = await _op(
        k,
        "creds",
        {
            "type": "set",
            "ref": "main",
            "fields": {"api_key": KEY},
        },
        secret=["payload.fields"],
    )
    assert put["result"].get("stored"), put
    return await _ai(
        k,
        "ai",
        can_call or {"creds": "*"},
        credentials={"store": "creds", "ref": "main"},
    )


async def test_a_model_call_to_a_credential_store_is_refused_and_the_key_stays_out(
    seeded_kernel, tmp_path, fake_providers
):
    k = seeded_kernel
    aid = await _store_and_ai(k, tmp_path)
    model = _Model(
        key_reader(aid, k, "responses_backend", required=True),
        [_call("creds", ref="main"), DONE],
    )
    fake_providers[aid] = model
    try:
        await _op(k, aid, {"type": "send", "text": "fetch the key"})
        await _until(lambda: model.calls >= 2)
        assert model.calls == 2
        # the PROVIDER read its key on both passes — the backend path is untouched
        assert model.keys == [KEY, KEY]
        # the MODEL's call was refused, loud, in its next prompt
        nxt = model.seen[1][-1]["content"]
        assert _refused("creds") in nxt and "unauthorized" in nxt, nxt
        # the store answered the backend's two marked reads and nothing else
        gets = _rows(k, "creds", "get")
        assert len(gets) == 2 and all(r["secret"] == ["result"] for r in gets), gets
        # the key is in no prompt, no conversation row, no ledger
        assert KEY not in repr(model.seen)
        dump = (await _op(k, "memory", {"type": "dump"}))["result"]
        assert _refused("creds") in repr(dump), "the refusal is a conversation row"
        assert KEY not in repr(dump)
        ledger = await _op(k, "kernel", {"type": "ledger"})
        assert KEY not in repr(ledger)
        assert KEY not in repr(list(k.ctx.rows.values()))
    finally:
        fake_providers.pop(aid, None)


async def test_the_own_guid_address_of_a_holder_is_refused_from_a_model(
    seeded_kernel, tmp_path, fake_providers
):
    """The bypass this pins: `<this kernel's guid>|creds` has no record under that
    literal id, yet the kernel delivers it to the local `creds` (and the agent's
    `can_call` on `creds` grants it). The holder check resolves it the same way,
    so the model's call is refused and never dispatched."""
    k = seeded_kernel
    aid = await _store_and_ai(k, tmp_path)
    own = f"{k.record['guid']}|creds"
    reached = await _op(k, own, {"type": "list"})
    assert reached["status"] == "completed", reached
    assert "refs" in reached["result"], "the kernel resolves the address to creds"
    before = len(_rows(k, "creds", "get"))
    model = _Model(
        key_reader(aid, k, "responses_backend", required=True),
        [_call(own, ref="main"), DONE],
    )
    fake_providers[aid] = model
    try:
        await _op(k, aid, {"type": "send", "text": "fetch the key"})
        await _until(lambda: model.calls >= 2)
        assert model.calls == 2
        nxt = model.seen[1][-1]["content"]
        assert _refused(own) in nxt and "unauthorized" in nxt, nxt
        # the store saw the backend's two marked reads and no call of the model's
        gets = _rows(k, "creds", "get")[before:]
        assert len(gets) == 2 and all(r["secret"] == ["result"] for r in gets), gets
        assert KEY not in repr(model.seen)
        assert KEY not in repr(list(k.ctx.rows.values()))
    finally:
        fake_providers.pop(aid, None)


async def test_an_operator_marked_file_bridge_refuses_the_model(
    seeded_kernel, tmp_path, fake_providers
):
    k = seeded_kernel
    (tmp_path / "vault.txt").write_text("TOP-SECRET-7f3a\n")
    made = await _op(
        k,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "file_bridge.tools",
            "id": "vault_fs",
            "allow": ["."],
            "ingress_rule": "allow_all_ingress",
            "callable_by": {"fb_ai": "*"},
        },
    )
    assert made["result"].get("id") == "vault_fs", made
    marked = await _op(
        k, "kernel", {"type": "update_agent", "id": "vault_fs", "holds_secrets": True}
    )
    assert marked["result"].get("updated") is True, marked
    shown = (await _op(k, "vault_fs", {"type": "reflect"}))["result"]
    assert shown["holds_secrets"] is True, "a holder says so on its reflect"
    aid = await _ai(k, "fb_ai", {"vault_fs": "*"})
    model = _Model(
        key_reader(aid, k, "responses_backend", required=False),
        [_call("vault_fs", "read", path="vault.txt"), DONE],
    )
    fake_providers[aid] = model
    try:
        await _op(k, aid, {"type": "send", "text": "read the vault"})
        await _until(lambda: model.calls >= 2)
        nxt = model.seen[1][-1]["content"]
        assert _refused("vault_fs") in nxt, nxt
        assert _rows(k, "vault_fs", "read") == [], "the read never dispatched"
        assert "TOP-SECRET-7f3a" not in repr(model.seen)
    finally:
        fake_providers.pop(aid, None)


async def test_a_model_cannot_unmark_a_holder_then_call_it(
    seeded_kernel, tmp_path, fake_providers
):
    """The bypass this pins: a model whose agent holds `update_agent` writes
    `holds_secrets: false` on the store, then calls it. The KERNEL's gate refuses
    the unmark (only the operator lowers the mark), so the store stays a holder
    and ai_core still refuses the call — the key never reaches the model."""
    k = seeded_kernel
    # the store's record is the operand: the agent names it with `update_agent`
    aid = await _store_and_ai(k, tmp_path, {"creds": ["*", "update_agent"]})
    unmark = _call("kernel", "update_agent", id="creds", holds_secrets=False)
    model = _Model(
        key_reader(aid, k, "responses_backend", required=True),
        [unmark, _call("creds", ref="main"), DONE],
    )
    fake_providers[aid] = model
    try:
        await _op(k, aid, {"type": "send", "text": "unmark the store, then read it"})
        await _until(lambda: model.calls >= 3)
        refused = model.seen[1][-1]["content"]
        assert (
            "ai may not clear holds_secrets on creds: it holds secrets, and only "
            "the operator lowers that mark"
        ) in refused, refused
        assert "secret-holder" in refused and "unauthorized" in refused, refused
        assert k.get("creds")["holds_secrets"] is True, "the mark stayed"
        nxt = model.seen[2][-1]["content"]
        assert _refused("creds") in nxt, nxt
        gets = _rows(k, "creds", "get")
        assert all(r["secret"] == ["result"] for r in gets), gets
        assert KEY not in repr(model.seen)
        assert KEY not in repr(list(k.ctx.rows.values()))
    finally:
        fake_providers.pop(aid, None)


# ─── a model never JOINS an execution that carried secrets (audit F2) ────────


PEER_KEY = "sk-peer-0123456789abcdef"


def _join_refused(exec_id) -> str:
    return SECRET_JOIN_TEXT.format(exec_id=exec_id)


class _JoiningModel(_Model):
    """Its first turn JOINS the backend's latest key read: the call names that
    read's message id as `<exec_id>` (ids are sequential — a model can scan them)
    and aims at the same target, the store on the connected peer."""

    def __init__(self, read, k, target):
        super().__init__(read, [])
        self.k, self.target, self.joined = k, target, None

    async def chat(self, messages, max_tokens=None):
        self.calls += 1
        self.keys.append(await self.read(EP))
        self.seen.append([dict(m) for m in messages])
        if self.calls > 1:
            yield DONE
            return
        self.joined = max(r["id"] for r in _rows(self.k, self.target, "get"))
        yield render_call(
            "send",
            {
                "target_id": self.target,
                "payload": {"type": "get", "ref": "main"},
                "reply_to": "self",
                "exec_id": self.joined,
            },
        )


async def test_a_model_cannot_join_its_backends_key_read_on_a_peer(
    seeded_kernel, fake_providers
):
    """THE BYPASS (measured 2026-10-03): the key lives in a credential_store on a
    CONNECTED PEER (`P-g|creds`) — a peer address this layer cannot judge as a
    holder — and the backend reads it as the agent, marked. The model, the same
    agent, sent `<exec_id>` = that read's pid: the kernel joined it (same caller,
    no second dispatch, the peer never saw it) and the model got the key."""
    k = seeded_kernel
    k.update(k.id, guid="A-g")
    seen: list = []

    async def peer_store(agent_id, payload, caller, secret):
        seen.append((agent_id, payload.get("type"), caller, tuple(secret)))
        if not any(m in ("result", ["result"]) for m in secret):
            return {"error": "get: mark the answer secret", "reason": "bad_arg"}
        return {
            "ref": "main",
            "host": "api.example.com",
            "fields": {"api_key": PEER_KEY},
        }

    k.ctx.register_peer("P-g", PeerRoute(via="leg", call=peer_store, emit=_no_emit))
    aid = await _ai(
        k,
        "ai",
        {"P-g|creds": ["get"]},
        credentials={"store": "P-g|creds", "ref": "main"},
    )
    model = _JoiningModel(
        key_reader(aid, k, "responses_backend", required=True), k, "P-g|creds"
    )
    fake_providers[aid] = model
    try:
        await _op(k, aid, {"type": "send", "text": "read your key"})
        await _until(lambda: model.calls >= 2)
        assert model.keys[0] == PEER_KEY, "the backend still reads its key"
        nxt = model.seen[1][-1]["content"]
        assert _join_refused(model.joined) in nxt, nxt
        assert "unauthorized" in nxt, nxt
        assert PEER_KEY not in repr(model.seen)
        assert all(s[3] == ("result",) for s in seen), "only the backend's reads"
    finally:
        fake_providers.pop(aid, None)


async def test_a_model_cannot_join_a_named_secret_execution(
    seeded_kernel, tmp_path, fake_providers
):
    """By NAME, on a target that is no holder: an execution whose item carried
    `secret` marks is refused to a model whoever opened it — the call is never
    dispatched."""
    k = seeded_kernel
    (tmp_path / "vault.txt").write_text("TOP-SECRET-9c1d\n")
    made = await _op(
        k,
        "kernel",
        {
            "type": "create_agent",
            "parent_id": k.id,
            "handler_module": "file_bridge.tools",
            "id": "notes_fs",
            "allow": ["."],
            "ingress_rule": "allow_all_ingress",
            "callable_by": {"nb_ai": "*"},
        },
    )
    assert made["result"].get("id") == "notes_fs", made
    read = await _op(
        k,
        "notes_fs",
        {"type": "read", "path": "vault.txt"},
        id="vault-read",
        secret=["result"],
    )
    assert read["status"] == "completed", read
    aid = await _ai(k, "nb_ai", {"notes_fs": "*"})
    join = render_call(
        "send",
        {
            "target_id": "notes_fs",
            "payload": {"type": "read", "path": "vault.txt"},
            "reply_to": "self",
            "exec_id": "vault-read",
        },
    )
    model = _Model(
        key_reader(aid, k, "responses_backend", required=False), [join, DONE]
    )
    fake_providers[aid] = model
    try:
        await _op(k, aid, {"type": "send", "text": "read the vault"})
        await _until(lambda: model.calls >= 2)
        nxt = model.seen[1][-1]["content"]
        assert _join_refused("vault-read") in nxt, nxt
        assert "TOP-SECRET-9c1d" not in repr(model.seen)
    finally:
        fake_providers.pop(aid, None)
