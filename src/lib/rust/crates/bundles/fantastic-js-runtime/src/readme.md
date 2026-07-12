# js_runtime — a general-purpose JavaScript executor (an authored agent's body)

NOT agentic. This bundle knows nothing about AI, loops, or control flow. It
gives caller-authored JavaScript a **persistent, addressable, bootable home**
— the same contract as `lua_runtime`, on a QuickJS engine (bundled, no
system deps):

- the agent record's `code` meta IS the program — it persists like any record,
  and `boot` re-starts it after a reboot (a durable, living script agent);
- the script's ONLY capability is the **kernel connector**:
  `await kernel.send(target, payload)` → reply object,
  `await kernel.emit(payload)`, `await kernel.on_message()` → next inbox
  payload (or `null` when the agent is deleted), `kernel.self_id`. No fs, no
  net, no process — sandbox by omission; every effect goes through `send`
  and is governed by agent rules;
- any verb this bundle doesn't own is **queued to the script's inbox**
  (`{queued:true}` ack) and consumed by `await kernel.on_message()` — an
  authored agent receives turns exactly like any other agent. Its real
  output rides `kernel.emit` events (tag them with the caller's `client_id`).

Authored code runs inside an async context, so top-level `await` works.

## Make a durable script agent

    create_agent handler_module=js_runtime.tools id=<name> code=<js source>
    send <name> {"type":"boot"}          # or it boots with the kernel

`start {code}` runs code EPHEMERALLY (not persisted); put `code` on the
record (create_agent / update_agent) for durability. `stop` aborts;
`status` → `{running, has_code}`.

## What authored code can be

Whatever the author needs: a **control harness** of any shape (a state
machine, event wiring, scheduled ticks, a feedback loop, something unnamed) —
e.g. one that composes prompts, `send`s a bare completion organ
(`ollama_backend` / `nvidia_nim_backend` / `anthropic_backend`), interprets
the completion and acts; or a tool; or a router. The engine directs nothing.

A safety guard rides the connector: every `kernel.send` carries a
`_call_stack` (own id appended); a call chain that revisits an agent or
exceeds depth 8 is refused with an error reply.

**Meta-possibility:** a script can `kernel.send('kernel',
{type:'create_agent', …})` — including MORE script agents. Tools can author
tools; harnesses can author their successors and hot-swap.

## Author-code contract — read before writing `code`

1. **`on_message` is a PULL, not a callback.** `await kernel.on_message()` takes
   NO arguments and RETURNS the next inbox payload (`null` when you are being
   deleted). Never pass it a function. LOOP to keep receiving turns:

       // event-loop
       while (true) {
         const m = await kernel.on_message();   // waits for the next turn
         if (m === null) break;                 // deleted -> exit
         if (m.type !== 'send') continue;
         // ... act, then emit say + done ...
       }

2. **`code` is ONE JSON string.** Separate statements with `\n` (a JSON `\n`
   decodes to a real newline) or `;`. NEVER put a raw line break inside a STRING
   LITERAL's content — keep each string on one line (use a space, not a newline,
   even in a `system_prompt`); if you truly need an in-string newline write it as
   `\\n`. Escape as needed.
3. **Every turn MUST emit both:** a `say` carrying the answer, THEN a `done` —
   both tagged with the caller's `client_id`. Never finish a turn without the
   `done`: `kernel.emit({type:'say', text:..., client_id:m.client_id})` then
   `kernel.emit({type:'done', client_id:m.client_id})`.
4. **Remove your own tool** with `kernel.send('kernel', {type:'delete_agent',
   id:kernel.self_id})` (or any id) — the same system verb that created it.
