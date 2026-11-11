> `fantastic mcp` (a coding CLI's MCP entry point) and `fantastic send` (one call from a shell).

# `fantastic mcp` and `fantastic send`

## `fantastic mcp`

THE PROJECT'S MCP CONFIG is written for you, INSIDE the state folder — nothing is
created outside it: at every start the `mcp` leg sets `mcpServers.fantastic =
{command: <this fantastic binary>, args: [mcp]}` in `.mcp.json`
in its bridge's folder (`<state>/agents/user/agents/mcp_config/`, through the `mcp_config` bridge), keeping every other server, and only
when it changed. Point a coding CLI at it: `claude --mcp-config
<that file>` (`<state>/readme.md` names the path).

MCP over stdio, protocol 2025-11-25, two tools: `reflect` and `send`. No
arguments but `--statedir DIR` and `--workdir DIR` (the state folder, default
`<workdir>/.fantastic`, and the work folder, default the current dir). Register
it once as a stdio server — command `fantastic`, args `["mcp"]` — and start it in
the project directory (or add `--statedir <state>`):

- a kernel already runs on that state folder (the terminal UI, the canvas,
  `--headless`, another `fantastic mcp`) → it ATTACHES over a loopback port
  named, with its token, in `<state>/cli/mcp.yaml` (a `--workdir` given now is
  refused: it cannot change a running kernel);
- none → it RUNS the kernel itself and opens that channel for others (a recorded `web` host listens too, as in every mode).

The client is the `mcp` agent (a child of the root, created on first
connection), with god-mode grants on every agent; the human reshapes them with
`update_agent` on its record.

- `reflect` with no arguments: the root's primer — tree, bundles, the readme
  index (`reflect` + `readme=<key>` drills in).
- `send {items: [{target, payload: {type: <verb>, …}, timeout?, secret?}]}` →
  `[{pid, status, result}]`. `secret` marks (`payload.<field>`, or a list of
  steps) keep a value out of every row, ledger, reflect and stream.
- A tool call is a kernel task owned by the leg (`ps` lists it); MCP's
  `notifications/cancelled` cancels it; an item `timeout` cancels that item.

## `fantastic send [--statedir DIR] '<item json>'`

One call into the kernel that holds the state folder (`--statedir DIR`, default
`<current dir>/.fantastic`), as the `mcp` leg, over the same channel. Attach-only: no live kernel → it fails, exit 1, and starts
nothing.

- A payload string that is exactly `$env:NAME` (any depth) becomes this
  shell's `NAME`, and its path is marked secret on the item. This is the only
  place `$env:` is resolved. Unset → exit 2, nothing sent.
- Prints `{pid, status, result}`; exit 0 when the item completed, else 1.

```sh
fantastic send '{"target":"kernel","payload":{"type":"ps"}}'
fantastic send --statedir /srv/proj/state '{"target":"kernel","payload":{"type":"ps"}}'
KEY=$(op read op://vault/openai/key) fantastic send \
  '{"target":"credentials","payload":{"type":"set","ref":"responses/api.openai.com","fields":{"api_key":"$env:KEY"}}}'
```

Storing a key is in `fantastic readme setup`.
