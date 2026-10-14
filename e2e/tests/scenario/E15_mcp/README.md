# `mcp` — `fantastic mcp`, the stdio leg, scripted end to end

The product binary's MCP face, driven by a **scripted MCP client**. No model,
and no Claude Code: a human plugs the binary into their own client with
`claude mcp add`, but a test never automates Claude Code (TOS).

```sh
# free: no BACKEND needed
cd src/lib/python && uv run python ../../../e2e/run.py --only-free --id mcp
# or directly, against any binary
uv run python ../../../e2e/runners/mcp.py mcp --bin /path/to/fantastic
```

## What it proves

| run | proves |
|---|---|
| `create` | a fresh dir inits from the blueprint; `initialize` answers **2025-11-25**; `tools/list` is exactly `reflect`, `send`; the zero-arg reflect is the kernel primer; a real `read_stream` through `files`; the gate refuses `delete_agent` as `isError`; a shape error names `payload.type`; an unknown method is `-32601`; on EOF the process exits 0, the lock is released, and the leg's record, the merged `callable_by` and the memory store `memory.db` are on disk — its rows hold the `read_stream` call (caller `mcp`) and its settle — and no `messages.yaml`/`ledger.json` |
| `rehydrate` | the same dir, no flags: binds to the existing leg (one record, not two), reads again, and the store's rows GREW in the same `memory.db` |
| `refuse-flags` | a creation flag on an existing leg exits 1 naming `update_agent`, and still releases the lock |

## Not here, and where it is

The **bound** (a call that outlasts `settle_bound_ms` answers `{status:"running",
pid}` and keeps going) and **cancellation** need a verb that blocks. No shipped
agent has one, and a test-only bundle does not belong in the product binary.
Both are pinned in the `fantastic-web-mcp` crate's own integration test, against
a real kernel with a sleeping test bundle.

## The binary

`--bin <path>` (or `FANTASTIC_BIN`) runs any `fantastic`. Without it the runner
builds `src/tui` and uses `src/target/debug/fantastic`, the thing under test.
