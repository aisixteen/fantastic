"""file_bridge — the SOURCE/SINK stream protocol (stateless cursor, RAW bytes)."""

from __future__ import annotations


async def _open(kernel, **meta):
    r = await kernel.send(
        "kernel_state",
        {
            "type": "create_agent",
            "handler_module": "file_bridge.tools",
            "ingress_rule": "allow_all",
            **meta,
        },
    )
    return r["id"]


async def test_write_stream_then_read_stream_round_trips(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    fid = await _open(seeded_kernel, root="sd")
    blob = bytes(range(256)) * 40  # 10240 bytes, binary
    # push in 3 chunks (truncate on first, append after) — RAW bytes, no base64
    off = 0
    for i, start in enumerate(range(0, len(blob), 4096)):
        chunk = blob[start : start + 4096]
        r = await seeded_kernel.send(
            fid,
            {
                "type": "write_stream",
                "path": "x.bin",
                "bytes": chunk,
                "truncate": i == 0,
            },
        )
        assert r["written"] == len(chunk)
        off = r["size"]
    assert off == len(blob)
    # pull it back with the cursor until eof — chunk is RAW bytes
    got = b""
    offset = 0
    while True:
        r = await seeded_kernel.send(
            fid,
            {"type": "read_stream", "path": "x.bin", "offset": offset, "length": 3000},
        )
        assert isinstance(r["bytes"], (bytes, bytearray))
        got += r["bytes"]
        offset = r["next_offset"]
        if r["eof"]:
            break
    assert got == blob
    assert r["size"] == len(blob)


async def test_stream_verbs_sealed_by_default(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # NO ingress_rule → sealed; read_stream must be denied + taught
    r = await seeded_kernel.send(
        "kernel_state",
        {"type": "create_agent", "handler_module": "file_bridge.tools", "root": "s"},
    )
    fid = r["id"]
    d = await seeded_kernel.send(fid, {"type": "read_stream", "path": "x"})
    assert d["reason"] == "unauthorized" and "ingress_rule" in d.get("hint", "")


async def test_pump_copies_between_bridges(seeded_kernel, tmp_path, monkeypatch):
    """The PUMP: a server-side SOURCE→SINK copy in one call, bound to both ends by
    id. Copy src/a.bin → dst/b.bin chunk-by-chunk; verify byte-for-byte."""
    monkeypatch.chdir(tmp_path)
    blob = bytes(range(256)) * 50  # 12800 bytes
    src = await _open(seeded_kernel, root="src")
    dst = await _open(seeded_kernel, root="dst")
    await seeded_kernel.send(
        src,
        {
            "type": "write_stream",
            "path": "a.bin",
            "bytes": blob,
            "truncate": True,
        },
    )
    r = await seeded_kernel.send(
        dst,
        {
            "type": "pump",
            "source": src,
            "source_path": "a.bin",
            "sink_path": "b.bin",
            "chunk": 4096,
        },
    )
    assert r.get("bytes") == len(blob) and r.get("chunks") >= 3, r
    got, off = b"", 0
    while True:
        c = await seeded_kernel.send(
            dst, {"type": "read_stream", "path": "b.bin", "offset": off}
        )
        got += c["bytes"]
        off = c["next_offset"]
        if c["eof"]:
            break
    assert got == blob


async def test_pump_refused_by_sealed_source(seeded_kernel, tmp_path, monkeypatch):
    """The pump only coordinates — each end SELF-gates. A SEALED source refuses the
    pump's read, so the pump fails (it never touches bytes itself)."""
    monkeypatch.chdir(tmp_path)
    dst = await _open(seeded_kernel, root="dst")
    r = await seeded_kernel.send(
        "kernel_state",
        {"type": "create_agent", "handler_module": "file_bridge.tools", "root": "src"},
    )
    sealed_src = r["id"]  # NO ingress_rule → sealed
    (tmp_path / "src").mkdir(parents=True, exist_ok=True)
    (tmp_path / "src" / "a.bin").write_bytes(b"hello")
    res = await seeded_kernel.send(
        dst,
        {
            "type": "pump",
            "source": sealed_src,
            "source_path": "a.bin",
            "sink_path": "b.bin",
        },
    )
    assert "error" in res and "read from" in res["error"]
