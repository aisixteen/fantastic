"""py_agent + folder bundles: a `<root>/bundles/<name>/` folder
extends py_agent; each instance's program is run by the kernel and answers the
bundle's verbs over the connector channel. Real programs, real processes."""

from __future__ import annotations

import asyncio
import os
import shutil
import sys
from importlib import resources
from pathlib import Path

import pytest
from _testkit import child_env
from file_bridge import fs
from kernel_state.tools import load_bundles

from exec_bridge import _sys

HELPER = resources.files("py_agent").joinpath("fantastic_agent.py")

ECHO = """\
import os, sys, time
import fantastic_agent as fa

def echo(p):
    fa.emit({"type": "echoed", "text": p.get("text")})
    return {"echo": p.get("text"), "pid": os.getpid()}

def die(p):
    sys.stderr.write("dying on purpose\\n")
    sys.stderr.flush()
    os._exit(3)

def chatty(p):
    sys.stdout.write("x" * 300000)
    sys.stdout.flush()
    return {"chatted": True}

def slow(p):
    time.sleep(30)
    return {"late": True}

def ask(p):
    return {"asked": fa.send(p["target"], {"type": "reflect"}).get("id")}

fa.serve({"echo": echo, "die": die, "slow": slow, "ask": ask, "chatty": chatty})
"""

VERBS = {
    "echo": "args: text:str. Returns {echo, pid}.",
    "die": "No args. The program exits.",
    "slow": "No args. Sleeps 30 s.",
    "ask": "args: target:str. Reflects target as this agent.",
    "chatty": "No args. Writes 300 kB to stdout.",
}


def _bundle(root: Path, name: str, *, program: str = ECHO, verbs=None, **extra) -> Path:
    folder = root / ".fantastic" / "bundles" / name
    folder.mkdir(parents=True, exist_ok=True)
    shutil.copy(HELPER, folder / "fantastic_agent.py")
    (folder / "agent.py").write_text(program)
    lines = [
        "base: py_agent",
        f"sentence: The {name} agent.",
        f"command: [{sys.executable}, agent.py]",
        "verbs:",
    ]
    for v, d in (verbs or VERBS).items():
        lines.append(f"  {v}: {d!r}")
    # what the OS needs to start the program at all (`exec_bridge._sys.OS_ENV`:
    # on Windows the connector's socket needs SYSTEMROOT)
    if (os_env := child_env()) and "record" not in extra:
        extra["record"] = "{inherit_env: [%s]}" % ", ".join(os_env)
    for k, v in extra.items():
        lines.append(f"{k}: {v}")
    (folder / "bundle.yaml").write_text("\n".join(lines) + "\n")
    return folder


def _load(kernel) -> None:
    load_bundles(fs.Root(kernel.ctx.workdir, ".fantastic"))


async def _call(kernel, target, payload, timeout=20):
    out = await kernel.send(
        [{"target": target, "payload": payload, "timeout": timeout}]
    )
    return out[0]["result"]


async def _create(kernel, handler_module, id_):
    return await _call(
        kernel,
        kernel.id,
        {
            "type": "create_agent",
            "parent_id": kernel.id,
            "handler_module": handler_module,
            "id": id_,
        },
    )


async def _gone(pid: int) -> bool:
    """Has process `pid` exited (within 5 s)?"""
    for _ in range(50):
        if not _sys.pid_alive(pid):
            return True
        await asyncio.sleep(0.1)
    return False


@pytest.fixture(autouse=True)
def _forget_folders():
    """The folder registry is process-wide (one kernel per process): drop what a
    test loaded, so no other test's catalog sees it."""
    from kernel import _bundles

    yield
    _bundles._FOLDER.clear()
    _bundles._FOLDER_ROWS.clear()


@pytest.fixture
def workdir(kernel):
    return Path(kernel.ctx.workdir)


async def test_an_instance_runs_its_program_and_answers_the_bundles_verbs(
    kernel, workdir
):
    _bundle(workdir, "echoer")
    _load(kernel)
    r = await _create(kernel, "echoer", "e1")
    assert r.get("id") == "e1", r
    inbox = kernel.ctx.ensure_inbox("_t_watch")
    kernel.watch("e1", "_t_watch")
    out = await _call(kernel, "e1", {"type": "echo", "text": "hi"})
    assert out["echo"] == "hi" and out["pid"] != os.getpid(), out
    events = []
    while not inbox.empty():
        events.append(inbox.get_nowait())
    for _ in range(50):
        if any(e.get("type") == "echoed" for e in events):
            break
        events.append(await asyncio.wait_for(inbox.get(), 2))
    assert any(e.get("type") == "echoed" and e.get("text") == "hi" for e in events)
    rf = await _call(kernel, "e1", {"type": "reflect"})
    assert (
        rf["base"] == "py_agent"
        and rf["program"] == "running"
        and "OS rights" in rf["security"]
    )
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "e1"})


async def test_docs_and_the_catalog_read_bundle_yaml(kernel, workdir):
    _bundle(workdir, "documented")
    _load(kernel)
    d = await _call(kernel, "kernel", {"type": "docs", "of": "documented"})
    assert d["verbs"]["echo"] == "args: text:str", d
    cat = (
        await _call(
            kernel, "kernel", {"type": "reflect", "tree": "none", "bundles": "all"}
        )
    )["bundles"]
    row = next(b for b in cat if b["name"] == "documented")
    assert row["base"] == "py_agent" and row["handler_module"] == "documented"
    assert row["sentence"] == "The documented agent." and "error" not in row


async def test_a_program_that_serves_other_verbs_than_declared_stops_saying_so(
    kernel, workdir
):
    _bundle(workdir, "liar", verbs={**VERBS, "extra": "No args."})
    _load(kernel)
    await _create(kernel, "liar", "l1")
    out = await _call(kernel, "l1", {"type": "echo", "text": "x"})
    assert out["reason"] == "transport_closed" and "must match" in out["error"], out
    rf = await _call(kernel, "l1", {"type": "reflect"})
    assert rf["program"] == "stopped" and "must match" in rf["program_error"]
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "l1"})


async def test_an_exit_settles_the_call_and_nothing_respawns(kernel, workdir):
    _bundle(workdir, "mortal")
    _load(kernel)
    await _create(kernel, "mortal", "m1")
    out = await _call(kernel, "m1", {"type": "die"})
    assert out["reason"] == "transport_closed" and "exited (code 3)" in out["error"], (
        out
    )
    rf = await _call(kernel, "m1", {"type": "reflect"})
    assert "dying on purpose" in rf["stderr_tail"], rf
    again = await _call(kernel, "m1", {"type": "echo", "text": "x"})
    assert again["reason"] == "transport_closed" and "reload_bundle" in again["hint"], (
        again
    )
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "m1"})


async def test_reload_restarts_the_instance_and_answers_the_inflight_call(
    kernel, workdir
):
    folder = _bundle(workdir, "reloaded")
    _load(kernel)
    await _create(kernel, "reloaded", "r1")
    first = await _call(kernel, "r1", {"type": "echo", "text": "a"})
    pending = asyncio.ensure_future(_call(kernel, "r1", {"type": "slow"}, timeout=60))
    await asyncio.sleep(0.5)
    (folder / "agent.py").write_text(
        ECHO.replace('"echo": p.get("text")', '"echo": "v2:" + p.get("text")')
    )
    r = await _call(kernel, "kernel", {"type": "reload_bundle", "name": "reloaded"})
    assert r == {"reloaded": "reloaded", "instances": ["r1"]}, r
    late = await pending
    assert late["reason"] == "transport_closed" and "reloaded" in late["error"], late
    second = await _call(kernel, "r1", {"type": "echo", "text": "a"})
    assert second["echo"] == "v2:a" and second["pid"] != first["pid"], second
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "r1"})


async def test_a_failed_reload_keeps_the_previous_load_running(kernel, workdir):
    folder = _bundle(workdir, "kept")
    _load(kernel)
    await _create(kernel, "kept", "k1")
    first = await _call(kernel, "k1", {"type": "echo", "text": "a"})
    (folder / "bundle.yaml").write_text("base: py_agent\n")  # sentence gone
    r = await _call(kernel, "kernel", {"type": "reload_bundle", "name": "kept"})
    assert r.get("reason") == "invalid_meta" and "sentence" in r["error"], r
    # the live instance runs the previous load, untouched; the catalog keeps it too
    again = await _call(kernel, "k1", {"type": "echo", "text": "b"})
    assert again == {"echo": "b", "pid": first["pid"]}, again
    cat = (
        await _call(
            kernel, "kernel", {"type": "reflect", "tree": "none", "bundles": "all"}
        )
    )["bundles"]
    row = next(b for b in cat if b["name"] == "kept")
    assert row.get("base") == "py_agent" and "error" not in row, row
    # and delete still reaches the program
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "k1"})
    assert await _gone(first["pid"])


async def test_a_send_from_the_program_runs_as_the_instance(kernel, workdir):
    _bundle(workdir, "asker")
    _load(kernel)
    await _create(kernel, "asker", "a1")
    # no grant on the target: the program's send is the INSTANCE's, refused at egress
    out = await _call(kernel, "a1", {"type": "ask", "target": "kernel"})
    assert out.get("asked") is None
    await _call(
        kernel,
        kernel.id,
        {"type": "update_agent", "id": "a1", "can_call": {kernel.id: "*"}},
    )
    cb = dict(kernel.record.get("callable_by") or {})
    await _call(
        kernel,
        kernel.id,
        {"type": "update_agent", "id": kernel.id, "callable_by": {**cb, "a1": "*"}},
    )
    out = await _call(kernel, "a1", {"type": "ask", "target": "kernel"})
    assert out["asked"] == kernel.id, out
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "a1"})


async def test_delete_kills_the_program(kernel, workdir):
    _bundle(workdir, "killed")
    _load(kernel)
    await _create(kernel, "killed", "k1")
    pid = (await _call(kernel, "k1", {"type": "echo", "text": "x"}))["pid"]
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "k1"})
    assert await _gone(pid), f"program {pid} still alive after delete"


@pytest.mark.parametrize(
    "name, yaml_text, fragment",
    [
        ("nobase", "sentence: x\n", "base: required"),
        ("badbase", "base: no_such_agent\nsentence: x\n", "known: py_agent"),
        (
            "nocmd",
            "base: py_agent\nsentence: x\nverbs: {a: 'b'}\n",
            "command: required",
        ),
        (
            "noverbs",
            f"base: py_agent\nsentence: x\ncommand: [{sys.executable}, a.py]\n",
            "verbs: required",
        ),
        ("exec_bridge", "base: py_agent\nsentence: x\n", "built-in bundle"),
    ],
)
async def test_a_folder_that_cannot_load_is_listed_with_its_error(
    kernel, workdir, name, yaml_text, fragment
):
    folder = workdir / ".fantastic" / "bundles" / name
    folder.mkdir(parents=True)
    (folder / "bundle.yaml").write_text(yaml_text)
    _load(kernel)
    cat = (
        await _call(
            kernel, "kernel", {"type": "reflect", "tree": "none", "bundles": "all"}
        )
    )["bundles"]
    row = next(b for b in cat if b["name"] == name and "folder" in b)
    assert fragment in row["error"], row
    # a record naming it is kept (as for any handler this runtime cannot load);
    # a call to it is refused
    await _create(kernel, name, f"x_{name}")
    r = await _call(kernel, f"x_{name}", {"type": "ping"})
    assert r.get("reason") == "unhandled", r


async def test_a_bundle_folder_linking_outside_is_refused(kernel, workdir, tmp_path):
    """A `bundles/<name>` that links OUTSIDE the running dir is refused by the `fs`
    clamp, never loaded (twin of the Rust test of the same name)."""
    outside = tmp_path / "outside_root"
    outside.mkdir()
    real = _bundle(workdir, "escape_src")
    away = outside / "escape"
    real.rename(away)
    link = workdir / ".fantastic" / "bundles" / "escape"
    link.symlink_to(away, target_is_directory=True)
    _load(kernel)
    cat = (
        await _call(
            kernel, "kernel", {"type": "reflect", "tree": "none", "bundles": "all"}
        )
    )["bundles"]
    row = next(b for b in cat if b["name"] == "escape" and "folder" in b)
    assert "escapes root" in row.get("error", ""), row
    from kernel import _bundles

    assert "escape" not in _bundles._FOLDER
    await _create(kernel, "escape", "x_escape")
    r = await _call(kernel, "x_escape", {"type": "echo", "text": "hi"})
    assert r.get("reason") == "unhandled", r


async def test_py_agent_itself_is_a_base_not_an_agent(kernel):
    r = await _create(kernel, "py_agent.tools", "p0")
    assert r.get("reason") == "invalid_meta" and "is a base" in r["error"], r


async def test_cancelling_the_programs_task_kills_the_program(kernel, workdir):
    """The program's tasks are in `ps`; a `cancel` reaches the real process."""
    _bundle(workdir, "cancelled")
    _load(kernel)
    await _create(kernel, "cancelled", "c1")
    pid = (await _call(kernel, "c1", {"type": "echo", "text": "x"}))["pid"]
    tasks = (await _call(kernel, "kernel", {"type": "ps"}))["tasks"]
    prog = next(t for t in tasks if t["agent"] == "c1" and t["verb"] == "program")
    await _call(kernel, "kernel", {"type": "cancel", "task_id": prog["id"]})
    for _ in range(50):
        if not _sys.pid_alive(pid):
            break
        await asyncio.sleep(0.1)
    else:
        pytest.fail(f"program {pid} still alive after cancel")
    out = await _call(kernel, "c1", {"type": "echo", "text": "x"})
    assert out["reason"] == "transport_closed" and "cancelled" in out["error"], out
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "c1"})


async def test_a_chatty_program_does_not_block_on_its_own_output(kernel, workdir):
    """stdout/stderr are pipes; the kernel drains them, so 300 kB of output
    returns instead of filling the pipe and hanging the program."""
    _bundle(workdir, "chatty")
    _load(kernel)
    await _create(kernel, "chatty", "ch1")
    out = await _call(kernel, "ch1", {"type": "chatty"}, timeout=10)
    assert out == {"chatted": True}, out
    await _call(kernel, kernel.id, {"type": "delete_agent", "id": "ch1"})
