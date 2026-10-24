# `mcp_client` (E95) — a remote MCP server as an agent

Free: no model. A scripted fake MCP server (2026-07-28, Streamable HTTP; it checks
the request rules a server must: headers mirror the body, `_meta` on every request,
the Bearer key), a legacy one (2025-11-25, shaped like mcp.exa.ai: session +
version header, refuses modern requests) and the kernel's `mcp_client` agent on the packaged host of
`RUNTIME`, every call over `web_rest`. What it checks is listed in `probe.py`'s
docstring and the group README.

```sh
cd src/lib/python
uv run python ../../../e2e/run.py --only-free --id mcp_client          # all three
```
