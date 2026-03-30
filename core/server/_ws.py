"""WebSocket endpoint and message handler."""

import json
import logging
from typing import Any

from fastapi import WebSocket, WebSocketDisconnect

from ..dispatch import ToolResult, _DISPATCH
from ..tools import _TOOL_DISPATCH

logger = logging.getLogger(__name__)

# Connected WebSocket clients — ws → scope ("" = all)
ws_subscriptions: dict[WebSocket, str] = {}


async def websocket_endpoint(ws: WebSocket):
    await ws.accept()
    ws_subscriptions[ws] = ""  # default: receive all broadcasts
    logger.info(f"WebSocket client connected ({len(ws_subscriptions)} total)")

    try:
        while True:
            raw = await ws.receive_text()
            msg = json.loads(raw)
            logger.debug(
                f"WS <- {msg.get('type', '?')} {json.dumps(msg, default=str)[:200]}"
            )
            await handle_ws_message(ws, msg)
    except WebSocketDisconnect:
        pass
    except Exception as e:
        logger.error(f"WebSocket error: {e}", exc_info=True)
    finally:
        ws_subscriptions.pop(ws, None)
        logger.info(f"WebSocket client disconnected ({len(ws_subscriptions)} total)")


async def handle_ws_message(ws: WebSocket, msg: dict[str, Any]) -> None:
    from . import broadcast  # deferred: broadcast defined after this module imports

    msg_type = msg.get("type")
    req_id = msg.get("_req_id")

    if msg_type == "subscribe":
        ws_subscriptions[ws] = msg.get("scope", "")
        return

    if msg_type == "call":
        tool = msg.get("tool", "")
        args = msg.get("args", {})
    else:
        tool = msg_type
        args = {k: v for k, v in msg.items() if k not in ("type", "_req_id")}

    def _reply(d: dict) -> str:
        if req_id is not None:
            d["_req_id"] = req_id
        return json.dumps(d, default=str)

    fn = _DISPATCH.get(tool)
    if fn is None:
        if msg_type == "call":
            tool_fn = _TOOL_DISPATCH.get(tool)
            if tool_fn is not None:
                try:
                    result = await tool_fn(**args)
                    await ws.send_text(
                        _reply(
                            {
                                "type": "call_result",
                                "tool": tool,
                                "data": result,
                            }
                        )
                    )
                except Exception as e:
                    await ws.send_text(
                        _reply({"type": "error", "tool": tool, "error": str(e)})
                    )
                return
        await ws.send_text(
            _reply(
                {
                    "type": "error",
                    "tool": tool,
                    "error": f"Unknown message type: {tool}",
                }
            )
        )
        return

    try:
        result = await fn(**args)
    except Exception as e:
        await ws.send_text(_reply({"type": "error", "tool": tool, "error": str(e)}))
        return

    if (
        isinstance(result, ToolResult)
        and isinstance(result.data, dict)
        and "error" in result.data
        and not result.broadcast
        and not result.reply
    ):
        await ws.send_text(
            _reply({"type": "error", "tool": tool, "error": result.data["error"]})
        )
        return

    if isinstance(result, ToolResult):
        for m in result.reply:
            await ws.send_text(json.dumps(m, default=str))
        for m in result.broadcast:
            await broadcast(m)
        if msg_type == "call":
            await ws.send_text(
                _reply(
                    {
                        "type": "call_result",
                        "tool": tool,
                        "data": result.data,
                    }
                )
            )
