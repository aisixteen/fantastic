"""file_bridge is DENY-ALL at the path level: the `allow` list on the record names
what it exposes (a file, or everything under a directory; `.` = the whole root). No
list exposes nothing. Every path verb sees only exposed paths; anything else is
`not_found` whether or not it exists. The list is record state, set at create or
replaced by `update_agent`."""

from __future__ import annotations

from pathlib import Path

from fastapi.testclient import TestClient

from _testkit import as_agent
from web.app import make_app


async def _call(kernel, payload, target="fb", as_=None):
    if as_ is None:
        return (await kernel.send([{"target": target, "payload": payload}]))[0][
            "result"
        ]
    with as_agent(kernel, as_):
        return (await kernel.send([{"target": target, "payload": payload}]))[0][
            "result"
        ]


def _tree():
    Path("site/docs").mkdir(parents=True)
    Path("site/docs/a.pdf").write_bytes(b"%PDF-a")
    Path("site/docs/b.pdf").write_bytes(b"%PDF-b")
    Path("site/secret.txt").write_text("private\n")


def _bridge(kernel, **meta):
    rec = kernel.create(
        "file_bridge.tools",
        id="fb",
        root="site",
        ingress_rule="allow_all_ingress",
        **meta,
    )
    assert rec.get("id") == "fb", rec


def _read(kernel, path, as_=None):
    return _call(kernel, {"type": "read_stream", "path": path}, as_=as_)


async def test_a_bridge_with_no_list_exposes_nothing(kernel):
    _tree()
    _bridge(kernel)
    for path in ("docs/a.pdf", "secret.txt", "missing.txt"):
        r = await _read(kernel, path)
        assert r["reason"] == "not_found" and "not exposed" in r["error"], (path, r)
    # present or absent, the answer is the same — nothing leaks past the list
    a, m = await _read(kernel, "secret.txt"), await _read(kernel, "missing.txt")
    assert a["error"].replace("secret.txt", "X") == m["error"].replace(
        "missing.txt", "X"
    )
    assert (await _call(kernel, {"type": "list"}))["reason"] == "not_found"
    w = await _call(kernel, {"type": "write_stream", "path": "new.txt", "bytes": b"x"})
    assert w["reason"] == "not_found", w
    assert not Path("site/new.txt").exists()


async def test_one_file_exposes_that_file_and_the_way_to_it(kernel):
    _tree()
    _bridge(kernel, allow=["docs/a.pdf"])
    assert (await _read(kernel, "docs/a.pdf"))["bytes"] == b"%PDF-a"
    assert (await _read(kernel, "docs/b.pdf"))["reason"] == "not_found"
    assert (await _read(kernel, "secret.txt"))["reason"] == "not_found"
    top = await _call(kernel, {"type": "list"})
    assert [(f["name"], f["type"]) for f in top["files"]] == [("docs", "dir")]
    docs = await _call(kernel, {"type": "list", "path": "docs"})
    assert [f["name"] for f in docs["files"]] == ["a.pdf"]


async def test_a_directory_exposes_everything_under_it_and_dot_the_whole_root(kernel):
    _tree()
    _bridge(kernel, allow=["docs"])
    assert (await _read(kernel, "docs/b.pdf"))["bytes"] == b"%PDF-b"
    assert (await _read(kernel, "secret.txt"))["reason"] == "not_found"
    await kernel.delete("fb")
    _bridge(kernel, allow=["."])
    assert (await _read(kernel, "secret.txt"))["bytes"] == b"private\n"


async def test_the_list_is_a_record_field_update_agent_replaces(kernel):
    _tree()
    _bridge(kernel, allow=[])
    assert (await _read(kernel, "docs/a.pdf"))["reason"] == "not_found"
    out = await _call(
        kernel,
        {"type": "update_agent", "id": "fb", "allow": ["docs/a.pdf"]},
        target=kernel.id,
    )
    assert out.get("updated") is True, out
    assert (await _read(kernel, "docs/a.pdf"))["bytes"] == b"%PDF-a"
    for verb in ("allow", "disallow", "allowed"):
        r = await _call(kernel, {"type": verb, "path": "docs/b.pdf"})
        assert r["reason"] == "unhandled", (verb, r)


async def test_nothing_outside_the_root_can_be_read(kernel):
    _tree()
    _bridge(kernel, allow=["."])
    r = await _read(kernel, "../../etc/passwd")
    assert "escape" in r["error"], r  # the clamp answers first


async def test_the_list_is_validated_at_create(kernel):
    for bad in ("docs", ["../up"], [""]):
        r = await _call(
            kernel,
            {
                "type": "create_agent",
                "parent_id": kernel.id,
                "handler_module": "file_bridge.tools",
                "allow": bad,
            },
            target=kernel.id,
        )
        assert r.get("reason") == "invalid_meta", (bad, r)


async def test_writes_stay_inside_the_list_and_readonly_still_holds(kernel):
    _tree()
    _bridge(kernel, allow=["docs"])
    ok = await _call(
        kernel,
        {"type": "write_stream", "path": "docs/c.txt", "bytes": b"c", "truncate": True},
    )
    assert ok.get("written") == 1, ok
    out = await _call(
        kernel, {"type": "rename", "old_path": "docs/c.txt", "new_path": "c.txt"}
    )
    assert out["reason"] == "not_found", out  # the target is outside the list
    assert (await _call(kernel, {"type": "delete", "path": "secret.txt"}))[
        "reason"
    ] == "not_found"
    assert Path("site/secret.txt").exists()
    kernel.update("fb", readonly=True)
    r = await _call(
        kernel, {"type": "write_stream", "path": "docs/d.txt", "bytes": b"d"}
    )
    assert r["reason"] == "unauthorized", r


async def test_the_web_serves_only_what_the_list_exposes(kernel):
    _tree()
    _bridge(kernel, allow=["docs/a.pdf"], callable_by={"web": ["read_stream"]})
    # `port`: the authority the web's Host guard admits (audit W-2).
    kernel.create("web.tools", id="web", port=8088, can_call={"fb": ["read_stream"]})
    with TestClient(make_app("web", kernel), base_url="http://127.0.0.1:8088") as c:
        r = c.get("/fb/file/docs/a.pdf")
        assert r.status_code == 200 and r.content == b"%PDF-a"
        assert c.get("/fb/file/docs/b.pdf").status_code == 404
        assert c.get("/fb/file/secret.txt").status_code == 404
        assert c.get("/fb/file/missing.txt").status_code == 404


# ─── symlinks: BOTH ends must be exposed (audit 2026-09-28) ─────────────────
# The list was checked on the path AS ASKED while the fs follows links, so a link
# inside an exposed dir served whatever in-root file it pointed at.


def _links():
    _tree()
    Path("site/docs/leak").symlink_to("../secret.txt")
    Path("site/docs/sib").symlink_to("b.pdf")
    Path("site/shared").mkdir()
    Path("site/shared/c.txt").write_text("c\n")
    Path("site/docs/shared").symlink_to("../shared")


async def test_a_link_inside_the_list_to_a_file_outside_it_serves_nothing(kernel):
    _links()
    _bridge(kernel, allow=["docs"])
    r = await _read(kernel, "docs/leak")
    assert r["reason"] == "not_found" and "not exposed" in r["error"], r
    w = await _call(
        kernel, {"type": "write_stream", "path": "docs/leak", "bytes": b"pwn"}
    )
    assert w["reason"] == "not_found", w
    assert Path("site/secret.txt").read_text() == "private\n"
    names = [
        f["name"]
        for f in (await _call(kernel, {"type": "list", "path": "docs"}))["files"]
    ]
    assert "leak" not in names and "shared" not in names, names


async def test_a_link_that_stays_inside_the_list_still_serves(kernel):
    _links()
    _bridge(kernel, allow=["docs"])
    assert (await _read(kernel, "docs/sib"))["bytes"] == b"%PDF-b"


async def test_an_exposed_linked_dir_lists_what_it_serves(kernel):
    _links()
    _bridge(kernel, allow=["docs", "shared"])
    listed = await _call(kernel, {"type": "list", "path": "docs/shared"})
    assert [f["path"] for f in listed["files"]] == ["docs/shared/c.txt"], listed
    assert (await _read(kernel, "docs/shared/c.txt"))["bytes"] == b"c\n"


async def test_rename_checks_where_both_ends_land(kernel):
    _links()
    _bridge(kernel, allow=["docs"])
    out = await _call(
        kernel, {"type": "rename", "old_path": "docs/leak", "new_path": "docs/moved"}
    )
    assert out["reason"] == "not_found", out
    assert Path("site/secret.txt").exists()
