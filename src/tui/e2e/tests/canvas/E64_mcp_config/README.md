# E64 · mcp_config — the `mcp_config` bridge's `.mcp.json` (`<state>/agents/user/agents/mcp_config/`), kept by the `mcp` leg

Scripted, no model (`costs: free`).

**PASS** when every check holds:

| check | what |
|---|---|
| entry | after a boot, `mcpServers.fantastic` is `{command: <the fantastic binary, absolute>, args: ["mcp"]}` |
| other_kept | a server already in the file, its fields and key order, and any other key are untouched |
| unchanged_on_reboot | a second boot leaves the file byte-identical |
| no_error | the leg's reflect carries no `mcp_config_error` |
| bridge_only_file | the `mcp_config` bridge is writable, rooted at its own folder, exposing only `.mcp.json` |
| nothing_outside_fantastic | the project holds nothing the CLI made but `.fantastic/` |
