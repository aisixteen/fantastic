> Add an interactive web panel — persist a `*.ts` view record to `web_loader`.

# Recipe: add an interactive panel

`web_loader` is a second `kernel_state` rooted at `.fantastic/web/` (its id
`web_loader`; created once — see `readme=daemon`). The browser `load_tree`s
it on boot and `persist_record`s changes back. These records are stored
OPAQUELY here (`handler_module` ending in `.ts` → inert on the host); the TS
app's page (the ts kernel booted by an app that registered the `*.ts` bundle) is what hydrates + renders them.
Reflect that frontend over the bridge to discover its compositor root and
view-agent kinds. The store holds ONE compositor root the frontend renders,
seeded once at setup — `load_tree` and look for it. Panels are its children.
(A child of a dir with no `agent.yaml` is unreachable, so the compositor root
record MUST exist before you parent to it — that's why setup seeds it.) Then
persist a frontend view record (a `.ts` `handler_module`) parented to that
root — e.g.:

    send web_loader {"type":"persist_record","record":{
      "id":"panel1", "handler_module":"<frontend-view>.ts", "parent_id":"<root_id>",
      "html":"<button id=run>Run</button> <pre id=out></pre><script>…</script>"
    }}

The `html` IS the panel BODY (a fragment — frontend state in the record). The
frontend compositor renders it as a sandboxed iframe automatically (no
mount), and INJECTS a `fantastic` connector the body uses to reach the JS
kernel — no import, no URL:

    let job = null;
    fantastic.watch("<exec_bridge_id>", (ev) => {          // live progress, host → here
      if (ev.job_id === job && ev.type === "progress") {
        out.textContent = ev.line;                            // updates in place
        fantastic.emit("panel2", {type:"v", value: ev.line}); // → another panel, by id
      }
    });
    run.onclick = async () => {
      const r = await fantastic.send("<exec_bridge_id>",   // a HOST exec_bridge you create here
        {type:"start", code:"import random;print(random.randint(0,9))"}); // NON-BLOCKING — returns a job_id
      job = r.job_id;                                         // output streams as `progress` events
    };

`exec_bridge` is an ASYNC job spawner: `start {command, args}` runs a command
from its `allowed` allowlist in the background (many in parallel) and returns
`{job_id}` at once; it streams
stdout/stderr as `progress` events + a final `job_done` (with the collected
output); use `status`/`stop` by job_id. There is no blocking "run-and-wait" —
watch the events or poll `status`.

Connector surface (mirrors the kernel): `send(target,payload)→reply`,
`emit(target,payload)` (fire-and-forget), `watch(src,cb)→unwatch`,
`onMessage(cb)` (messages sent to THIS panel's id). A receiver panel:
`fantastic.onMessage(p => out.textContent = p.value)`.
