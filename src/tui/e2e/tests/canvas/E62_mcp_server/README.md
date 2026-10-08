# E62 · mcp_server — an MCP server added from the canvas

Scripted, no model (`costs: free`). The server is the lib's fake (E95,
protocol 2026-07-28), which refuses any request without the key.

**PASS** when every check holds:

| check | what |
|---|---|
| form | the double-click's `mcp connector` tile places the add form |
| agent_under_canvas · agent_names_ref | `add_mcp` makes an `mcp_client.tools` agent under `canvas_root`, naming its key's ref (`mcp_client/<host>`) |
| key_on_no_record · key_in_home | the key is on no project record; it is in the user's credentials file |
| view · view_url · view_server · view_tools · form_gone | the form gives way to the agent's view: its url, the server's name, its tools (not the agent's own verbs) |
| relisted | ⟳ asks the server for its tools again |
| tool_answers · bearer_every_time | a tool answers through the agent; every request carried `Authorization: Bearer <key>` |
