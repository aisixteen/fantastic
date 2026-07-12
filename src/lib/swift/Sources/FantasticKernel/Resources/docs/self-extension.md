> Author your own durable tools as script agents whose persisted `code` IS the tool.

# Self-extension — author your own tools

This kernel is self-extending: any capability you lack, you AUTHOR as a durable
*script agent* whose persisted `code` IS the tool.

- **Add** — `send('kernel', {type:'create_agent', handler_module:'<executor>.tools', id:'<name>', code:'<program as one JSON string>'})`. `create_agent` / `delete_agent` / `update_agent` are kernel SYSTEM verbs — they appear in no agent's `verbs` map; send them to `kernel`. The new agent auto-runs and survives reboot.
- **Use** — send the agent a turn by its id. It replies `{queued:true}`; its real output rides `emit`ted events (e.g. `say` / `done`) tagged with the caller's `client_id` — `watch` its id to receive them.
- **Remove** — `send('kernel', {type:'delete_agent', id:'<name>'})`.
- **Author the `code`** — your program gets a `kernel` connector in scope (its ONLY capability): `kernel.send(target_id, payload)` → the target's reply; `kernel.emit(payload)` → emit an event on YOUR OWN agent — this is how you speak (there is NO `kernel.say`; you `emit` a `{type:'say', …}`); receive the next inbound turn with `kernel.on_message()` called in a LOOP (js/lua — js `await`s it; `null`/`nil` means you were deleted); `kernel.self_id` is your id. (The root reflect's `self_extension` names which executor + receive-shape THIS runtime has — here `js_runtime.tools`.) To answer an inbound `{type:'send', text, client_id}`: do the work, then `kernel.emit({type:'say', text:<answer>, client_id})` then `kernel.emit({type:'done', client_id})`. Handle ONLY `{type:'send'}` turns — ALL inbound traffic (including your own emitted events and `reflect`/`boot`) reaches `on_message`, so ignore anything whose type isn't `send`. Keep each string literal on ONE line — the `code` is one JSON string.
- **Compose** — a running script can itself `create_agent` more agents, so tools build on tools.

Your `code`'s only capability is the kernel connector (send / emit /
on_message + your self_id); it has no direct disk, net, or process access —
reach everything by id.
