"""How the substrate reaches a bundle's MODULE: the handler, the duck-typed hooks
(on_delete / on_cancel / on_shutdown / reflect_root_extra / flush_pending), the
VERBS table — and what each does when the module is missing, half-built or raises.

Each of these reaches the module through `import_handler(handler_module)` — the
bundle catalog plus `register_unlisted`, never all of `sys.path`.
Routing them through one `module_hook` (reduction candidate E) must keep every
failure mode below: a raising hook never aborts the walk it is part of, each logs
its own line (stdout for cascade/cancel, stderr for shutdown), an unimportable
bundle answers a coded refusal carrying the import error, and the full-detail
tree keeps its node shape.
"""

from __future__ import annotations

import asyncio
import types

from kernel import register_unlisted

from _testkit import as_agent

_started: dict = {}


def _bundle(name: str, *, handler=True, **hooks) -> types.ModuleType:
    """Register a fake bundle module under `name` (`register_unlisted`)."""
    mod = types.ModuleType(name)
    mod.VERBS = {}
    if handler:

        async def _handler(id, payload, kernel):
            verb = payload.get("type")
            if verb in ("boot",):
                return None
            if verb == "reflect":
                return {"id": id, "sentence": "hook probe", "verbs": {}}
            if verb == "slow":
                _started.setdefault(id, asyncio.Event()).set()
                await asyncio.sleep(30)
            return {"ok": id}

        mod.handler = _handler
    for k, v in hooks.items():
        setattr(mod, k, v)
    register_unlisted(name, mod)
    return mod


async def _boom(*a, **kw):
    raise RuntimeError("boom")


async def _call(k, target, payload) -> dict:
    return (await k.send([{"target": target, "payload": payload}]))[0]


# ─── a module that cannot answer ─────────────────────────────────────────────


async def test_handler_module_not_installed_is_the_python_unhandled_shape(seeded_kernel):
    k = seeded_kernel
    rec = k.create("no_such_bundle_pin.tools", id="ghost")
    assert rec.get("id") == "ghost", rec
    out = await _call(k, "ghost", {"type": "ping"})
    # a reply carrying `error` settles as `errored` (dispatched: it has a pid)
    assert out["status"] == "errored" and isinstance(out["pid"], int), out
    assert out["attempts"] == 1
    assert out["result"] == {
        "error": (
            "agent 'ghost': handler_module 'no_such_bundle_pin.tools' cannot be "
            "imported in this runtime: ModuleNotFoundError: No module named "
            "'no_such_bundle_pin'"
        ),
        "reason": "unhandled",
        "verb": "ping",
    }
    # reflect is answered the same way — and NOT decorated into a healthy reflect
    r = (await _call(k, "ghost", {"type": "reflect"}))["result"]
    assert r["reason"] == "unhandled" and "kernel_verbs" not in r, r


async def test_a_module_without_handler_is_a_coded_refusal(seeded_kernel):
    _bundle("hook_pin_nohandler.tools", handler=False)
    k = seeded_kernel
    k.create("hook_pin_nohandler.tools", id="nh")
    r = (await _call(k, "nh", {"type": "ping"}))["result"]
    assert r == {
        "error": "hook_pin_nohandler.tools has no handler()",
        "reason": "unhandled",
        "verb": "ping",
    }


async def test_a_bare_agent_answers_only_substrate_verbs(seeded_kernel):
    k = seeded_kernel
    k.create(None, id="bare")
    assert (await _call(k, "bare", {"type": "boot"}))["result"] is None
    assert (await _call(k, "bare", {"type": "shutdown"}))["result"] is None
    r = (await _call(k, "bare", {"type": "reflect"}))["result"]
    assert r["id"] == "bare" and r["sentence"] == (
        "Bare agent (no handler_module) — answers substrate verbs only."
    )
    assert (await _call(k, "bare", {"type": "ping"}))["result"] == {
        "error": "agent 'bare' has no handler_module; cannot answer verb 'ping'",
        "reason": "unhandled",
        "verb": "ping",
    }
# twin: a_kernel_verb_at_a_child_says_pythons_words_and_names_the_verb
# twin: not_here_points_at_the_root
# twin: a_kernel_verb_at_a_non_root_agent_says_where_to_send_it


async def test_a_kernel_verb_sent_to_a_non_root_names_where_to_go(seeded_kernel):
    r = (await _call(seeded_kernel, "cli", {"type": "list_agents"}))["result"]
    assert r == {
        "error": "list_agents is a kernel verb — cli does not answer it",
        "reason": "not_here",
        "hint": (
            "address the kernel root ('kernel'); name the agent you mean in "
            "`parent_id` / `id`"
        ),
        "verb": "list_agents",
    }


# ─── raising hooks never abort the walk ──────────────────────────────────────


async def test_a_raising_on_delete_does_not_abort_the_cascade(seeded_kernel, capsys):
    _bundle("hook_pin_del.tools", on_delete=_boom)
    k = seeded_kernel
    k.create("hook_pin_del.tools", id="dp")
    k.ctx.agents["dp"].create("hook_pin_del.tools", id="dk")
    out = await _call(k, "kernel", {"type": "delete_agent", "id": "dp"})
    assert out["status"] == "completed" and "error" not in out["result"], out
    assert "dp" not in k.ctx.agents and "dk" not in k.ctx.agents
    printed = capsys.readouterr().out
    # children first, each on its own line, on STDOUT
    assert printed.index(
        "  [cascade] dk bundle on_delete raised: boom"
    ) < printed.index("  [cascade] dp bundle on_delete raised: boom")


async def test_an_unimportable_bundle_is_still_deleted(seeded_kernel, capsys):
    k = seeded_kernel
    k.create("no_such_bundle_pin2.tools", id="gone")
    out = await _call(k, "kernel", {"type": "delete_agent", "id": "gone"})
    assert out["status"] == "completed" and "gone" not in k.ctx.agents
    assert (
        "  [cascade] gone bundle on_delete raised: No module named 'no_such_bundle_pin2'"
        in capsys.readouterr().out
    )


async def test_on_cancel_failure_prints_python_line(seeded_kernel, capsys):
    _bundle("hook_pin_cancel.tools", on_cancel=_boom)
    k = seeded_kernel
    k.create("hook_pin_cancel.tools", id="cp")
    _started.pop("cp", None)
    pending = asyncio.create_task(
        k.send([{"target": "cp", "payload": {"type": "slow"}}])
    )
    await asyncio.wait_for(_started.setdefault("cp", asyncio.Event()).wait(), 2)
    tasks = (await _call(k, "kernel", {"type": "ps"}))["result"]["tasks"]
    pid = next(t["id"] for t in tasks if t["agent"] == "cp")
    out = (await _call(k, "kernel", {"type": "cancel", "task_id": pid}))["result"]
    assert out == {"cancelled": True, "killed": [pid]}
    settled = (await asyncio.wait_for(pending, 2))[0]
    assert settled["status"] == "cancelled" and settled["result"] == {"cancelled": True}
    assert "  [cancel] cp bundle on_cancel raised: boom" in capsys.readouterr().out


async def test_shutdown_hook_failures_log_to_stderr_and_the_walk_goes_on(
    seeded_kernel, capsys
):
    fired: list = []

    async def ok(agent):
        fired.append(agent.id)

    _bundle("hook_pin_shut_bad.tools", on_shutdown=_boom)
    _bundle("hook_pin_shut_ok.tools", on_shutdown=ok)
    k = seeded_kernel
    k.create("hook_pin_shut_bad.tools", id="sb")
    k.create("no_such_bundle_pin3.tools", id="sx")
    k.create("hook_pin_shut_ok.tools", id="so")
    await k.ctx.shutdown()
    err = capsys.readouterr().err
    assert "  [shutdown] sb hook raised: boom" in err
    assert (
        "  [shutdown] sx import 'no_such_bundle_pin3.tools' raised: No module named "
        "'no_such_bundle_pin3'"
    ) in err
    assert fired == ["so"]


async def test_a_raising_flush_pending_does_not_stop_the_shutdown(
    seeded_kernel, monkeypatch, capsys
):
    import kernel_state.tools as ks

    monkeypatch.setattr(ks, "flush_pending", _boom, raising=False)
    await seeded_kernel.ctx.shutdown()
    assert "  [shutdown] flush_pending raised: boom" in capsys.readouterr().err
    assert seeded_kernel.ctx._shutdown_complete is True


async def test_an_unimportable_bundle_is_not_told_to_shut_down(seeded_kernel):
    async def told(id, payload, kernel):
        return {"ok": id}

    mod = _bundle("hook_pin_adv.tools")
    mod.VERBS = {"shutdown": told}
    k = seeded_kernel
    k.create("hook_pin_adv.tools", id="adv")
    k.create("no_such_bundle_pin4.tools", id="mute")
    k.ctx.QUIET_TIMEOUT = 0.2
    try:
        out = (await _call(k, "kernel", {"type": "shutdown_agents"}))["result"]
    finally:
        k.ctx.draining = False
    assert out["shut"] == ["adv"], out


# ─── reflect's module reads ──────────────────────────────────────────────────


async def test_a_raising_reflect_root_extra_fails_the_reflect(
    seeded_kernel, monkeypatch
):
    """The loader's `reflect_root_extra` is the ONLY source of `persistence` and
    `guid` on the root reflect. A raise inside it must not be swallowed with the
    reflect answering without them — an operator would read "no persistence" and
    nothing would say why. The raise fails the reflect."""
    import kernel_state.tools as ks

    def _raise(agent, full=False):
        raise RuntimeError("extra broke")

    base = (await _call(seeded_kernel, "kernel", {"type": "reflect"}))["result"]
    assert "persistence" in base and "guid" in base
    monkeypatch.setattr(ks, "reflect_root_extra", _raise)
    for target in ("kernel", seeded_kernel.id):
        out = await _call(seeded_kernel, target, {"type": "reflect"})
        assert out["status"] == "failed", out
        assert out["result"] == {"error": "RuntimeError: extra broke", "raised": True}


async def test_tree_node_includes_id_and_empty_children(seeded_kernel):
    k = seeded_kernel
    k.create(
        "file_bridge.tools",
        id="fbn",
        description="docs on disk",
        allow=["src"],
        ingress_rule="allow_all_ingress",
        display_name="Docs",
        mode="ignored-in-full",
    )
    k.ctx.agents["fbn"].create(None, id="fbn_kid")
    r = (await _call(k, "kernel", {"type": "reflect", "detail": "full"}))["result"]
    root = r["tree"]
    assert list(root)[:4] == ["id", "parent_id", "handler_module", "display_name"]
    node = next(c for c in root["children"] if c["id"] == "fbn")
    base = {"$self": "*"}
    assert node == {
        "id": "fbn",
        "parent_id": "kernel_state",
        "handler_module": "file_bridge.tools",
        "display_name": "Docs",
        "description": "docs on disk",
        "ingress_rule": "allow_all_ingress",
        "allow": ["src"],
        "can_call": base,
        "callable_by": base,
        "children": [
            {
                "id": "fbn_kid",
                "parent_id": "fbn",
                "handler_module": None,
                "display_name": "fbn_kid",
                "can_call": base,
                "callable_by": base,
                "children": [],
            }
        ],
    }


async def test_an_agent_reflect_through_the_gate_still_reads_the_module(seeded_kernel):
    """`reflect` of a stub bundle by a granted agent: the module is reached once
    through the gate and answers; the substrate adds its own fields."""
    _bundle("hook_pin_refl.tools")
    k = seeded_kernel
    k.create("hook_pin_refl.tools", id="hr", callable_by={"asker": ["reflect"]})
    k.create(None, id="asker", can_call={"hr": ["reflect"]})
    with as_agent(seeded_kernel, "asker"):
        r = (await _call(k, "hr", {"type": "reflect"}))["result"]
    assert r["sentence"] == "hook probe" and "tree" in r
