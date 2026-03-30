"""
Core engine — orchestrates agent state via AgentStore.

Each agent gets its own directory under `.fantastic/agents/`. Python code is executed
via subprocess (stateless, one-shot). Server registry is persisted to disk.
"""

import hashlib
import html as html_mod
import json
import logging
import os
import re
import time
from pathlib import Path
from typing import Any, Callable, Awaitable

from .agent_store import AgentStore
from .code_runner import CodeRunner

logger = logging.getLogger(__name__)

# Directories/files to exclude from project file listing
_EXCLUDED_DIRS = {
    "__pycache__", ".git", ".hg", ".svn", "node_modules", ".mypy_cache",
    ".pytest_cache", ".tox", ".eggs", "*.egg-info", ".venv", "venv",
    "env", ".env", "dist", "build", ".next", ".nuxt", ".fantastic",
}


class Engine:
    def __init__(
        self,
        project_dir: str | None = None,
        broadcast: Callable[[dict[str, Any]], Awaitable[None]] | None = None,
    ):
        self._project_dir = Path(project_dir or os.getenv("PROJECT_DIR", os.getcwd()))
        self._broadcast = broadcast

        # Agent store (persistent .fantastic/ directory)
        self._store = AgentStore(self._project_dir)
        self._store.init()

        # Code runner (subprocess-based, stateless)
        self._runner = CodeRunner(project_dir=str(self._project_dir))

        # Content alias registry (lazy-loaded from .fantastic/aliases.json)
        self._content_aliases: dict[str, dict] | None = None

        # State enrichment hooks (plugins add custom fields)
        self._state_hooks: list[Callable[[dict], None]] = []

    @property
    def store(self) -> AgentStore:
        return self._store

    @property
    def runner(self) -> CodeRunner:
        return self._runner

    @property
    def project_dir(self) -> Path:
        return self._project_dir

    async def start(self) -> None:
        """Start the engine. Store is already initialized in __init__."""
        logger.info(f"Engine started (project_dir={self._project_dir})")

    async def stop(self) -> None:
        """Stop all running processes."""
        await self._runner.stop_all()
        logger.info("Engine stopped")

    def set_broadcast(self, broadcast: Callable[[dict[str, Any]], Awaitable[None]]) -> None:
        self._broadcast = broadcast

    def resolve_working_dir(self, agent_id: str) -> Path:
        """Walk up to root parent, return its working_dir or project_dir."""
        root = self._store.get_root_parent(agent_id)
        if root and root.get("working_dir"):
            wd = Path(root["working_dir"])
            if not wd.is_absolute():
                wd = self._project_dir / wd
            return wd
        return self._project_dir

    # ─── Agent CRUD ───────────────────────────────────────────

    def create_agent(
        self,
        agent_id: str | None = None,
        bundle: str | None = None,
        parent: str | None = None,
        url: str | None = None,
        html_content: str | None = None,
        author_type: int = 0,
        created_by: str | None = None,
    ) -> dict[str, Any]:
        """Create a new agent and return its full state dict."""
        agent = self._store.create_agent(
            agent_id=agent_id,
            bundle=bundle,
            parent=parent,
            author_type=author_type,
            created_by=created_by,
        )

        # HTML: store URL and/or content in agent metadata
        if url:
            iframe_html = (
                f'<iframe src="{url}" style="width:100%;height:100%;border:none" '
                f'sandbox="allow-scripts allow-same-origin allow-forms allow-popups" '
                f'allowTransparency="true"></iframe>'
            )
            self._store.update_agent_meta(agent["id"], url=url, html_content=iframe_html)
            agent["url"] = url
            agent["html_content"] = iframe_html
        if html_content:
            self._store.update_agent_meta(agent["id"], html_content=html_content)
            agent["html_content"] = html_content

        return agent

    def get_agent(self, agent_id: str) -> dict[str, Any] | None:
        return self._store.get_agent(agent_id)

    def delete_agent(self, agent_id: str) -> bool:
        return self._store.delete_agent(agent_id)

    @staticmethod
    def _render_outputs_html(outputs: list[dict[str, Any]]) -> str:
        """Auto-convert code execution outputs to HTML."""
        parts: list[str] = []
        for out in outputs:
            otype = out.get("output_type")
            if otype == "stream":
                text = out.get("text", "")
                parts.append(
                    f'<pre style="margin:0;white-space:pre-wrap">'
                    f'{html_mod.escape(text)}</pre>'
                )
            elif otype == "error":
                # Strip ANSI escape codes from traceback
                tb = "\n".join(out.get("traceback", []))
                tb = re.sub(r'\x1b\[[0-9;]*m', '', tb)
                parts.append(
                    f'<pre style="margin:0;color:#f87171;white-space:pre-wrap">'
                    f'{html_mod.escape(tb)}</pre>'
                )
        return "\n".join(parts)

    async def execute_code(
        self, agent_id: str, code: str, author_type: int = 0, triggered_by: str | None = None,
    ) -> dict[str, Any]:
        """Execute code via subprocess."""
        agent = self._store.get_agent(agent_id)
        if agent is None:
            raise ValueError(f"Agent {agent_id} not found")

        self._store.set_source(agent_id, code)

        wd = self.resolve_working_dir(agent_id)
        t0 = time.monotonic()
        result = await self._runner.execute(agent_id, code, cwd=str(wd))
        duration_ms = int((time.monotonic() - t0) * 1000)

        # Auto-render code outputs as HTML and push to agent
        output_html = self._render_outputs_html(result["outputs"])
        if output_html:
            await self.post_output(agent_id, output_html)

        # Append to agent long-term memory
        try:
            await self._store.append_memory(agent_id, author_type, {
                "kind": "execution",
                "source_hash": hashlib.sha256(code.encode()).hexdigest(),
                "source_snippet": code[:500],
                "exit_code": 0 if result.get("success") else 1,
                "duration_ms": duration_ms,
                "output_size": len(output_html) if output_html else 0,
                "triggered_by": triggered_by,
            })
        except Exception:
            logger.warning(f"Failed to append memory for agent {agent_id}", exc_info=True)

        return result

    async def resolve_agent(self, agent_id: str, code: str) -> dict[str, Any]:
        """External caller resolves an agent: provides code and executes."""
        agent = self._store.get_agent(agent_id)
        if agent is None:
            raise ValueError(f"Agent {agent_id} not found")

        result = await self.execute_code(agent_id, code)
        return {
            "agent_id": agent_id,
            "code": code,
            "outputs": result["outputs"],
            "success": result["success"],
        }

    def list_children(self, parent_id: str) -> list[dict[str, Any]]:
        """Return agents whose parent == parent_id."""
        return self._store.list_children(parent_id)

    def update_agent_meta(self, agent_id: str, **kwargs: Any) -> bool:
        try:
            self._store.update_agent_meta(agent_id, **kwargs)
            return True
        except ValueError:
            return False

    def on_enrich_state(self, fn: Callable[[dict], None]) -> None:
        """Register a hook to enrich get_state() response (e.g. add scene_vfx_js)."""
        self._state_hooks.append(fn)

    def get_state(self) -> dict[str, Any]:
        """Return full state for frontend."""
        agents = self._store.list_agents()
        state: dict[str, Any] = {"agents": agents}
        for hook in self._state_hooks:
            hook(state)
        return state

    async def post_output(self, agent_id: str, html: str) -> None:
        """Write output.html and broadcast to frontend."""
        self._store.set_output(agent_id, html)
        agent = self._store.get_agent(agent_id)
        has_iframe = agent and (agent.get("has_iframe") or agent.get("html_content"))
        # Persist html_content so iframe re-renders
        if has_iframe:
            self._store.update_agent_meta(agent_id, html_content=html)
        if self._broadcast:
            await self._broadcast({
                "type": "agent_output",
                "agent_id": agent_id,
                "output_html": html,
            })
            # Force iframe remount for iframe agents
            if has_iframe:
                await self._broadcast({
                    "type": "agent_refresh",
                    "agent_id": agent_id,
                })

    # ─── Content aliases ─────────────────────────────────────

    @property
    def _aliases_path(self) -> Path:
        return self._project_dir / ".fantastic" / "aliases.json"

    def _load_aliases(self) -> dict[str, dict]:
        if self._aliases_path.exists():
            data = json.loads(self._aliases_path.read_text())
            # Only keep persistent aliases across restarts
            return {k: v for k, v in data.items() if v.get("persistent")}
        return {}

    def _save_aliases(self) -> None:
        self._aliases_path.parent.mkdir(parents=True, exist_ok=True)
        self._aliases_path.write_text(json.dumps(self._content_aliases))

    @property
    def content_aliases(self) -> dict[str, dict]:
        if self._content_aliases is None:
            self._content_aliases = self._load_aliases()
        return self._content_aliases

    def add_content_alias(self, alias_id: str, entry: dict) -> None:
        self.content_aliases[alias_id] = entry
        self._save_aliases()

    def remove_content_alias(self, alias_id: str) -> bool:
        if alias_id in self.content_aliases:
            del self._content_aliases[alias_id]
            self._save_aliases()
            return True
        return False

    # ─── File operations ───────────────────────────────────────────

    def list_files(self) -> list[dict[str, Any]]:
        """Recursively list project files as a tree structure."""
        return self._walk_dir(self._project_dir)

    def _walk_dir(self, dirpath: Path) -> list[dict[str, Any]]:
        entries = []
        try:
            items = sorted(dirpath.iterdir(), key=lambda p: (not p.is_dir(), p.name.lower()))
        except PermissionError:
            return entries
        for item in items:
            if item.name.startswith(".") and item.name in {".git", ".hg", ".svn", ".env", ".fantastic"}:
                continue
            if item.name in _EXCLUDED_DIRS:
                continue
            if item.is_dir():
                children = self._walk_dir(item)
                entries.append({
                    "name": item.name,
                    "path": str(item.relative_to(self._project_dir)),
                    "isDir": True,
                    "children": children,
                })
            else:
                entries.append({
                    "name": item.name,
                    "path": str(item.relative_to(self._project_dir)),
                    "isDir": False,
                })
        return entries

    def rename_file(self, old_path: str, new_path: str) -> None:
        """Rename/move a file within project_dir."""
        old = self._project_dir / old_path
        new = self._project_dir / new_path
        if not old.exists():
            raise ValueError(f"File not found: {old_path}")
        old.resolve().relative_to(self._project_dir.resolve())
        new.resolve().relative_to(self._project_dir.resolve())
        new.parent.mkdir(parents=True, exist_ok=True)
        old.rename(new)

    def delete_file(self, path: str) -> None:
        """Delete a file within project_dir."""
        target = self._project_dir / path
        if not target.exists():
            raise ValueError(f"File not found: {path}")
        target.resolve().relative_to(self._project_dir.resolve())
        target.unlink()

    def read_file(self, path: str) -> dict[str, Any]:
        """Read a file and return its content. For images, returns base64 data."""
        import base64
        target = self._project_dir / path
        if not target.exists():
            raise ValueError(f"File not found: {path}")
        target.resolve().relative_to(self._project_dir.resolve())

        ext = target.suffix.lower()
        image_exts = {'.png', '.jpg', '.jpeg', '.gif', '.bmp', '.webp', '.svg', '.ico'}

        if ext in image_exts:
            with open(target, "rb") as f:
                data = base64.b64encode(f.read()).decode("ascii")
            mime = {
                '.png': 'image/png', '.jpg': 'image/jpeg', '.jpeg': 'image/jpeg',
                '.gif': 'image/gif', '.bmp': 'image/bmp', '.webp': 'image/webp',
                '.svg': 'image/svg+xml', '.ico': 'image/x-icon',
            }.get(ext, 'application/octet-stream')
            return {"kind": "image", "data": data, "mime": mime}
        else:
            try:
                with open(target, "r", encoding="utf-8") as f:
                    content = f.read()
                return {"kind": "text", "content": content}
            except UnicodeDecodeError:
                return {"kind": "binary", "content": "(binary file — cannot display)"}

    # ─── Server registry (delegated to store) ─────────────

    def register_server(
        self,
        agent_id: str,
        url: str,
        name: str = "",
        tools: list[str] | None = None,
    ) -> dict[str, Any]:
        registry = self._store.get_registry()
        entry = {
            "agent_id": agent_id,
            "url": url,
            "name": name or f"agent-{agent_id}",
            "tools": tools or [],
            "registered_at": time.time(),
        }
        registry[agent_id] = entry
        self._store.set_registry(registry)
        logger.info(f"Server registered: {entry['name']} at {url} (agent {agent_id})")
        return entry

    def unregister_server(self, agent_id: str) -> bool:
        registry = self._store.get_registry()
        removed = registry.pop(agent_id, None)
        if removed:
            self._store.set_registry(registry)
            logger.info(f"Server unregistered: agent {agent_id}")
        return removed is not None

    def list_servers(self) -> list[dict[str, Any]]:
        return list(self._store.get_registry().values())

    def get_server(self, agent_id: str) -> dict[str, Any] | None:
        return self._store.get_registry().get(agent_id)
