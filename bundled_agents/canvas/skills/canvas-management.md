# Canvas Management

Agent lifecycle, content aliases, and visual effects.

## Getting Started

```bash
fantastic add canvas        # creates canvas agent, marks bundle as added
fantastic                   # starts server with canvas loaded
```

## Agent Types

| Type | Description | Terminal? | Default Size |
|------|-------------|-----------|-------------|
| `terminal` | Plain shell | Yes | 600x350 |
| `html` | HTML content or URL in iframe | No | 800x600 |

## Agent CRUD (Tools)

### `create_agent(x, y, template, url, html_content, options)`
Create agent on canvas. `template` sets the type (`"terminal"` or `"html"`). Returns `{agent_id, bundle, x, y, width, height}`.

### `read_agent(agent_id)`
Get full agent state: source, output_html, position.

### `delete_agent(agent_id)`
Delete agent. Cleans up terminal. Respects `delete_lock` property.

### `move_agent(agent_id, x, y)` / `resize_agent(agent_id, width, height)`
Reposition or resize an agent on the canvas.

### `rename_agent(agent_id, display_name)`
Set display name in agent header. Empty string resets to default.

### `update_agent(agent_id, options)`
Bulk property update (e.g. `{"autostart": true, "delete_lock": true}`).

### `refresh_agent(agent_id)`
Restart terminal or reload iframe. Broadcasts process lifecycle events.

## Code Execution

### `execute_python(code, agent_id)`
Execute Python via subprocess. Stateless — each call is independent. `agent_id` is required.

**Subprocess cwd = project directory.** Always use relative paths in code: `open("notebooks/config.yaml")`, not `os.path.expanduser("~/Projects/.../config.yaml")`. Relative paths work locally and in Docker containers.

## Content Aliases

### `content_alias_file(file_path, persistent=True)` → `/content/{id}`
Serve a local file via HTTP. Auto-detects MIME type.

### `content_alias_url(url, persistent=True)` → `/content/{id}`
Create a redirect alias for an external URL.

Aliases are persisted to `.fantastic/aliases.json`. Use the returned path in HTML `<img>`, `<script>`, `<link>` tags. By default `persistent=True` — aliases survive server restarts. Pass `persistent=False` for temporary aliases that are cleaned up on reload.

## Scene VFX

### `scene_vfx(js_code)`
Set canvas scene VFX (THREE.js). The JS receives `scene`, `THREE`, `camera`, `renderer`, `clock`. Use `this.onFrame = (delta, elapsed) => { ... }` for animation loops. Return a cleanup function to dispose resources. Stored in the canvas agent's `scene_vfx.js`.

```python
scene_vfx("""
const geo = new THREE.TorusKnotGeometry(20, 6, 64, 16)
const mat = new THREE.MeshStandardMaterial({ color: '#ff4488', wireframe: true })
const mesh = new THREE.Mesh(geo, mat)
mesh.position.set(0, 100, 0)
scene.add(mesh)
this.onFrame = (dt, t) => { mesh.rotation.y += 0.01 }
return () => { scene.remove(mesh); geo.dispose(); mat.dispose() }
""")
```

### `scene_vfx_data(data)`
Push live data to the VFX animation loop. Available in VFX code as `window.__vfxData`. Call at 10-30fps from a music/audio panel to drive reactive visuals.

```python
# From an HTML agent (e.g. music panel with Web Audio API):
scene_vfx_data({"bass": 0.8, "mid": 0.3, "treble": 0.1, "bpm": 120})
```

```python
# VFX code that reads the live data:
scene_vfx("""
var d = window.__vfxData || {};
var bass = d.bass || 0;
var geo = new THREE.SphereGeometry(50 + bass * 200, 32, 32);
var mat = new THREE.MeshStandardMaterial({ color: '#7c83ff', wireframe: true, transparent: true, opacity: 0.2 + bass * 0.6 });
var mesh = new THREE.Mesh(geo, mat);
scene.add(mesh);
this.onFrame = (dt, t) => {
  var b = (window.__vfxData || {}).bass || 0;
  mesh.scale.setScalar(1 + b * 2);
  mesh.rotation.y += 0.01;
};
return () => { scene.remove(mesh); geo.dispose(); mat.dispose(); };
""")
```

## Canvas State

### `get_canvas_state()`
Returns full state: all agents with positions, sizes, types, sources, outputs.

### `list_agents()`
Returns agent list with id, display_name, bundle, position, source.

## Persistent State

```
.fantastic/
  fantastic.md
  config.json                # Server config (port, PID)
  registry.json              # Server registry
  aliases.json               # Content alias registry
  instances.json             # Instance tracking
  agents/
    {canvas_agent_id}/       # Canvas (real agent, bundle="canvas")
      agent.json             # {id, bundle: "canvas", ...}
      layout.json            # {agent_id: {x, y, width, height}}
      canvasbg.js            # Background VFX
    {agent_id}/              # Per agent
      agent.json             # identity, type, metadata
      source.py              # last executed code
      output.html            # HTML output
      terminal.log           # scrollback (terminal-type only)
```

## Agent Storage Policy — IMPORTANT

> **Agents are ephemeral runners, not code containers.** All source code MUST live in the project directory, never inside `.fantastic/`. If you are unsure where to put something — ask the user.

**Rules:**
1. **Code lives in the project** — write .py files to project dirs (e.g. `steps/`, `src/`, `notebooks/`)
2. **Agents reference external files** — use `execute_python("exec(open('steps/01_load.py').read())", agent_id)` to run them
3. **Never pass large code blocks to `execute_python`** — always read from project files
4. **`.fantastic/` is metadata only** — `agent.json` (config), `output.html` (ephemeral render), nothing else matters
5. **`source.py` in `.fantastic/` is an auto-generated log** — NOT source of truth, NOT for editing
6. **When in doubt, ask the user** — if you're unsure whether code should go in the project or an agent, ask first

The pattern: **project owns code, agents own execution and output.**

## File Operations

### `list_files()` / `read_file(path)`
List project files as tree or read a specific file. Excludes `.fantastic/`, `.git/`, `node_modules/`, etc.

### `rename_file(old_path, new_path)` / `delete_file(path)`
Rename/move or delete a project file. Broadcasts `file_renamed` / `file_deleted`.

## External Execution

For external agent integration:
1. Create an agent
2. Submit code via `POST /api/agents/{id}/resolve`
3. Result broadcast to frontend
