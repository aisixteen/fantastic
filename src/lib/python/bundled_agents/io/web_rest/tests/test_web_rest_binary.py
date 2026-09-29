"""web_rest — the octet-stream binary leg.

JSON has no bytes type, so a verb whose payload OR reply carries raw bytes
(a file_bridge `edit`, `write_stream`, `read_stream`) rides an octet-stream body: the
bytes ARE the body, the rest of the envelope is JSON on `X-Fantastic-Payload`, and
the one bytes-field is named by `X-Fantastic-Bytes-Field` (default `bytes`).
Symmetric across request and reply. Never base64.
"""

from __future__ import annotations

import json

from fastapi import FastAPI
from fastapi.testclient import TestClient

from web_rest.tools import _make_post_endpoint


def _mount(kernel):
    app = FastAPI()
    app.add_api_route(
        "/rest_xyz/{target_id}",
        _make_post_endpoint("rest_xyz", kernel),
        methods=["POST"],
    )
    return app


def test_binary_leg_request_and_reply_round_trip(seeded_kernel, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    # Grant the surface reach to the bridge on BOTH ends (egress + ingress) —
    # authority is deny-all by default.
    seeded_kernel.create(
        "web_rest.tools",
        id="rest_xyz",
        ingress_rule="allow_all_ingress",
        can_call={"fb": ["*"]},
    )
    seeded_kernel.create(
        "file_bridge.tools",
        allow=["."],
        id="fb",
        root="work",
        ingress_rule="allow_all_ingress",
        callable_by={"rest_xyz": ["*"]},
    )
    (tmp_path / "work").mkdir(parents=True, exist_ok=True)
    (tmp_path / "work" / "m.py").write_text("a = 1\nb = 2\n")

    with TestClient(_mount(seeded_kernel)) as c:
        # REQUEST binary: the patch is the octet body; the envelope is a header.
        patch = b"<<<<<<< SEARCH\nb = 2\n=======\nb = 20\n>>>>>>> REPLACE"
        r = c.post(
            "/rest_xyz/fb",
            content=patch,
            headers={
                "content-type": "application/octet-stream",
                "X-Fantastic-Payload": json.dumps(
                    {"type": "edit", "edits": [{"path": "m.py", "hunks": 1}]}
                ),
                "X-Fantastic-Bytes-Field": "bytes",
            },
        )
        assert r.status_code == 200
        assert r.json()["edited"] is True
        assert (tmp_path / "work" / "m.py").read_text() == "a = 1\nb = 20\n"

        # REPLY binary: read_stream's chunk comes back as an octet body.
        r2 = c.post(
            "/rest_xyz/fb",
            json={"type": "read_stream", "path": "m.py", "offset": 0, "length": 4096},
        )
        assert r2.status_code == 200
        assert r2.headers["content-type"].startswith("application/octet-stream")
        assert r2.content == b"a = 1\nb = 20\n"
        meta = json.loads(r2.headers["x-fantastic-payload"])
        assert meta["path"] == "m.py" and meta["eof"] is True and "bytes" not in meta


def test_json_path_unchanged(seeded_kernel):
    # A non-binary verb still goes plain JSON both ways.
    seeded_kernel.create(
        "web_rest.tools", id="rest_xyz", ingress_rule="allow_all_ingress"
    )
    with TestClient(_mount(seeded_kernel)) as c:
        r = c.post("/rest_xyz/kernel_state", json={"type": "reflect"})
        assert r.status_code == 200
        assert r.headers["content-type"].startswith("application/json")
        assert r.json()["id"] == "kernel_state"
