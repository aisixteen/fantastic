# E64 · mcp_config — the project's `.mcp.json`, kept by the `mcp` leg

Scripted, no model (`costs: free`).

**PASS** when every check holds:

| check | what |
|---|---|
| entry | after a boot, `mcpServers.fantastic` is `{command: <the fantastic binary, absolute>, args: ["mcp"]}` |
| other_kept | a server already in the file, its fields and key order, and any other key are untouched |
| unchanged_on_reboot | a second boot leaves the file byte-identical |
| no_error | the leg's reflect carries no `mcp_config_error` |
| bridge_only_file | the `mcp_config` bridge is writable, rooted at the project, exposing only `.mcp.json` |
