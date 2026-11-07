# E93 `reflect_view` — a renderer is resolved from `reflect`

#483: `reflect` carries every agent's `view` in every kernel, and the canvas resolves
renderers from it, not from the `list_agents` record.

1. a host bundle agent (`ram_store.tools`, a handler module, so its reflect is the
   bundle's own) is made with `view {kind: html, bytes}`; its `reflect` carries that
   `view`;
2. `place {backend_id}` draws it as an `agent_view` cell, and the cell shows the body;
3. `update_agent` changes the `view`: the canvas re-reflects the agent and the cell
   shows the new body;
4. no page error.
