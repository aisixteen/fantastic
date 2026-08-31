"""coding/file_edit — the C2 edit organ, end to end over the real kernel.

Proves `file_bridge.patch_text_file` at the product surface: create an OPEN file
bridge, write a text file, apply a SEARCH/REPLACE patch (as RAW bytes on the
binary channel — quote/newline-heavy hunks never JSON-escaped), read it back
changed, then prove a non-matching hunk FAILS LOUD and leaves the file untouched
(all-or-nothing). No model in the loop — a deterministic capability probe, so it
runs free and, once the verb is ported, cross-runtime.
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

# e2e bootstrap — walk up to `.e2e-root`; no hard-coded depth.
sys.path.insert(
    0,
    str(
        next(
            d
            for d in Path(__file__).resolve().parents
            if (d / ".e2e-root").exists()
        )
    ),
)
from helpers.kernel import open_kernel  # noqa: E402
from helpers.verdict import verdict  # noqa: E402

BLOCK = "<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE"

# A FIXED id so the spawned-kernel driver can DECLARE it in the surface's reach
# (`open_kernel(reach=[FB_ID])`); `callable_by:{rest}` opens the other half of the
# gate for the REST surface. Both are inert for the in-process python plug.
FB_ID = "edit_fb"


async def _create_file_bridge(root):
    r = (
        await root.send(
            [
                {
                    "target": root.id,
                    "payload": {
                        "type": "create_agent",
                        "parent_id": root.id,
                        "id": FB_ID,
                        "handler_module": "file_bridge.tools",
                        "root": "work",
                        "ingress_rule": "allow_all_ingress",
                        "callable_by": {"rest": ["*"]},
                    },
                }
            ]
        )
    )[0]["result"]
    assert not r.get("error"), f"create file_bridge: {r}"
    return r.get("id", FB_ID)


async def _call(root, fid, payload):
    return (await root.send([{"target": fid, "payload": payload}]))[0]["result"]


async def _read(root, fid, path):
    # `read_stream` is a binary-REPLY verb; an empty `bytes` field forces the call
    # onto the binary channel so the compiled kernels answer it (they refuse a
    # stream verb over the text channel), and the chunk rides back raw.
    r = await _call(
        root,
        fid,
        {"type": "read_stream", "path": path, "offset": 0, "length": 1 << 20, "bytes": b""},
    )
    b = r.get("bytes") or b""
    return bytes(b).decode("utf-8") if isinstance(b, (bytes, bytearray)) else str(b)


async def main():
    root, _core, close = await open_kernel(prefix="coding_edit_", reach=[FB_ID])
    try:
        checks: list[tuple[str, bool]] = []
        fid = await _create_file_bridge(root)

        # seed a text file
        await _call(
            root,
            fid,
            {
                "type": "write_stream",
                "path": "m.py",
                "bytes": b"a = 1\nb = 2\nc = 3\n",
                "truncate": True,
            },
        )

        # apply a two-hunk patch, delivered as RAW bytes
        patch = (
            BLOCK.format(old="b = 2", new="b = 20")
            + "\n"
            + BLOCK.format(old="c = 3", new="c = 30")
        )
        r = await _call(
            root,
            fid,
            {"type": "patch_text_file", "path": "m.py", "bytes": patch.encode("utf-8")},
        )
        checks.append(
            ("patch applied", r.get("patched") is True and r.get("hunks") == 2)
        )
        checks.append(("content edited", await _read(root, fid, "m.py") == "a = 1\nb = 20\nc = 30\n"))

        # fail-loud: a non-matching hunk writes nothing and leaves the file untouched
        bad = BLOCK.format(old="does_not_exist", new="x")
        r2 = await _call(
            root,
            fid,
            {"type": "patch_text_file", "path": "m.py", "bytes": bad.encode("utf-8")},
        )
        checks.append(
            ("no-match fails loud", r2.get("reason") == "no_match" and "patched" not in r2)
        )
        checks.append(
            ("file untouched on failure", await _read(root, fid, "m.py") == "a = 1\nb = 20\nc = 30\n")
        )

        for name, ok in checks:
            print(f"  [{'ok' if ok else 'XX'}] {name}")
        allok = all(ok for _, ok in checks)
        verdict(
            allok,
            "patch_text_file: apply + fail-loud + all-or-nothing"
            if allok
            else "a patch_text_file check failed",
        )
    finally:
        await close()


if __name__ == "__main__":
    asyncio.run(main())
