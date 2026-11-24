# `connectors/` — an outside service as an agent, measured at its wire

No model. Each test runs a scripted fake of the outside service and drives the
kernel's own connector agent on the rust host (the PACKAGED host, every
call through `web_rest`), checking what crosses the wire.

| test | what it measures |
|---|---|
| `mcp_client` (E95) | a remote MCP server (protocol 2026-07-28, or a legacy 2025-11-25 one through `initialize` + its session) as an agent: `server/discover` + every page of `tools/list` → the agent's verbs (`reflect mcp part=docs`, built from each tool's schema; unservable tools skipped and named); a call's arguments → `tools/call` with `Mcp-Name` / `Mcp-Param-*`; every result mapping (text, `isError` → error, `structuredContent`, an image → the reply's raw `bytes` with a later one `omitted`, an SSE answer, `input_required`, unknown `resultType`, a stream with no response, -32602, an unknown tool); the static key — an env-backed credential ref — as `Authorization: Bearer` on EVERY request, never in the ledger nor in any file of the state folder; `mcp` holds `creds: {call: [get]}`, and an agent with no line gets the unknown answer from `creds` and `mcp`; a record without `credentials` sends none (401 → `unauthorized`) |

```sh
cd src/lib/python && uv run python ../../../e2e/run.py --only-free --group connectors
# one runtime, directly
RUNTIME=rust uv run python ../../../e2e/tests/connectors/E95_mcp_client/probe.py
```
