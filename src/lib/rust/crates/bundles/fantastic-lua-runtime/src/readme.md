# lua_runtime — a general-purpose Lua executor (an authored agent's body)

NOT agentic. This bundle knows nothing about AI, loops, or control flow. It
gives caller-authored Lua code a **persistent, addressable, bootable home**:

- the agent record's `code` meta IS the program — it persists like any record,
  and `boot` re-starts it after a reboot (a durable, living script agent);
- the script's ONLY capability is the **kernel connector**:
  `kernel.send(target, payload)` → reply (suspends the script),
  `kernel.emit(payload)`, `kernel.on_message()` → next inbox payload
  (suspends), `kernel.self_id`, plus `kernel.json_decode(str)` /
  `kernel.json_encode(v)` (Lua ships no JSON). No `io`, no `os`, no
  filesystem — sandbox by omission; every effect goes through `send` and is
  governed by agent rules;
- any verb this bundle doesn't own is **queued to the script's inbox**
  (`{queued:true}` ack) and consumed by `kernel.on_message()` — an authored
  agent receives turns exactly like any other agent. Its real output rides
  `kernel.emit` events (tag them with the caller's `client_id`).

## Make a durable script agent

    create_agent handler_module=lua_runtime.tools id=<name> code=<lua source>
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
