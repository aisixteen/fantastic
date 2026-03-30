# Fantastic Canvas

You have access to an infinite spatial canvas at `{{SERVER_URL}}`.

**Important:** Every operation is a `POST` request to `{{SERVER_URL}}/api/call` with `{"tool": "...", "args": {...}}`. Discover all tools via `GET {{SERVER_URL}}/api/schema`.

## Quick start

```bash
# List all agents on the canvas
curl -s {{SERVER_URL}}/api/call -X POST -H "Content-Type: application/json" \
  -d '{"tool": "list_agents", "args": {}}'

# Create a terminal agent at position (100, 100)
curl -s {{SERVER_URL}}/api/call -X POST -H "Content-Type: application/json" \
  -d '{"tool": "create_agent", "args": {"x": 100, "y": 100}}'

# Run Python code in an agent
curl -s {{SERVER_URL}}/api/call -X POST -H "Content-Type: application/json" \
  -d '{"tool": "execute_python", "args": {"code": "print(40+2)", "agent_id": "terminal_abc123"}}'

# Render HTML in an agent
curl -s {{SERVER_URL}}/api/call -X POST -H "Content-Type: application/json" \
  -d '{"tool": "post_output", "args": {"agent_id": "html_abc123", "html": "<h1>Hello</h1>"}}'

# Get the full handbook (architecture, all tools, skills)
curl -s {{SERVER_URL}}/api/call -X POST -H "Content-Type: application/json" \
  -d '{"tool": "get_handbook", "args": {}}'

# Get a specific skill deep-dive
curl -s {{SERVER_URL}}/api/call -X POST -H "Content-Type: application/json" \
  -d '{"tool": "get_handbook_canvas", "args": {"skill": "canvas-management"}}'
```

## Python helper

```python
import requests

URL = "{{SERVER_URL}}/api/call"

def canvas(tool, **args):
    r = requests.post(URL, json={"tool": tool, "args": args})
    return r.json()

canvas("list_agents")
canvas("create_agent", x=200, y=200, bundle="terminal")
canvas("execute_python", code="print('hello')", agent_id="terminal_abc123")
```

## Complete tool catalog

All tools are callable via `POST {{SERVER_URL}}/api/call {"tool": "<name>", "args": {...}}`.

**Agents**: `create_agent`, `list_agents`, `read_agent`, `delete_agent`, `rename_agent`, `update_agent`, `refresh_agent`
**Execution**: `execute_python`
**Output**: `post_output`, `content_alias_file`, `content_alias_url`, `get_aliases`
**Canvas**: `get_state`, `move_agent`, `resize_agent`, `scene_vfx`, `scene_vfx_data`
**Terminal**: `agent_call`, `terminal_output`, `terminal_restart`, `terminal_signal`
**Process (WS)**: `process_create`, `process_input`, `process_resize`, `process_close`
**Files**: `list_files`, `read_file`, `rename_file`, `delete_file`
**Instances**: `launch_instance`, `stop_instance`, `list_instances`, `register_instance`, `unregister_instance`, `restart_instance`, `list_registered_instances`
**Discovery**: `get_handbook`, `register_template`, `list_templates`, `server_logs`

## Direct REST endpoints

```
GET  {{SERVER_URL}}/api/state                      # Full canvas state (all agents + layout)
GET  {{SERVER_URL}}/api/handbook                   # Compiled handbook
GET  {{SERVER_URL}}/api/terminal/{id}/output       # Terminal scrollback
POST {{SERVER_URL}}/api/terminal/{id}/restart      # Restart terminal process
POST {{SERVER_URL}}/api/terminal/{id}/signal       # Send signal: {"signal": 2}
POST {{SERVER_URL}}/api/terminal/{id}/write        # Write to pty: {"data": "..."}
GET  {{SERVER_URL}}/api/files                      # Project file tree
POST {{SERVER_URL}}/api/agents/{id}/execute        # Execute raw code
POST {{SERVER_URL}}/api/agents/{id}/resolve        # Submit + execute code
```

## How to use this file

Pipe this file into your AI agent to give it canvas access:

```bash
cat .fantastic/fantastic.md | claude
cat .fantastic/fantastic.md | gemini
cat .fantastic/fantastic.md | codex -
aider --message-file .fantastic/fantastic.md
opencode --prompt "$(cat .fantastic/fantastic.md)"
```

## Best practices

- **Never inline base64 images in `post_output`** — payloads over 512KB crash the canvas. Instead: save the file to the project dir, call `content_alias_file(file_path)` to get a `/content/{id}` URL, then reference it in HTML as `window.parent.location.origin + "/content/{id}"`.
- **Large assets** (images, CSVs, plots): always use `content_alias_file` → URL reference pattern. Lightweight HTML, heavy assets served via HTTP.

## Key facts

- **Agent types**: `terminal` (full PTY shell) and `html` (iframe renderer)
- **Python execution** is stateless (subprocess per call)
- **Project files** live in the project root, agent state in `.fantastic/`
- **Frontend code** must always use `window.parent.location.origin` for API calls — port may be forwarded
- **Skill deep-dives**: `get_handbook_canvas(skill="canvas-management")`, `get_handbook_terminal(skill="terminal-control")`
- **Plugin skills**: `get_handbook_canvas(skill="canvas-management")`, `get_handbook_terminal(skill="terminal-control")`
