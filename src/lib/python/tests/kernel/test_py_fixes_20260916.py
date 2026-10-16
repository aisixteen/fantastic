"""Each test pins one behaviour in the smallest reachable form."""

from __future__ import annotations

import yaml
import asyncio
import builtins
import json
import threading
from pathlib import Path

from _testkit import command_rx
from kernel._agent import _as_id, _json_safe


def test_as_id_accepts_decimal_digits_only():
    assert _as_id("12") == 12
    assert _as_id("²") is None  # str.isdigit() is True, int() raised
    assert _as_id("١٢") == 12  # Unicode decimals are decimals
    assert _as_id(True) is None and _as_id(0) is None


def test_json_safe_shrinks_bytes_like_telemetry():
    assert _json_safe({"a": [b"xy", {"b": bytearray(b"z")}], "c": 1}) == {
        "a": ["<bytes:2>", {"b": "<bytes:1>"}],
        "c": 1,
    }


async def test_kernel_ledger_is_json_after_a_binary_outcome(seeded_kernel, file_bridge):
    k = seeded_kernel
    fb = file_bridge if isinstance(file_bridge, str) else file_bridge.id
    await k.send(
        [
            {
                "target": fb,
                "payload": {
                    "type": "write_stream",
                    "path": "a.txt",
                    "bytes": b"hi",
                    "truncate": True,
                },
            }
        ]
    )
    r = (
        await k.send(
            # NAMED — only a named call's result is kept in its settle row
            [
                {
                    "target": fb,
                    "payload": {"type": "read_stream", "path": "a.txt"},
                    "id": "read-a",
                }
            ]
        )
    )[0]["result"]
    assert isinstance(r.get("bytes"), (bytes, bytearray))
    ledger = (await k.send([{"target": "kernel", "payload": {"type": "ledger"}}]))[0][
        "result"
    ]
    json.dumps(ledger)  # must not raise: bytes are not JSON
    assert "<bytes:2>" in json.dumps(ledger)


async def test_readme_key_is_lexical_and_exact(seeded_kernel):
    k = seeded_kernel
    bad = (
        await k.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": "a/../b"}}]
        )
    )[0]["result"]
    assert str(bad.get("readme", bad).get("error", "")).startswith("bad key")
    idx = (
        await k.send(
            [{"target": "kernel", "payload": {"type": "reflect", "readme": "index"}}]
        )
    )[0]["result"]
    keys = [e["key"] for e in (idx.get("readme", idx).get("index") or [])]
    if keys and keys[0].upper() != keys[0]:
        r = (
            await k.send(
                [
                    {
                        "target": "kernel",
                        "payload": {"type": "reflect", "readme": keys[0].upper()},
                    }
                ]
            )
        )[0]["result"]
        assert str(r.get("readme", r).get("error", "")).startswith("no doc")


def test_corrupt_root_record_keeps_the_children(tmp_path, monkeypatch):
    from file_bridge import fs
    from kernel_state.tools import read_tree

    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    d = tmp_path / ".fantastic"
    (d / "agents" / "memo").mkdir(parents=True)
    (d / "agent.yaml").write_text("{ not json")
    (d / "agents" / "memo" / "agent.yaml").write_text(
        json.dumps({"id": "memo", "handler_module": "file_bridge.tools"})
    )
    recs = read_tree(fs.Root(tmp_path, d))
    assert [r["id"] for r in recs] == ["memo"] and recs[0][
        "parent_id"
    ] == "kernel_state"
    root = boot_root()
    assert "memo" in root.ctx.agents and root.id == "kernel_state"


def test_describe_scopes_like_the_gate():
    from io_bridge._base import describe

    assert describe({}) == {
        "ingress_rule": "deny_ingress",
        "egress_rule": "silent_egress",
        "sealed": True,
    }
    assert describe({"auth": "allow_all"})["ingress_rule"] == "allow_all_ingress"
    assert describe({"auth": "allow_all"})["sealed"] is False
    # an already-suffixed `auth` stem resolves to an UNKNOWN rule at the gate → sealed
    d = describe({"auth": "allow_all_ingress"})
    assert d["ingress_rule"] == "allow_all_ingress_ingress" and d["sealed"] is True
    assert describe({"ingress_rule": "allow_all_ingress"})["sealed"] is False


def test_small_coercions():
    from ai_core.core import _as_list, _safe_client

    assert (
        _safe_client(5) == "5"
        and _safe_client(None) == "cli"
        and _safe_client(" a b ") == "a_b"
    )
    assert _as_list(None) == [] and _as_list(5) == [5] and _as_list([1, 2]) == [1, 2]


async def test_repl_prints_a_reflect_as_json_not_created(capsys):
    from kernel.modes.print import _print_result

    await _print_result({"id": "kernel_state", "handler_module": "kernel_state.tools"})
    out = capsys.readouterr().out
    assert "created" not in out and '"handler_module"' in out


async def test_stdin_reads_on_a_daemon_thread(monkeypatch):
    from kernel.modes.print import _read_line

    seen: dict = {}

    def fake_input(prompt):
        seen["daemon"] = threading.current_thread().daemon
        return "hello"

    monkeypatch.setattr(builtins, "input", fake_input)
    assert await _read_line("> ") == "hello"
    assert seen["daemon"] is True


async def test_scheduler_forgets_its_cache_on_delete(seeded_kernel, store_agent):
    import scheduler.tools as sched

    k = seeded_kernel
    for _ in range(2):
        rec = (
            await k.send(
                [
                    {
                        "target": "kernel",
                        "payload": {
                            "type": "create_agent",
                            "parent_id": k.id,
                            "handler_module": "scheduler.tools",
                            "id": "sch",
                        },
                    }
                ]
            )
        )[0]["result"]
        assert rec.get("id") == "sch", rec
        await k.send([{"target": "sch", "payload": {"type": "boot"}}])
        r = (
            await k.send(
                [
                    {
                        "target": "sch",
                        "payload": {
                            "type": "schedule",
                            "target": k.id,
                            "payload": {"type": "reflect"},
                            "interval_seconds": 3600,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert "error" not in r, r
        assert sched._cache.get("sch")
        await k.send(
            [{"target": "kernel", "payload": {"type": "delete_agent", "id": "sch"}}]
        )
        assert sched._cache.get("sch") is None
        assert "sch" not in sched._tick_tasks or sched._tick_tasks["sch"].done()


def test_exec_drain_rows_are_filed_under_the_owner():
    import exec_bridge.tools as x

    src = Path(x.__file__).read_text()
    assert "spawn_task(\n                            id," not in src
    assert (
        'spawn_task(\n                            owner_id,\n                            "watch_drain"'
        in src
    )


def test_web_serve_task_detaches_from_the_boot_dispatch():
    import web.tools as w

    # an EMPTY context: no task parent (and, a transport, no identity)
    assert "context=contextvars.Context()" in Path(w.__file__).read_text()


async def test_a_long_line_does_not_end_the_pipe(seeded_kernel):
    import sys

    k = seeded_kernel
    py = sys.executable
    rec = (
        await k.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": k.id,
                        "handler_module": "exec_bridge.tools",
                        "id": "runner",
                        "ingress_rule": "allow_all_ingress",
                        "allowed": [f"{command_rx(py)} .*"],
                        "cwd": ".",
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == "runner", rec
    s = (
        await k.send(
            [
                {
                    "target": "runner",
                    "payload": {
                        "type": "start",
                        "command": py,
                        "args": ["-c", "print('x' * 70000)"],
                    },
                }
            ]
        )
    )[0]["result"]
    jid = s.get("job_id") or (s.get("job") or {}).get("job_id")
    assert jid, s
    for _ in range(100):
        await asyncio.sleep(0.1)
        st = (
            await k.send(
                [{"target": "runner", "payload": {"type": "status", "job_id": jid}}]
            )
        )[0]["result"]
        text = json.dumps(st, default=str)
        if '"exit_code": 0' in text or '"exit_code":0' in text:
            break
    assert "[pump error]" not in text and "x" * 1000 in text


async def test_records_over_a_chunk_keep_every_key(tmp_path, monkeypatch):
    """A merge read of ONE 64 KiB chunk would make a larger agent.yaml fail to
    parse and its unknown keys vanish on the next write."""
    from _testkit import boot_root

    monkeypatch.chdir(tmp_path)
    root = boot_root()
    root.create(
        "file_bridge.tools",
        allow=["."],
        id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    await root.send([{"target": root.id, "payload": {"type": "boot"}}])
    big = "y" * (70 * 1024)
    rec = (
        await root.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "handler_module": "file_bridge.tools",
                        "id": "fat",
                        "note": big,
                    },
                }
            ]
        )
    )[0]["result"]
    assert rec.get("id") == "fat", rec
    await asyncio.sleep(0.6)
    await root.send(
        [
            {
                "target": "kernel",
                "payload": {"type": "update_agent", "id": "fat", "mark": 1},
            }
        ]
    )
    await asyncio.sleep(0.6)
    f = tmp_path / ".fantastic" / "agents" / "fat" / "agent.yaml"
    assert f.exists(), "record not persisted"
    doc = yaml.safe_load(f.read_text())
    assert doc.get("mark") == 1 and doc.get("note") == big
