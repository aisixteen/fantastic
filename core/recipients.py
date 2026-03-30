"""Recipient abstraction for @tag command routing.

Each recipient handles a set of commands. @core is the default recipient
when no @ tag is given.
"""

from typing import Any


class Recipient:
    """Abstract command recipient."""

    name: str = ""

    def parse(self, text: str) -> tuple[str, dict[str, Any]] | None:
        """Parse text into (tool_name, args) or None if not a command."""
        return None

    async def execute(self, tool_name: str, args: dict[str, Any]) -> Any:
        """Execute a parsed command. Returns ToolResult or string."""
        raise NotImplementedError


class CoreRecipient(Recipient):
    """Handles core commands: add, remove, list, log, say."""

    name = "core"

    def parse(self, text: str) -> tuple[str, dict[str, Any]] | None:
        parts = text.split()
        if not parts:
            return None
        cmd = parts[0].lower()

        if cmd == "add" and len(parts) >= 2:
            # Parse --name flag
            name = ""
            if "--name" in parts:
                idx = parts.index("--name")
                if idx + 1 < len(parts):
                    name = parts[idx + 1]
            # Parse --working-dir flag
            working_dir = ""
            if "--working-dir" in parts:
                idx = parts.index("--working-dir")
                if idx + 1 < len(parts):
                    working_dir = parts[idx + 1]
            # Parse --from flag
            from_source = ""
            if "--from" in parts:
                idx = parts.index("--from")
                if idx + 1 < len(parts):
                    from_source = parts[idx + 1]
            return (
                "add_bundle",
                {
                    "bundle_name": parts[1],
                    "name": name,
                    "working_dir": working_dir,
                    "from_source": from_source,
                },
            )
        if cmd == "remove" and len(parts) >= 2:
            name = ""
            if "--name" in parts:
                idx = parts.index("--name")
                if idx + 1 < len(parts):
                    name = parts[idx + 1]
            return ("remove_bundle", {"bundle_name": parts[1], "name": name})
        if cmd == "list":
            return ("list_bundles", {})
        if cmd == "log":
            max_lines = int(parts[1]) if len(parts) >= 2 else 100
            return ("conversation_log", {"max_lines": max_lines})
        if cmd == "run" and len(parts) >= 2:
            return ("run_bundle", {"bundle_name": parts[1]})
        return None

    async def execute(self, tool_name: str, args: dict[str, Any]) -> Any:
        from .tools._bundles import _add_bundle, _remove_bundle, _list_bundles
        from .tools._conversation import _conversation_log, _conversation_say

        handlers = {
            "add_bundle": _add_bundle,
            "remove_bundle": _remove_bundle,
            "list_bundles": _list_bundles,
            "conversation_log": _conversation_log,
            "conversation_say": _conversation_say,
        }
        fn = handlers.get(tool_name)
        if not fn:
            return None
        return await fn(**args)
