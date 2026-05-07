"""ssh_runner — verb shapes + SSH command construction (mocked).

Tests don't actually SSH anywhere — they monkey-patch `_ssh_exec` +
`_open_tunnel` to assert the bundle invokes them with correct
arguments and threads the results through correctly. Real SSH is
exercised only in selftest.md against a live host.
"""

from __future__ import annotations

import shlex

import pytest

from ssh_runner import tools as sr


# ─── fixtures ────────────────────────────────────────────────────


@pytest.fixture
def runner_record():
    return {
        "host": "test-box",
        "remote_path": "/home/me/proj",
        "remote_cmd": "/home/me/.venv/bin/fantastic",
        "remote_port": 8888,
        "local_port": 49001,
        "entry_path": "canvas_webapp_abc/",
    }


async def _make(kernel, **fields):
    rec = await kernel.send(
        "core",
        {"type": "create_agent", "handler_module": "ssh_runner.tools", **fields},
    )
    return rec["id"]


@pytest.fixture(autouse=True)
def _wipe_state():
    """Process-memory state must not leak across tests."""
    yield
    sr._runners.clear()


# ─── tests ───────────────────────────────────────────────────────


async def test_reflect_lists_verbs(seeded_kernel):
    rid = await _make(seeded_kernel)
    r = await seeded_kernel.send(rid, {"type": "reflect"})
    for v in (
        "reflect",
        "boot",
        "shutdown",
        "start",
        "stop",
        "restart",
        "status",
        "get_webapp",
    ):
        assert v in r["verbs"], f"missing verb {v}"


async def test_reflect_surfaces_record_fields(seeded_kernel, runner_record):
    rid = await _make(seeded_kernel, **runner_record)
    r = await seeded_kernel.send(rid, {"type": "reflect"})
    assert r["host"] == "test-box"
    assert r["remote_path"] == "/home/me/proj"
    assert r["remote_cmd"] == "/home/me/.venv/bin/fantastic"
    assert r["remote_port"] == 8888
    assert r["local_port"] == 49001
    assert r["entry_path"] == "canvas_webapp_abc/"
    assert r["tunnel_alive"] is False  # nothing booted


async def test_get_webapp_url_composes_local_tunnel_plus_entry(
    seeded_kernel, runner_record
):
    rid = await _make(seeded_kernel, **runner_record)
    r = await seeded_kernel.send(rid, {"type": "get_webapp"})
    assert r["url"] == "http://localhost:49001/canvas_webapp_abc/"
    assert r["title"] == "test-box"
    assert r["default_width"] == 800


async def test_get_webapp_requires_local_port(seeded_kernel):
    rid = await _make(seeded_kernel, host="x", remote_path="/p", remote_cmd="f")
    r = await seeded_kernel.send(rid, {"type": "get_webapp"})
    assert "error" in r and "local_port" in r["error"]


async def test_start_requires_all_four_fields(seeded_kernel):
    rid = await _make(seeded_kernel, host="x")
    r = await seeded_kernel.send(rid, {"type": "start"})
    assert "error" in r
    assert "remote_path" in r["error"] or "required" in r["error"]


async def test_start_command_shape(seeded_kernel, runner_record, monkeypatch):
    """Verifies the SSH command we build:
      ssh test-box 'cd /home/me/proj && mkdir -p .fantastic &&
                    nohup /home/me/.venv/bin/fantastic serve --port 8888
                    > .fantastic/serve.log 2>&1 &'
    plus the lock-poll command + tunnel construction."""
    calls = []

    async def fake_ssh_exec(host, cmd, timeout=30.0):
        calls.append({"host": host, "cmd": cmd, "timeout": timeout})
        if "nohup" in cmd:
            return 0, "", ""
        if "lock.json" in cmd:
            return 0, '{"pid": 4242, "port": 8888}', ""
        return 0, "", ""

    async def fake_open_tunnel(host, local_port, remote_port):
        # Return a sentinel object that survives the .poll() call.
        class _Stub:
            pid = 5151

            def poll(self):
                return None

            def terminate(self):
                pass

            def wait(self, timeout=None):
                return 0

        return _Stub()

    monkeypatch.setattr(sr, "_ssh_exec", fake_ssh_exec)
    monkeypatch.setattr(sr, "_open_tunnel", fake_open_tunnel)

    rid = await _make(seeded_kernel, **runner_record)
    r = await seeded_kernel.send(rid, {"type": "start"})
    assert r.get("started") is True, r
    assert r["remote_pid"] == 4242
    assert r["tunnel_pid"] == 5151

    # First SSH call: the boot command. Verify shape.
    boot_cmd = calls[0]["cmd"]
    assert calls[0]["host"] == "test-box"
    assert "cd /home/me/proj" in boot_cmd
    assert "mkdir -p .fantastic" in boot_cmd
    assert "/home/me/.venv/bin/fantastic" in boot_cmd
    assert "serve --port 8888" in boot_cmd
    assert "> .fantastic/serve.log 2>&1 &" in boot_cmd
    assert "nohup" in boot_cmd

    # Subsequent calls: lock.json polls.
    assert any("lock.json" in c["cmd"] for c in calls[1:])


async def test_start_handles_paths_with_spaces(seeded_kernel, monkeypatch):
    """remote_path with a space must be shell-quoted."""
    calls = []

    async def fake_ssh_exec(host, cmd, timeout=30.0):
        calls.append(cmd)
        if "lock.json" in cmd:
            return 0, '{"pid": 7}', ""
        return 0, "", ""

    async def fake_tunnel(host, local_port, remote_port):
        class _Stub:
            pid = 1

            def poll(self):
                return None

            def terminate(self):
                pass

            def wait(self, timeout=None):
                return 0

        return _Stub()

    monkeypatch.setattr(sr, "_ssh_exec", fake_ssh_exec)
    monkeypatch.setattr(sr, "_open_tunnel", fake_tunnel)

    rid = await _make(
        seeded_kernel,
        host="h",
        remote_path="/home/me/my project",
        remote_cmd="/usr/bin/fantastic",
        remote_port=8888,
        local_port=49099,
    )
    r = await seeded_kernel.send(rid, {"type": "start"})
    assert r.get("started") is True
    boot_cmd = calls[0]
    assert shlex.quote("/home/me/my project") in boot_cmd


async def test_stop_kills_remote_pid(seeded_kernel, runner_record, monkeypatch):
    calls = []

    async def fake_ssh_exec(host, cmd, timeout=30.0):
        calls.append(cmd)
        if "cat" in cmd and "lock.json" in cmd:
            return 0, '{"pid": 9999, "port": 8888}', ""
        return 0, "", ""

    monkeypatch.setattr(sr, "_ssh_exec", fake_ssh_exec)

    rid = await _make(seeded_kernel, **runner_record)
    r = await seeded_kernel.send(rid, {"type": "stop"})
    assert r["stopped"] is True
    assert r["remote_pid"] == 9999
    # The kill command was sent
    assert any("kill 9999" in c for c in calls)


async def test_stop_idempotent_when_no_lock(seeded_kernel, runner_record, monkeypatch):
    """No remote lock.json (stale agent) → stop returns cleanly with
    remote_pid:None."""

    async def fake_ssh_exec(host, cmd, timeout=30.0):
        if "lock.json" in cmd:
            return 0, "", ""  # empty file
        return 0, "", ""

    monkeypatch.setattr(sr, "_ssh_exec", fake_ssh_exec)

    rid = await _make(seeded_kernel, **runner_record)
    r = await seeded_kernel.send(rid, {"type": "stop"})
    assert r["stopped"] is True
    assert r["remote_pid"] is None


async def test_status_when_nothing_running(seeded_kernel, runner_record, monkeypatch):
    async def fake_ssh_exec(host, cmd, timeout=30.0):
        return 0, "", ""  # no lock.json

    monkeypatch.setattr(sr, "_ssh_exec", fake_ssh_exec)
    monkeypatch.setattr(sr, "_http_health", lambda port: False)

    rid = await _make(seeded_kernel, **runner_record)
    r = await seeded_kernel.send(rid, {"type": "status"})
    assert r["tunnel_alive"] is False
    assert r["remote_alive"] is False
    assert r["http_ok"] is False
    assert r["remote_pid"] is None


async def test_shutdown_via_delete_agent_lifecycle(
    seeded_kernel, runner_record, monkeypatch
):
    """core.delete_agent's universal shutdown hook invokes
    ssh_runner.shutdown (== stop). The runner should clean up
    silently — no exception even if nothing is running."""
    calls = []

    async def fake_ssh_exec(host, cmd, timeout=30.0):
        calls.append(cmd)
        return 0, "", ""

    monkeypatch.setattr(sr, "_ssh_exec", fake_ssh_exec)

    rid = await _make(seeded_kernel, **runner_record)
    r = await seeded_kernel.send("core", {"type": "delete_agent", "id": rid})
    assert r.get("deleted") is True
    # The shutdown hook fired: at least one ssh_exec call to read lock.
    assert any("lock.json" in c for c in calls)


async def test_unknown_verb_errors(seeded_kernel):
    rid = await _make(seeded_kernel)
    r = await seeded_kernel.send(rid, {"type": "garbage"})
    assert "error" in r and "unknown type" in r["error"]
