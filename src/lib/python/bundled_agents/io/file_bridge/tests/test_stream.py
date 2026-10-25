"""file_bridge — the SOURCE/SINK stream protocol (stateless cursor, RAW bytes)."""

from __future__ import annotations


async def _open(kernel, **meta):
    r = (
        await kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "ingress_rule": "allow_all_ingress",
                        **meta,
                    },
                }
            ]
        )
    )[0]["result"]
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
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": fid,
                        "payload": {
                            "type": "write_stream",
                            "path": "x.bin",
                            "bytes": chunk,
                            "truncate": i == 0,
                        },
                    }
                ]
            )
        )[0]["result"]
        assert r["written"] == len(chunk)
        off = r["size"]
    assert off == len(blob)
    # pull it back with the cursor until eof — chunk is RAW bytes
    got = b""
    offset = 0
    while True:
        r = (
            await seeded_kernel.send(
                [
                    {
                        "target": fid,
                        "payload": {
                            "type": "read_stream",
                            "path": "x.bin",
                            "offset": offset,
                            "length": 3000,
                        },
                    }
                ]
            )
        )[0]["result"]
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
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "s",
                    },
                }
            ]
        )
    )[0]["result"]
    fid = r["id"]
    d = (
        await seeded_kernel.send(
            [{"target": fid, "payload": {"type": "read_stream", "path": "x"}}]
        )
    )[0]["result"]
    assert d["reason"] == "unauthorized" and "ingress_rule" in d.get("hint", "")


async def test_pump_copies_between_bridges(seeded_kernel, tmp_path, monkeypatch):
    """The PUMP: a server-side SOURCE→SINK copy in one call, bound to both ends by
    id. Copy src/a.bin → dst/b.bin chunk-by-chunk; verify byte-for-byte."""
    monkeypatch.chdir(tmp_path)
    blob = bytes(range(256)) * 50  # 12800 bytes
    src = await _open(seeded_kernel, root="src")
    # The pump has dst PULL from src (read_stream) — a lateral edge, so deny-all
    # needs an explicit grant on the sink for exactly that verb on the source.
    dst = await _open(seeded_kernel, root="dst", can_call={src: ["read_stream"]})
    # Two-sided deny-all: the source must reciprocally ACCEPT the sink's read_stream.
    src_agent = seeded_kernel.ctx.agents.get(src)
    cb = dict(src_agent._meta.get("callable_by") or {})
    cb[dst] = ["read_stream"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel_state",
                    "payload": {"type": "update_agent", "id": src, "callable_by": cb},
                }
            ]
        )
    )[0]["result"]
    (
        await seeded_kernel.send(
            [
                {
                    "target": src,
                    "payload": {
                        "type": "write_stream",
                        "path": "a.bin",
                        "bytes": blob,
                        "truncate": True,
                    },
                }
            ]
        )
    )[0]["result"]
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": dst,
                    "payload": {
                        "type": "pump",
                        "source": src,
                        "source_path": "a.bin",
                        "sink_path": "b.bin",
                        "chunk": 4096,
                    },
                }
            ]
        )
    )[0]["result"]
    assert r.get("bytes") == len(blob) and r.get("chunks") >= 3, r
    got, off = b"", 0
    while True:
        c = (
            await seeded_kernel.send(
                [
                    {
                        "target": dst,
                        "payload": {
                            "type": "read_stream",
                            "path": "b.bin",
                            "offset": off,
                        },
                    }
                ]
            )
        )[0]["result"]
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
    r = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "src",
                    },
                }
            ]
        )
    )[0]["result"]
    sealed_src = r["id"]  # NO ingress_rule → sealed
    (tmp_path / "src").mkdir(parents=True, exist_ok=True)
    (tmp_path / "src" / "a.bin").write_bytes(b"hello")
    res = (
        await seeded_kernel.send(
            [
                {
                    "target": dst,
                    "payload": {
                        "type": "pump",
                        "source": sealed_src,
                        "source_path": "a.bin",
                        "sink_path": "b.bin",
                    },
                }
            ]
        )
    )[0]["result"]
    assert "error" in res and "read from" in res["error"]


async def test_pump_names_a_refused_write_and_the_sealed_read(
    seeded_kernel, tmp_path, monkeypatch
):
    """The two refusals the pump composes, exactly: the sink's write and the
    source's read, each wrapped as a transport_error naming the end that failed."""
    monkeypatch.chdir(tmp_path)
    src = await _open(seeded_kernel, root="src")
    (tmp_path / "src").mkdir(parents=True, exist_ok=True)
    (tmp_path / "src" / "a.bin").write_bytes(b"hello")
    sealed = (
        await seeded_kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "create_agent",
                        "parent_id": "kernel_state",
                        "handler_module": "file_bridge.tools",
                        "allow": ["."],
                        "root": "dst",
                        "id": "sealed_sink",
                    },
                }
            ]
        )
    )[0]["result"]["id"]
    res = (
        await seeded_kernel.send(
            [
                {
                    "target": src,
                    "payload": {
                        "type": "pump",
                        "source_path": "a.bin",
                        "sink": sealed,
                        "sink_path": "b.bin",
                    },
                }
            ]
        )
    )[0]["result"]
    assert res["reason"] == "transport_error", res
    assert res["error"].startswith("pump: write to 'sealed_sink' failed: {"), res
    assert "unauthorized" in res["error"]
    missing = (
        await seeded_kernel.send(
            [
                {
                    "target": src,
                    "payload": {
                        "type": "pump",
                        "source_path": "nope.bin",
                        "sink_path": "out.bin",
                    },
                }
            ]
        )
    )[0]["result"]
    assert missing["reason"] == "transport_error", missing
    assert missing["error"].startswith(f"pump: read from {src!r} failed: {{"), missing


async def test_a_pump_onto_its_own_source_is_refused(
    seeded_kernel, file_bridge, tmp_path
):
    """With only `source_path` the sink and sink_path default to the source, and
    the first write's `truncate` would cut the file the next read comes from —
    shrinking it under a success reply. Refused, the path judged
    normalised (`./x` is `x`); the file is untouched."""
    k, fb = seeded_kernel, file_bridge
    data = bytes(range(256)) * 800
    (tmp_path / "self.bin").write_bytes(data)
    want = {
        "error": "pump: sink_path must be another file than source_path when the "
        "sink is the source — a pump onto its own source truncates it after the "
        "first chunk",
        "reason": "bad_arg",
        "arg": "sink_path",
    }
    for extra in (
        {},
        {"sink_path": "self.bin"},
        {"sink_path": "./self.bin"},
        {"sink": fb, "sink_path": "sub/../self.bin"},
    ):
        out = (
            await k.send(
                [
                    {
                        "target": fb,
                        "payload": {"type": "pump", "source_path": "self.bin", **extra},
                    }
                ]
            )
        )[0]["result"]
        assert out == want, (extra, out)
    assert (tmp_path / "self.bin").read_bytes() == data
    # another file on the same bridge still copies
    out = (
        await k.send(
            [
                {
                    "target": fb,
                    "payload": {
                        "type": "pump",
                        "source_path": "self.bin",
                        "sink_path": "copy.bin",
                    },
                }
            ]
        )
    )[0]["result"]
    assert out["bytes"] == len(data), out
    assert (tmp_path / "copy.bin").read_bytes() == data


async def test_pump_takes_source_path_only(seeded_kernel, file_bridge, tmp_path):
    """ARCH2-8: `path` is no alias for `source_path` — the verb doc lists only
    `source_path`, so a `path` names nothing and the read end refuses the empty
    source."""
    k, fb = seeded_kernel, file_bridge
    (tmp_path / "a.txt").write_bytes(b"hello")
    out = (
        await k.send(
            [
                {
                    "target": fb,
                    "payload": {"type": "pump", "path": "a.txt", "sink_path": "b.txt"},
                }
            ]
        )
    )[0]["result"]
    assert out["reason"] == "transport_error", out
    assert out["error"].startswith(f"pump: read from {fb!r} failed: {{"), out
    assert not (tmp_path / "b.txt").exists()


async def test_a_huge_read_length_reads_only_what_exists(
    seeded_kernel, tmp_path, monkeypatch
):
    """Rust tests.rs::huge_length_reads_only_what_exists: a length past EOF is clamped
    to what exists, never allocated."""
    from file_bridge import fs

    monkeypatch.chdir(tmp_path)
    (tmp_path / "a.txt").write_bytes(b"abc")
    fid = await _open(seeded_kernel)

    async def read(offset, length):
        return (
            await seeded_kernel.send(
                [
                    {
                        "target": fid,
                        "payload": {
                            "type": "read_stream",
                            "path": "a.txt",
                            "offset": offset,
                            "length": length,
                        },
                    }
                ]
            )
        )[0]["result"]

    for length in (10**15, 2**63 - 1):
        r = await read(0, length)
        assert "error" not in r, r
        assert r["bytes"] == b"abc" and r["eof"] is True, r
        assert r["size"] == 3 and r["next_offset"] == 3, r
    r = await read(1, 10**15)
    assert r["bytes"] == b"bc" and r["next_offset"] == 3 and r["eof"] is True, r
    root = fs.Root(seeded_kernel.ctx.workdir, "")
    assert fs.read_chunk(root, "a.txt", 0, 10**15) == (b"abc", 3)
