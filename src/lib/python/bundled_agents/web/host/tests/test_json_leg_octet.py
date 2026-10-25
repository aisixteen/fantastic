"""web JSON leg — a reply that HOLDS a chunk is octet, an empty chunk included.

Twin of fantastic-web tests.rs::json_leg_read_stream_answers_octet_including_empty_chunk.
An empty chunk is a zero-length octet body; a refusal stays JSON.
"""

from __future__ import annotations

import json

from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_rest.tools import _make_post_endpoint
from _testkit import wire


def _mount(kernel):
    app = FastAPI()
    app.add_api_route(
        "/rest_x/{target_id}",
        _make_post_endpoint("rest_x", kernel),
        methods=["POST"],
    )
    return app


def test_json_leg_read_stream_answers_octet_including_empty_chunk(
    seeded_kernel, tmp_path, monkeypatch
):
    monkeypatch.chdir(tmp_path)
    seeded_kernel.create(
        "web_rest.tools",
        id="rest_x",
        ingress_rule="allow_all_ingress",
        can_call={"fb": ["*"]},
    )
    wire(seeded_kernel, "rest_x", "kernel_state")
    seeded_kernel.create(
        "file_bridge.tools",
        allow=["."],
        id="fb",
        root="work",
        ingress_rule="allow_all_ingress",
        callable_by={"rest_x": ["*"]},
    )
    (tmp_path / "work").mkdir(parents=True, exist_ok=True)
    (tmp_path / "work" / "hello.txt").write_bytes(b"hello world\n")
    (tmp_path / "work" / "empty.txt").write_bytes(b"")

    with TestClient(_mount(seeded_kernel)) as c:
        r = c.post("/rest_x/fb", json={"type": "read_stream", "path": "hello.txt"})
        assert r.status_code == 200
        assert r.headers["content-type"].startswith("application/octet-stream")
        assert r.headers["x-fantastic-bytes-field"] == "bytes"
        meta = json.loads(r.headers["x-fantastic-payload"])
        assert meta["path"] == "hello.txt"
        assert meta["offset"] == 0
        assert meta["next_offset"] == 12
        assert meta["eof"] is True
        assert meta["size"] == 12
        assert "origin" in meta
        assert "bytes" not in meta
        assert r.content == b"hello world\n"

        # The heart: an EMPTY chunk is still a zero-length octet body.
        r = c.post("/rest_x/fb", json={"type": "read_stream", "path": "empty.txt"})
        assert r.status_code == 200
        assert r.headers["content-type"].startswith("application/octet-stream")
        assert r.headers["content-length"] == "0"
        assert r.content == b""
        assert r.headers["x-fantastic-bytes-field"] == "bytes"

        # A refusal holds no chunk and stays JSON.
        r = c.post("/rest_x/fb", json={"type": "read_stream", "path": "nope.txt"})
        assert r.status_code == 200
        assert r.headers["content-type"].startswith("application/json")
        assert r.json()["reason"] == "not_found"
