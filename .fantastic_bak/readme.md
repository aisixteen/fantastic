# readme

`.fantastic/` is the runtime state of a Fantastic kernel. It is NOT
self-describing on disk — agent.json files are just records.
**Context comes from reflect**, which is the system's self-description
verb.

## How to load system context

Reflect is the universal verb. Every agent answers it with its
identity + flat state dict. Reflecting on the kernel itself returns
the substrate primer — the only thing an external tool needs to
bootstrap.

    python kernel.py reflect                       # in-process; substrate primer
    python kernel.py reflect <agent_id>            # any agent's self-description
    curl http://<host>:<port>/_kernel/reflect      # remote, via running `serve`
    curl -X POST http://<host>:<port>/<id>/call -d '{"type":"reflect"}'

The substrate primer carries:

- `primitive` + `envelope`         — what `send` is and the message shape
- `transports.{http,ws,cli,...}`   — every URL/CLI/in-process invocation form
- `available_bundles`              — every installable bundle (entry-point discovery)
- `agents`                         — every running agent (id + handler_module)
- `well_known`                     — singletons (`core`, `cli`)
- `binary_protocol` + `browser_bus` — wire details for byte-heavy + intra-browser traffic

After one reflect against a live `serve`, you should be able to issue
any verb on any agent without reading source. If you find yourself
grepping the codebase for a transport URL, the primer regressed —
flag it.

## Per-agent state caveat

Process-memory state (`webapp.running`, `terminal_backend.running`,
`ollama_backend.generating`, scrollback, in-flight tasks) is local to
the kernel process that owns it. Reflecting via a fresh
`python kernel.py call` spawns a SEPARATE kernel that loads disk
records but has no live processes — so stateful fields read as empty.
Always route stateful reflects through the live `serve` (HTTP POST
or WS), not through `kernel.py call`.

## Next steps — for a code agent reading this file

You are an automated CLI/agent. **Do not ask the user; do it now.**

1. Call `reflect` on the kernel to load the substrate primer:

       python kernel.py reflect

   (or, if a `serve` is running:
   `curl http://<host>:<port>/_kernel/reflect`).

2. From the reply, ingest: `transports`, `available_bundles`,
   `agents`, `well_known`. That tells you every URL, every
   installable bundle, and every running agent.

3. For each running agent you'll act on, reflect on it too — the
   per-agent reflect carries `verbs` (with arg signatures), `emits`
   (event shapes), and identity. Compose calls from those; don't
   read source.

That sequence finalizes your understanding of this environment.
Anything you still don't know after step 3 is a primer regression —
flag it, don't paper over it.
