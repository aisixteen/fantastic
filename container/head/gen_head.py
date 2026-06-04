#!/usr/bin/env python3
"""Generate the container "head" page — ONE self-contained HTML showing all
readmes (main -> kernels -> containers) + the GitHub URL + how to drive the
kernel. Plain (readmes in <pre> sections) + a tiny inline-JS table of contents;
NO vendored markdown lib, no external assets.

The container's `head` runtime serves this at `/` on :80 (the python kernel is
also live there — reflect / web_ws / web_rest — so :80 is BOTH the human-readable
head AND a reflectable/bridgeable brain kernel).

Run from the repo root:  python3 container/head/gen_head.py > container/head/index.html
"""

from __future__ import annotations

import html
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
GITHUB = "https://github.com/Alexadar/fantastic_canvas"

# (section group, title, repo-relative path) — order is the read order an LLM
# should follow: main first, then each kernel, then the container.
DOCS = [
    ("Main", "Root — Aisixteen Fantastic", "README.md"),
    ("Kernels", "python kernel", "python/README.md"),
    ("Kernels", "rust kernel", "rust/README.md"),
    ("Kernels", "swift kernel", "swift/README.md"),
    ("Kernels", "ts frontend kernel", "ts/readme.md"),
    ("Containers", "container (this image)", "container/README.md"),
]

CSS = """
:root{color-scheme:dark}
*{box-sizing:border-box}
body{margin:0;background:#0b0b12;color:#e5e5e5;
  font:14px/1.6 'SF Mono','Menlo',ui-monospace,monospace}
a{color:#9db4ff;text-decoration:none}a:hover{text-decoration:underline}
header{padding:28px 32px 8px}
h1{margin:0 0 4px;font-size:20px;letter-spacing:.12em}
.tag{color:#8a8aa0;margin:0 0 14px}
.how{background:#14141f;border:1px solid #23233a;border-radius:8px;
  padding:14px 18px;margin:0 32px 8px;max-width:980px;white-space:pre-wrap}
.how code{color:#b6f0c0}
nav{padding:8px 32px 18px;max-width:980px}
nav b{color:#8a8aa0;font-weight:600;letter-spacing:.1em}
nav ul{list-style:none;padding:0;margin:6px 0 0;columns:2}
nav li{margin:2px 0}
section{padding:8px 32px 0;max-width:980px}
section h2{font-size:15px;letter-spacing:.08em;border-bottom:1px solid #23233a;
  padding-bottom:6px;margin-top:26px}
pre{background:#0f0f18;border:1px solid #1d1d2e;border-radius:8px;
  padding:14px 16px;overflow:auto;white-space:pre-wrap;word-break:break-word}
footer{padding:24px 32px 48px;color:#6a6a80;max-width:980px}
"""

HOW = f"""# This is a Fantastic kernel — its own description IS the API.
# You are reading the container HEAD: every readme below, in read order.
#
# Drive it (no client library):
#   reflect (machine head):  POST /rest/kernel  {{"type":"reflect","readme":true,"bundles":"all"}}
#   live verb calls (WS):    ws://<host>/web/ws
#   pick a runtime at launch: -e FANTASTIC_RUNTIME=python|rust|ts (default python)
#   image tag:               ghcr.io/alexadar/fantastic:latest
# Repo: {GITHUB}
"""


def _slug(s: str) -> str:
    return "".join(c if c.isalnum() else "-" for c in s.lower()).strip("-")


def main() -> int:
    parts: list[str] = []
    parts.append(f"<!doctype html><html lang=en><head><meta charset=utf-8>")
    parts.append("<meta name=viewport content='width=device-width,initial-scale=1'>")
    parts.append("<title>Fantastic — kernel head</title>")
    parts.append(f"<style>{CSS}</style></head><body>")
    parts.append("<header><h1>FANTASTIC — KERNEL HEAD</h1>")
    parts.append(
        "<p class=tag>pull · run · read this page → an LLM figures out the rest "
        f"from the kernels' own self-description · <a href='{GITHUB}'>{GITHUB}</a></p>"
    )
    parts.append(f"<div class=how>{html.escape(HOW)}</div></header>")

    # nav (grouped) — anchors filled by the sections below.
    nav = ["<nav>"]
    last_group = None
    sections = []
    for group, title, rel in DOCS:
        path = REPO / rel
        if not path.exists():
            continue
        sid = _slug(f"{group}-{title}")
        if group != last_group:
            if last_group is not None:
                nav.append("</ul>")
            nav.append(f"<b>{html.escape(group)}</b><ul>")
            last_group = group
        nav.append(f"<li><a href='#{sid}'>{html.escape(title)}</a> "
                   f"<span style='color:#5a5a70'>{html.escape(rel)}</span></li>")
        body = html.escape(path.read_text(encoding="utf-8", errors="replace"))
        sections.append(
            f"<section id='{sid}'><h2>{html.escape(group)} — {html.escape(title)} "
            f"<span style='color:#5a5a70;font-weight:400'>({html.escape(rel)})</span></h2>"
            f"<pre>{body}</pre></section>"
        )
    if last_group is not None:
        nav.append("</ul>")
    nav.append("</nav>")

    parts.append("".join(nav))
    parts.append("".join(sections))
    parts.append(
        f"<footer>Fantastic kernel head · generated from the repo readmes · "
        f"<a href='{GITHUB}'>{GITHUB}</a></footer>"
    )
    parts.append("</body></html>")
    sys.stdout.write("".join(parts))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
