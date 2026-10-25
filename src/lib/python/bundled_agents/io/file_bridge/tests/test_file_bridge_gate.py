"""file_bridge — the IO auth gate + the running-dir law.

The fs boundary is an IO edge: a file_bridge with no `ingress_rule` is SEALED
(every verb except `reflect` denies with the teaching shape), and even an open
leg can never see outside the dir the kernel runs in (root + every path are
clamped; `../`, `~`, absolute and symlink escapes refuse)."""

from __future__ import annotations

import os


async def _make(kernel, **meta):
    rec = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": kernel.id,
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
    assert "id" in rec, f"file_bridge creation failed: {rec!r}"
    return rec["id"]


# ─── (a) sealed by default ──────────────────────────────────────


async def test_sealed_by_default_denies_verbs(seeded_kernel):
    fid = await _make(seeded_kernel)  # NO ingress_rule — the seal
    for payload in (
        {"type": "read_stream", "path": "x.txt"},
        {"type": "write_stream", "path": "x.txt", "bytes": b"x", "truncate": True},
        {"type": "list", "path": ""},
    ):
        r = (await seeded_kernel.send([{"target": fid, "payload": payload}]))[0][
            "result"
        ]
        assert r["reason"] == "unauthorized", f"{payload['type']}: {r!r}"
        assert True  # see removed from posture
        assert "ingress_rule" in r["hint"]
        assert "error" in r


async def test_sealed_bridge_still_reflects(seeded_kernel):
    fid = await _make(seeded_kernel)
    r = (await seeded_kernel.send([{"target": fid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r["sealed"] is True
    assert r["id"] == fid  # discovery works on a sealed edge
    d = (
        await seeded_kernel.send(
            [{"target": "kernel", "payload": {"type": "docs", "of": fid}}]
        )
    )[0]["result"]
    assert "read" in d["verbs"]  # and so do its docs


# ─── (b) open leg works ─────────────────────────────────────────


async def test_open_leg_reads_and_writes(seeded_kernel, tmp_path):
    fid = await _make(seeded_kernel, ingress_rule="allow_all_ingress")
    w = (
        await seeded_kernel.send(
            [
                {
                    "target": fid,
                    "payload": {
                        "type": "write_stream",
                        "path": "open.txt",
                        "bytes": b"through the door",
                        "truncate": True,
                    },
                }
            ]
        )
    )[0]["result"]
    assert w.get("written") == 16  # write_stream reports BYTES written
    r = (
        await seeded_kernel.send(
            [{"target": fid, "payload": {"type": "read_stream", "path": "open.txt"}}]
        )
    )[0]["result"]
    assert bytes(r["bytes"]) == b"through the door"
    listing = (
        await seeded_kernel.send(
            [{"target": fid, "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    assert any(f["name"] == "open.txt" for f in listing["files"])


# ─── (c) the running-dir law ────────────────────────────────────


async def test_relative_root_escaping_cwd_refuses(seeded_kernel):
    fid = await _make(seeded_kernel, ingress_rule="allow_all_ingress", root="../x")
    for payload in (
        {"type": "read_stream", "path": "a.txt"},
        {"type": "write_stream", "path": "a.txt", "bytes": b"x", "truncate": True},
        {"type": "list", "path": ""},
    ):
        r = (await seeded_kernel.send([{"target": fid, "payload": payload}]))[0][
            "result"
        ]
        assert "escapes the running dir" in r.get("error", ""), f"{payload}: {r!r}"


async def test_absolute_root_outside_cwd_refuses(seeded_kernel, tmp_path):
    outside = str(tmp_path.parent)  # a real dir, strictly outside cwd
    fid = await _make(seeded_kernel, ingress_rule="allow_all_ingress", root=outside)
    r = (
        await seeded_kernel.send(
            [{"target": fid, "payload": {"type": "list", "path": ""}}]
        )
    )[0]["result"]
    assert "escapes the running dir" in r.get("error", "")


async def test_dotdot_path_on_open_in_cwd_bridge_refuses(seeded_kernel):
    fid = await _make(seeded_kernel, ingress_rule="allow_all_ingress")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": fid,
                    "payload": {"type": "read_stream", "path": "../../etc/passwd"},
                }
            ]
        )
    )[0]["result"]
    assert "escape" in r.get("error", "")


async def test_symlink_pointing_outside_cwd_refuses(seeded_kernel, tmp_path):
    secret = tmp_path.parent / "gate_outside_secret.txt"
    secret.write_text("outside the trust domain")
    os.symlink(secret, tmp_path / "innocent.txt")
    fid = await _make(seeded_kernel, ingress_rule="allow_all_ingress")
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": fid,
                    "payload": {"type": "read_stream", "path": "innocent.txt"},
                }
            ]
        )
    )[0]["result"]
    assert "escape" in r.get("error", ""), f"symlink escape served: {r!r}"


async def test_reflect_never_raises_on_invalid_root(seeded_kernel):
    fid = await _make(seeded_kernel, root="../nope")
    r = (await seeded_kernel.send([{"target": fid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r["root"] == "../nope"  # the configured string, displayed not resolved
    assert "escapes the running dir" in r["root_error"]


# ─── (d) reflect posture ────────────────────────────────────────


async def test_reflect_surfaces_posture(seeded_kernel):
    sealed = await _make(seeded_kernel)
    r = (
        await seeded_kernel.send([{"target": sealed, "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert r["ingress_rule"] == "deny_ingress"
    assert r["sealed"] is True
    assert True  # see removed from posture

    opened = await _make(seeded_kernel, ingress_rule="allow_all_ingress")
    r = (
        await seeded_kernel.send([{"target": opened, "payload": {"type": "reflect"}}])
    )[0]["result"]
    assert r["ingress_rule"] == "allow_all_ingress"
    assert r["sealed"] is False
    assert True  # see removed from posture


# ─── (e) write-time refusal, on BOTH write verbs ────────────────


async def test_an_unimplementable_rule_is_refused_at_CREATE_not_just_update(
    seeded_kernel,
):
    """`validate_meta` runs on `create_agent` as on `update_agent`: `create_agent …
    ingress_rule=bogus` must not return a record carrying `ingress_rule: "bogus"`
    while the IDENTICAL `update_agent` is refused by the IDENTICAL function.

    The argument the update path already makes applies verbatim at create — accepting a
    value you cannot honour and reporting success is the worst of both — and it is worse
    at create, because there is no prior good state to fall back to and the agent boots
    believing it has a security posture it does not have."""
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "x",
                        "ingress_rule": "bogus_rule",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in r, f"create must refuse an unimplementable rule, got {r}"
    assert "legal values" in r["error"], "the refusal must NAME what would work"
    assert r.get("reason") == "invalid_meta"
    assert "id" not in r, "nothing may be created"

    # And the same value on update is refused identically — one implementation.
    good = await _make(seeded_kernel, ingress_rule="allow_all_ingress")
    u = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": good,
                        "ingress_rule": "bogus_rule",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in u and "legal values" in u["error"]
    assert u.get("reason") == "invalid_meta"


async def test_a_legal_rule_still_creates(seeded_kernel):
    """The refusal must not have made the gate paranoid — the happy path is untouched."""
    fid = await _make(seeded_kernel, ingress_rule="allow_all_ingress")
    r = (await seeded_kernel.send([{"target": fid, "payload": {"type": "reflect"}}]))[
        0
    ]["result"]
    assert r["ingress_rule"] == "allow_all_ingress" and r["sealed"] is False


# ─── the path gate, every verb (Rust tests_path_gate.rs) ────────


async def test_every_verb_refuses_a_path_outside_the_list_and_changes_nothing(
    seeded_kernel, tmp_path, monkeypatch
):
    from pathlib import Path

    from file_bridge import tools

    monkeypatch.chdir(tmp_path)
    served = Path("served")
    (served / "sub").mkdir(parents=True)
    (served / "priv").mkdir()
    (served / "sub/in.txt").write_bytes(b"old\n")
    (served / "other.txt").write_bytes(b"secret\n")
    (served / "priv/p.txt").write_bytes(b"p\n")
    k = seeded_kernel
    fb = await _make(
        k, root="served", ingress_rule="allow_all_ingress", **{"allow": ["sub"]}
    )

    async def send(payload):
        return (await k.send([{"target": fb, "payload": payload}]))[0]["result"]

    def refused(r, label):
        assert "error" in r, f"{label}: {r!r}"
        assert r["reason"] == "not_found", f"{label}: {r!r}"
        assert "not exposed" in r["error"], f"{label}: {r!r}"

    refused(await send({"type": "list", "path": "priv"}), "list")
    refused(await send({"type": "read_stream", "path": "other.txt"}), "read_stream")
    refused(await send({"type": "watch_path", "path": "other.txt"}), "watch_path")
    assert fb not in tools._WATCH, "a refused watch_path armed something"

    refused(await send({"type": "write", "path": "new.txt", "bytes": b"x"}), "write")
    assert not (served / "new.txt").exists()

    refused(
        await send({"type": "append", "path": "other.txt", "bytes": b"x"}), "append"
    )
    assert (served / "other.txt").read_bytes() == b"secret\n"

    refused(
        await send(
            {"type": "write_stream", "path": "new.bin", "bytes": b"x", "truncate": True}
        ),
        "write_stream",
    )
    assert not (served / "new.bin").exists()

    refused(await send({"type": "delete", "path": "other.txt"}), "delete")
    assert (served / "other.txt").exists()

    refused(await send({"type": "mkdir", "path": "newdir"}), "mkdir")
    assert not (served / "newdir").exists()

    # undo: a real slot for other.txt, then the list narrows back
    r = (
        await k.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "update_agent",
                        "id": fb,
                        "allow": ["sub", "other.txt"],
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" not in r, r
    w = await send({"type": "write", "path": "other.txt", "bytes": b"changed\n"})
    assert "error" not in w, w
    r = (
        await k.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": fb, "allow": ["sub"]},
                }
            ]
        )
    )[0]["result"]
    assert "error" not in r, r
    refused(await send({"type": "undo", "path": "other.txt"}), "undo")
    assert (served / "other.txt").read_bytes() == b"changed\n"
    (served / "other.txt").write_bytes(b"secret\n")

    patch = (
        "<<<<<<< SEARCH\nold\n=======\nnew\n>>>>>>> REPLACE\n"
        "<<<<<<< SEARCH\nsecret\n=======\nchanged\n>>>>>>> REPLACE\n"
    )
    refused(
        await send(
            {
                "type": "edit",
                "edits": [
                    {"path": "sub/in.txt", "hunks": 1},
                    {"path": "other.txt", "hunks": 1},
                ],
                "bytes": patch.encode(),
            }
        ),
        "edit",
    )
    assert (served / "sub/in.txt").read_bytes() == b"old\n"
    assert (served / "other.txt").read_bytes() == b"secret\n"

    refused(
        await send(
            {"type": "rename", "old_path": "other.txt", "new_path": "sub/moved.txt"}
        ),
        "rename out",
    )
    assert (served / "other.txt").exists()
    assert not (served / "sub/moved.txt").exists()

    refused(
        await send(
            {
                "type": "rename",
                "old_path": "priv/p.txt",
                "new_path": "sub/imported.txt",
            }
        ),
        "rename in",
    )
    assert (served / "priv/p.txt").exists()
    assert not (served / "sub/imported.txt").exists()
