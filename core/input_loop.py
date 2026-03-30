"""Conversation input loop — runs in both Core-only and Core+Server modes.

Supports @tag routing: @core for core commands (default when no tag).
Unrecognized input becomes conversation.
"""

import asyncio
import logging

from . import conversation
from .recipients import CoreRecipient, Recipient

logger = logging.getLogger(__name__)


class InputLoop:
    """Interactive conversation loop with @tag routing."""

    def __init__(self, remote_url: str | None = None):
        self._remote_url = remote_url
        self._core = CoreRecipient()
        self._recipients: dict[str, Recipient] = {
            "core": self._core,
        }

    def register_recipient(self, recipient: Recipient) -> None:
        """Register an additional recipient for @tag routing."""
        self._recipients[recipient.name] = recipient

    async def run(self):
        """Main input loop: read → parse → execute → print."""
        # Show recent conversation only when connecting to running server
        if self._remote_url:
            for entry in conversation.read(max_lines=20):
                print(conversation.format_entry(entry))

        loop = asyncio.get_event_loop()
        while True:
            try:
                line = await loop.run_in_executor(
                    None,
                    lambda: input(f"{conversation.USER_COLOR}>{conversation.RESET} "),
                )
                line = line.strip()
                if not line:
                    continue
            except (EOFError, KeyboardInterrupt):
                print()
                break

            if line.lower() in ("exit", "quit", "q"):
                print()
                break

            # @tag routing
            if line.startswith("@"):
                tag, _, rest = line[1:].partition(" ")
                rest = rest.strip()
                recipient = self._recipients.get(tag)
                if recipient:
                    await self._dispatch_to(recipient, rest)
                else:
                    conversation.say("fantastic", f"unknown: @{tag}")
                    print(
                        conversation.format_entry(
                            {"who": "fantastic", "message": f"unknown: @{tag}"}
                        )
                    )
            else:
                # Default → @core
                await self._dispatch_to(self._core, line)

    def _say_system(self, message: str):
        entry = conversation.say("fantastic", message)
        print(conversation.format_entry(entry))

    async def _run_chat_agent(self, bundle_name: str):
        """Discover and run a bundle's @chat_run function."""
        import importlib.util
        from ._paths import bundled_agents_dir
        from .chat_run import find_chat_run

        tools_file = bundled_agents_dir() / bundle_name / "tools.py"
        if not tools_file.exists():
            self._say_system(f"bundle '{bundle_name}' not found")
            return

        spec = importlib.util.spec_from_file_location(
            f"bundle_{bundle_name}_run",
            str(tools_file),
        )
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)

        run_fn = find_chat_run(mod)
        if run_fn is None:
            self._say_system(f"bundle '{bundle_name}' has no @chat_run function")
            return

        loop = asyncio.get_event_loop()

        async def ask(prompt: str) -> str:
            entry = conversation.say(bundle_name, prompt)
            print(conversation.format_entry(entry))
            answer = await loop.run_in_executor(
                None, lambda: input(f"{conversation.USER_COLOR}>{conversation.RESET} ")
            )
            answer = answer.strip()
            conversation.say("user", answer)
            return answer

        def say(message: str):
            entry = conversation.say(bundle_name, message)
            print(conversation.format_entry(entry))

        try:
            await run_fn(ask, say)
        except (EOFError, KeyboardInterrupt):
            say("interrupted")
        except Exception as e:
            say(f"error: {e}")

    async def _dispatch_to(self, recipient: Recipient, text: str):
        """Parse and execute via a recipient, or fall through to conversation."""
        parsed = recipient.parse(text)
        if parsed:
            tool_name, args = parsed
            if tool_name == "run_bundle":
                await self._run_chat_agent(args["bundle_name"])
                return
            result = await self._execute(recipient, tool_name, args)
            self._print_result(tool_name, result)
        else:
            # Plain conversation
            conversation.say("user", text)
            if self._remote_url:
                await self._remote_call(
                    "conversation_say", {"who": "user", "message": text}
                )

    async def _execute(self, recipient: Recipient, tool_name: str, args: dict) -> str:
        """Execute a tool — locally via recipient or via REST."""
        if self._remote_url:
            return await self._remote_call(tool_name, args)
        tr = await recipient.execute(tool_name, args)
        if tr is None:
            return f"Unknown command: {tool_name}"
        return self._format_tool_result(tool_name, tr)

    async def _remote_call(self, tool_name: str, args: dict) -> str:
        """Call tool via REST /api/call."""
        import httpx

        try:
            async with httpx.AsyncClient() as client:
                resp = await client.post(
                    f"{self._remote_url}/api/call",
                    json={"tool": tool_name, "args": args},
                    timeout=10,
                )
                data = resp.json()
                return data.get("result", str(data))
        except Exception as e:
            return f"[ERROR] {e}"

    def _format_tool_result(self, tool_name: str, tr) -> str:
        """Format a ToolResult for CLI display."""
        data = tr.data
        if isinstance(data, dict) and "error" in data:
            return f"  [ERROR] {data['error']}"
        if tool_name == "list_bundles" and isinstance(data, list):
            lines = []
            for b in data:
                if b.get("instances"):
                    for inst in b["instances"]:
                        display = inst.get("display_name") or inst["id"]
                        children = inst.get("children", 0)
                        lines.append(
                            f"  {b['name']}  {display}  ({inst['id']})  [{children} agents]"
                        )
                else:
                    status = "[available]"
                    lines.append(f"  {b['name']}  {status}")
            return "\n".join(lines) if lines else "  (no bundles found)"
        if tool_name == "conversation_log":
            entries = data.get("entries", [])
            if not entries:
                return "  (no conversation history)"
            return "\n".join(conversation.format_entry(e) for e in entries)
        if isinstance(data, dict):
            # Generic success
            return "  " + " ".join(f"{k}={v}" for k, v in data.items())
        return str(data)

    def _print_result(self, tool_name: str, result: str):
        """Print tool result to CLI."""
        if result:
            print(result)
