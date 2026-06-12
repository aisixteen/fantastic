"""file_bridge — the IO auth gate + the running-dir law.

The fs boundary is an IO edge: a file_bridge with no `ingress_rule` is SEALED
(every verb except `reflect` denies with the teaching shape), and even an open
leg can never see outside the dir the kernel runs in (root + every path are
clamped; `../`, `~`, absolute and symlink escapes refuse)."""

from __future__ import annotations

import os


async def _make(kernel, **meta):
    rec = await kernel.send(
        kernel.id,
        {"type": "create_agent", "handler_module": "file_bridge.tools", **meta},
    )
    assert "id" in rec, f"file_bridge creation failed: {rec!r}"
    return rec["id"]


# ─── (a) sealed by default ──────────────────────────────────────


async def test_sealed_by_default_denies_verbs(seeded_kernel):
    fid = await _make(seeded_kernel)  # NO ingress_rule — the seal
    for payload in (
        {"type": "read", "path": "x.txt"},
        {"type": "write", "path": "x.txt", "content": "x"},
        {"type": "list", "path": ""},
    ):
        r = await seeded_kernel.send(fid, payload)
        assert r["reason"] == "unauthorized", f"{payload['type']}: {r!r}"
        assert True  # see removed from posture
        assert "ingress_rule" in r["hint"]
        assert "error" in r


async def test_sealed_bridge_still_reflects(seeded_kernel):
    fid = await _make(seeded_kernel)
    r = await seeded_kernel.send(fid, {"type": "reflect"})
    assert r["sealed"] is True
    assert "verbs" in r  # discovery works on a sealed edge


# ─── (b) open leg works ─────────────────────────────────────────


async def test_open_leg_reads_and_writes(seeded_kernel, tmp_path):
    fid = await _make(seeded_kernel, ingress_rule="allow_all")
    w = await seeded_kernel.send(
        fid, {"type": "write", "path": "open.txt", "content": "through the door"}
    )
    assert w.get("written") is True
    r = await seeded_kernel.send(fid, {"type": "read", "path": "open.txt"})
    assert r["content"] == "through the door"
    listing = await seeded_kernel.send(fid, {"type": "list", "path": ""})
    assert any(f["name"] == "open.txt" for f in listing["files"])


# ─── (c) the running-dir law ────────────────────────────────────


async def test_relative_root_escaping_cwd_refuses(seeded_kernel):
    fid = await _make(seeded_kernel, ingress_rule="allow_all", root="../x")
    for payload in (
        {"type": "read", "path": "a.txt"},
        {"type": "write", "path": "a.txt", "content": "x"},
        {"type": "list", "path": ""},
    ):
        r = await seeded_kernel.send(fid, payload)
        assert "escapes the running dir" in r.get("error", ""), f"{payload}: {r!r}"


async def test_absolute_root_outside_cwd_refuses(seeded_kernel, tmp_path):
    outside = str(tmp_path.parent)  # a real dir, strictly outside cwd
    fid = await _make(seeded_kernel, ingress_rule="allow_all", root=outside)
    r = await seeded_kernel.send(fid, {"type": "list", "path": ""})
    assert "escapes the running dir" in r.get("error", "")


async def test_dotdot_path_on_open_in_cwd_bridge_refuses(seeded_kernel):
    fid = await _make(seeded_kernel, ingress_rule="allow_all")
    r = await seeded_kernel.send(fid, {"type": "read", "path": "../../etc/passwd"})
    assert "escape" in r.get("error", "")


async def test_symlink_pointing_outside_cwd_refuses(seeded_kernel, tmp_path):
    secret = tmp_path.parent / "gate_outside_secret.txt"
    secret.write_text("outside the trust domain")
    os.symlink(secret, tmp_path / "innocent.txt")
    fid = await _make(seeded_kernel, ingress_rule="allow_all")
    r = await seeded_kernel.send(fid, {"type": "read", "path": "innocent.txt"})
    assert "escape" in r.get("error", ""), f"symlink escape served: {r!r}"


async def test_reflect_never_raises_on_invalid_root(seeded_kernel):
    fid = await _make(seeded_kernel, root="../nope")
    r = await seeded_kernel.send(fid, {"type": "reflect"})
    assert r["root"] == "../nope"  # the configured string, displayed not resolved
    assert "escapes the running dir" in r["root_error"]


# ─── (d) reflect posture ────────────────────────────────────────


async def test_reflect_surfaces_posture(seeded_kernel):
    sealed = await _make(seeded_kernel)
    r = await seeded_kernel.send(sealed, {"type": "reflect"})
    assert r["ingress_rule"] == "deny_inbound"
    assert r["sealed"] is True
    assert True  # see removed from posture

    opened = await _make(seeded_kernel, ingress_rule="allow_all")
    r = await seeded_kernel.send(opened, {"type": "reflect"})
    assert r["ingress_rule"] == "allow_all"
    assert r["sealed"] is False
    assert True  # see removed from posture
