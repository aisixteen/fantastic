"""The cold boot — `main._bootstrap` — and the root record it seeds.

Twins of the Rust kernel's `bootstrap/tests.rs`: a virgin dir seeds the root the
loader owns (`kernel_state`) under the dir's lock, the seed is the CONSTRUCTED
record (its grant baseline in order, the guid appended by the loader's boot), and
a tree on disk that fails validation refuses the boot instead of loading as-is.
"""

from __future__ import annotations

import pytest
import yaml

import main
from _testkit import boot_root
from kernel import Kernel, acquire_lock, release_lock
from kernel._state import SnapshotError
from kernel.modes.longrun import take_seed


@pytest.fixture(autouse=True)
def _cwd(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    yield
    release_lock()


def _seed_the_dir(kernel: Kernel) -> None:
    """What the first mode to take the dir's lock does: hold it, write the seed."""
    acquire_lock()
    take_seed(kernel)


def test_bootstrap_creates_root_on_virgin_workdir(tmp_path):
    kernel = Kernel()
    main._bootstrap(kernel)
    assert kernel.root is not None
    assert kernel.root.id == "kernel_state"
    # The seed is written by the first mode that takes the dir's lock.
    _seed_the_dir(kernel)
    assert (tmp_path / ".fantastic" / "agent.yaml").exists()
    assert (tmp_path / ".fantastic" / "lock.yaml").exists()


def test_root_is_the_kernel_state_loader_and_the_seed_is_the_constructed_record(
    tmp_path,
):
    """A virgin dir seeds the ROOT the loader owns, and the record written is the
    CONSTRUCTED root, so its meta is the structural grant baseline in order."""
    kernel = Kernel()
    main._bootstrap(kernel)
    assert kernel.root.id == "kernel_state"
    assert kernel.root.handler_module == "kernel_state.tools"
    _seed_the_dir(kernel)
    rec = yaml.safe_load((tmp_path / ".fantastic" / "agent.yaml").read_text())
    assert rec["id"] == "kernel_state"
    assert rec["handler_module"] == "kernel_state.tools"
    keys = list(rec)
    assert [k for k in keys if k in ("can_call", "callable_by")] == [
        "can_call",
        "callable_by",
    ], f"grant baseline in python's order: {keys}"


async def test_root_meta_order_is_grants_then_guid(tmp_path):
    """The root's meta order is `can_call, callable_by, guid`: the guid is APPENDED
    by the loader's boot, after the stamped grants."""
    root = boot_root()
    root.create(
        "file_bridge.tools",
        allow=["."],
        id="store",
        root=".fantastic",
        ingress_rule="allow_all_ingress",
    )
    (await root.send([{"target": root.id, "payload": {"type": "boot"}}]))[0]
    try:
        await root._fs_flush_loop.aflush()
        assert list(root._meta) == ["can_call", "callable_by", "guid"]
        assert list(root.record)[-3:] == ["can_call", "callable_by", "guid"]
        on_disk = yaml.safe_load((tmp_path / ".fantastic" / "agent.yaml").read_text())
        assert list(on_disk)[-3:] == ["can_call", "callable_by", "guid"], list(on_disk)
    finally:
        from kernel_state.tools import on_shutdown

        await on_shutdown(root)


def _plant(work, dir: str, body: str) -> None:
    at = work / ".fantastic" / "agents" / dir
    at.mkdir(parents=True)
    (at / "agent.yaml").write_text(body)


def _refusal(work, monkeypatch) -> str:
    monkeypatch.chdir(work)
    with pytest.raises(SnapshotError) as e:
        main._bootstrap(Kernel())
    return str(e.value)


def test_bootstrap_refuses_a_tree_that_fails_validation(tmp_path, monkeypatch):
    """`_bootstrap` is `kernel.load(read_tree(...))`, and `load` VALIDATES before it
    builds: a record on disk with an id `create_agent` would refuse, a reserved key,
    or a duplicate refuses the boot instead of loading as-is."""
    for dir, body, want in [
        ("kernel", "id: kernel\n", "record 'kernel': load: id "),
        (
            "rp",
            "id: rp\nroot_path: /elsewhere\n",
            "record 'rp' carries reserved key(s) ['root_path'] — "
            "these are Agent constructor parameters",
        ),
        ("seven", "id: 7\n", "record 7: load: id must be a non-empty string"),
    ]:
        work = tmp_path / dir
        _plant(work, dir, body)
        err = _refusal(work, monkeypatch)
        assert want in err, f"{dir}: {err}"
    # A duplicate id across two dirs.
    work = tmp_path / "dup"
    for dir in ("one", "two"):
        _plant(work, dir, "id: twin\n")
    err = _refusal(work, monkeypatch)
    assert "duplicate agent id 'twin' in snapshot" in err, err
