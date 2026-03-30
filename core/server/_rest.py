"""REST API endpoints."""

import inspect
import logging
import mimetypes
import types as _types
from typing import Any, Union, get_args, get_origin

from fastapi import HTTPException, Query
from fastapi.responses import FileResponse, RedirectResponse
from pydantic import BaseModel

from ..dispatch import _DISPATCH, dispatch
from ..tools import _TOOL_DISPATCH
from . import _state

logger = logging.getLogger(__name__)


class ResolveRequest(BaseModel):
    code: str


class ExecuteRequest(BaseModel):
    code: str


class MemoryAppendRequest(BaseModel):
    type: int
    message: dict


async def resolve_agent(agent_id: str, req: ResolveRequest):
    """External caller submits generated code -> executes and pushes to frontend."""
    from . import broadcast
    try:
        result = await _state.engine.resolve_agent(agent_id, req.code)
        await broadcast({
            "type": "agent_output",
            "agent_id": agent_id,
            "outputs": result["outputs"],
            "success": result["success"],
        })
        await broadcast({
            "type": "agent_complete",
            "agent_id": agent_id,
            "final_code": req.code,
            "outputs": result["outputs"],
        })
        return result
    except ValueError as e:
        return {"error": str(e)}


async def execute_agent(agent_id: str, req: ExecuteRequest):
    """Execute raw code for an agent."""
    from . import broadcast
    try:
        result = await _state.engine.execute_code(agent_id, req.code)
        await broadcast({
            "type": "agent_output",
            "agent_id": agent_id,
            "outputs": result["outputs"],
            "success": result["success"],
        })
        return result
    except ValueError as e:
        return {"error": str(e)}


async def get_state(scope: str = ""):
    """Get state. Filter by scope (container name)."""
    from ..tools._agents import _get_full_state
    tr = await _get_full_state(scope=scope)
    return tr.data


async def list_files_rest():
    """Return project file tree."""
    return {"files": _state.engine.list_files()}


async def get_handbook_rest(skill: str = ""):
    """Return skill handbook."""
    tr = await dispatch("get_handbook", skill=skill)
    if "error" in tr.data:
        return {"handbook": ""}
    return {"handbook": tr.data["text"]}


async def api_call_proxy(body: dict):
    """Call any tool via REST."""
    from . import broadcast
    tool_name = body.get("tool", "")
    args = body.get("args", {})
    fn = _TOOL_DISPATCH.get(tool_name)
    if fn is not None:
        try:
            result = await fn(**args)
            return {"result": result}
        except Exception as e:
            return {"error": str(e)}
    fn = _DISPATCH.get(tool_name)
    if fn is not None:
        try:
            tr = await fn(**args)
            for m in tr.broadcast:
                await broadcast(m)
            return {"result": tr.data}
        except Exception as e:
            return {"error": str(e)}
    return {"error": f"Unknown tool '{tool_name}'"}


# ─── Agent memory ────────────────────────────────────────────────────────


async def get_agent_memory(
    agent_id: str,
    from_ts: str | None = Query(None, alias="from"),
    to_ts: str | None = Query(None, alias="to"),
):
    """Read agent long-term memory, optionally filtered by time range."""
    agent = _state.engine.get_agent(agent_id)
    if not agent:
        raise HTTPException(status_code=404, detail=f"Agent {agent_id} not found")
    entries = _state.engine.store.read_memory(agent_id, from_ts=from_ts, to_ts=to_ts)
    return entries


async def post_agent_memory(agent_id: str, req: MemoryAppendRequest):
    """Append an entry to agent long-term memory."""
    agent = _state.engine.get_agent(agent_id)
    if not agent:
        raise HTTPException(status_code=404, detail=f"Agent {agent_id} not found")
    entry = await _state.engine.store.append_memory(agent_id, req.type, req.message)
    return entry


# ─── Schema builder ─────────────────────────────────────────────────────


def _param_to_json_schema(p: inspect.Parameter) -> dict:
    """Convert a function parameter to a JSON Schema property."""
    type_map = {
        str: "string",
        int: "integer",
        float: "number",
        bool: "boolean",
        dict: "object",
        list: "array",
    }
    annotation = p.annotation if p.annotation != inspect.Parameter.empty else str

    # Handle Optional[X] (Union[X, None]) — both typing.Union and Python 3.10+ X | None
    origin = get_origin(annotation)
    if origin is Union or isinstance(annotation, _types.UnionType):
        args = [a for a in get_args(annotation) if a is not type(None)]
        if args:
            annotation = args[0]

    base = type_map.get(annotation, "string")
    schema: dict[str, Any] = {"type": base}
    if p.default is not inspect.Parameter.empty and p.default is not None:
        schema["default"] = p.default
    return schema


def build_schema() -> dict:
    """Build JSON schema from _TOOL_DISPATCH function signatures + docstrings."""
    tools = []
    for name, fn in sorted(_TOOL_DISPATCH.items()):
        sig = inspect.signature(fn)
        doc = inspect.getdoc(fn) or ""
        description = doc.split("\n")[0].strip() if doc else ""
        params: dict[str, Any] = {"type": "object", "properties": {}, "required": []}
        for pname, p in sig.parameters.items():
            prop = _param_to_json_schema(p)
            params["properties"][pname] = prop
            if p.default is inspect.Parameter.empty:
                params["required"].append(pname)
        tools.append({"name": name, "description": description, "parameters": params})
    return {"version": "1.0", "call_endpoint": "/api/call", "tools": tools}


async def api_schema():
    """Return JSON schema of all available tools."""
    return build_schema()


async def favicon_redirect():
    """Redirect /favicon.ico to /favicon.png."""
    return RedirectResponse("/favicon.png", status_code=301)


async def serve_content_alias(alias_id: str):
    """Serve a content alias — file or URL redirect."""
    alias = _state.engine.content_aliases.get(alias_id)
    if not alias:
        raise HTTPException(status_code=404, detail="Alias not found")
    if alias["type"] == "file":
        file_path = alias["path"]
        if alias.get("relative"):
            file_path = str(_state.engine.project_dir / file_path)
        mt, _ = mimetypes.guess_type(file_path)
        if not mt or mt == "text/plain":
            mt = "application/octet-stream"
        return FileResponse(file_path, media_type=mt)
    return RedirectResponse(alias["url"])
