"""file bundle — filesystem root agent."""

from __future__ import annotations

import base64


async def test_reflect(seeded_kernel, file_agent):
    r = await seeded_kernel.send(file_agent, {"type": "reflect"})
    assert r["sentence"].startswith("Filesystem")
    assert r["readonly"] is False


async def test_write_then_read(seeded_kernel, file_agent, tmp_path):
    await seeded_kernel.send(
        file_agent, {"type": "write", "path": "hello.txt", "content": "hi there"}
    )
    r = await seeded_kernel.send(file_agent, {"type": "read", "path": "hello.txt"})
    assert r["content"] == "hi there"
    assert (tmp_path / "hello.txt").read_text() == "hi there"


async def test_write_creates_parent_dirs(seeded_kernel, file_agent, tmp_path):
    await seeded_kernel.send(
        file_agent, {"type": "write", "path": "a/b/c.txt", "content": "deep"}
    )
    assert (tmp_path / "a" / "b" / "c.txt").read_text() == "deep"


async def test_read_missing_file_errors(seeded_kernel, file_agent):
    r = await seeded_kernel.send(file_agent, {"type": "read", "path": "nope.txt"})
    assert "error" in r


async def test_read_image_returns_base64(seeded_kernel, file_agent, tmp_path):
    img_data = b"\x89PNG\r\n\x1a\nfake-png-bytes"
    (tmp_path / "img.png").write_bytes(img_data)
    r = await seeded_kernel.send(file_agent, {"type": "read", "path": "img.png"})
    assert "image_base64" in r
    assert base64.b64decode(r["image_base64"]) == img_data
    assert r["mime"] == "image/png"


async def test_list(seeded_kernel, file_agent, tmp_path):
    (tmp_path / "a.txt").write_text("a")
    (tmp_path / "b.txt").write_text("b")
    (tmp_path / "subdir").mkdir()
    r = await seeded_kernel.send(file_agent, {"type": "list", "path": ""})
    names = {f["name"] for f in r["files"]}
    assert {"a.txt", "b.txt", "subdir"} <= names


async def test_list_excludes_hidden_default(seeded_kernel, file_agent, tmp_path):
    (tmp_path / ".git").mkdir()
    (tmp_path / "visible.txt").write_text("v")
    r = await seeded_kernel.send(file_agent, {"type": "list", "path": ""})
    names = {f["name"] for f in r["files"]}
    assert ".git" not in names
    assert "visible.txt" in names


async def test_delete_file(seeded_kernel, file_agent, tmp_path):
    (tmp_path / "x.txt").write_text("x")
    r = await seeded_kernel.send(file_agent, {"type": "delete", "path": "x.txt"})
    assert r["deleted"] is True
    assert not (tmp_path / "x.txt").exists()


async def test_rename(seeded_kernel, file_agent, tmp_path):
    (tmp_path / "old.txt").write_text("o")
    r = await seeded_kernel.send(
        file_agent,
        {"type": "rename", "old_path": "old.txt", "new_path": "new.txt"},
    )
    assert r["new_path"] == "new.txt"
    assert (tmp_path / "new.txt").read_text() == "o"


async def test_mkdir(seeded_kernel, file_agent, tmp_path):
    r = await seeded_kernel.send(file_agent, {"type": "mkdir", "path": "newdir/sub"})
    assert r["created"] is True
    assert (tmp_path / "newdir" / "sub").is_dir()


async def test_path_safety_rejects_escape(seeded_kernel, file_agent):
    r = await seeded_kernel.send(
        file_agent, {"type": "read", "path": "../../etc/passwd"}
    )
    assert "error" in r
    assert "escape" in r["error"]


async def test_readonly_refuses_write(seeded_kernel, file_agent):
    await seeded_kernel.send(
        "core",
        {"type": "update_agent", "id": file_agent, "readonly": True},
    )
    r = await seeded_kernel.send(
        file_agent, {"type": "write", "path": "x.txt", "content": "x"}
    )
    assert "error" in r
    assert "readonly" in r["error"]


async def test_unknown_verb_errors(seeded_kernel, file_agent):
    r = await seeded_kernel.send(file_agent, {"type": "garbage"})
    assert "error" in r
