> Add an html agent — ask the frontend agent that owns that part of the tree.

# Recipe: add an html agent

An **html agent** is the general-purpose web UI: a FRONTEND agent (its
`handler_module` ends in `.ts`) whose record carries its `html` body. It runs in
the browser's ts kernel, not here, so it is created THERE, by its parent:

1. Reflect the frontend over the bridge (its kernel is a peer — see
   `reflect kernel` → `peers`, then reflect `<guid>|kernel`) to find the view
   that renders html agents and the verbs it answers.
2. Ask THAT view to create it — its own verbs (e.g. a place/create verb with the
   `html` body); the view writes the frontend record.

The frontend's loader persists the new agent to the host store (`web_loader`)
by itself — storage follows the tree, it is not how an agent is made.

The `html` IS the body (a fragment — frontend state in the record). The view
renders it as a sandboxed iframe (no mount) and INJECTS a `fantastic` connector
the body uses to reach the JS kernel — no import, no URL:

    let job = null;
    fantastic.watch("<exec_bridge_id>", (ev) => {          // live progress, host → here
      if (ev.job_id === job && ev.type === "progress") {
        out.textContent = ev.line;                            // updates in place
        fantastic.emit("<other_html_agent>", {type:"v", value: ev.line}); // → another agent, by id
      }
    });
    run.onclick = async () => {
      const r = await fantastic.send("<exec_bridge_id>",   // a HOST exec_bridge you create here
        {type:"start", command:"<allowed command>", args:[]}); // NON-BLOCKING — returns a job_id
      job = r.job_id;                                         // output streams as `progress` events
    };

`exec_bridge` is an ASYNC job spawner: `start {command, args}` runs a command
from its `allowed` allowlist in the background (many in parallel) and returns
`{job_id}` at once; it streams
stdout/stderr as `progress` events + a final `job_done` (exit code and line
count); read its output with `tail`/`grep`, `status`/`stop` by job_id. There is
no blocking "run-and-wait" — watch the events or poll `status`.

Connector surface (mirrors the kernel): `send(target,payload)→reply`,
`emit(target,payload)` (fire-and-forget), `watch(src,cb)→unwatch`,
`onMessage(cb)` (messages sent to THIS agent's id). A receiving html agent:
`fantastic.onMessage(p => out.textContent = p.value)`.
