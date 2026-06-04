"""REPL output + input helpers."""

from __future__ import annotations

import asyncio
import json
from typing import Any


async def _print_result(result: Any) -> None:
    if result is None:
        return
    if isinstance(result, dict):
        if "error" in result:
            print(f"  error: {result['error']}")
            return
        if "id" in result and "handler_module" in result:
            print(f"  created {result['id']}")
            return
    try:
        print(f"  {json.dumps(result, indent=2, default=str)}")
    except (TypeError, ValueError):
        print(f"  {result}")


async def _read_line(prompt: str) -> str:
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, input, prompt)
