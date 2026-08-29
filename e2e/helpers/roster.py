"""Extract the agent roster a probe builds — the ONE definition.

Used twice: to write each test's `kernel.yaml`, and by `run.py` to re-check it
before every run. Two copies of this would drift, and a drifting checker is worse
than none — it would report a mismatch that is only in the reader.

It reads the SOURCE rather than running anything, so it stays free and cannot be
fooled by a probe that fails before it finishes building its tree.
"""

from __future__ import annotations

import ast
import pathlib


def _field(v):
    if isinstance(v, ast.Constant):
        return v.value
    try:
        return ast.literal_eval(v)
    except Exception:
        # A value the probe computes at run time (a store id minted by the
        # fixture). Recorded as the expression, so the roster stays honest about
        # what is literal and what is not.
        return f"<{ast.unparse(v)}>"


def roster(path: str | pathlib.Path) -> list[dict]:
    tree = ast.parse(pathlib.Path(path).read_text())
    out: list[dict] = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Dict):
            continue
        keys = {k.value: v for k, v in zip(node.keys, node.values)
                if isinstance(k, ast.Constant)}
        t = keys.get("type")
        if not (isinstance(t, ast.Constant) and t.value == "create_agent"):
            continue
        ent = {f: _field(keys[f]) for f in
               ("id", "handler_module", "bundle", "parent_id") if f in keys}
        for f in ("can_call", "callable_by"):
            if f in keys:
                ent[f] = _field(keys[f])
        out.append(ent)

    # The AI agent is created by `helpers.ai.connect`, not by the probe — but it
    # is still part of the tree the test builds, and its grants are the whole
    # point of a deny-all substrate. Take them from the probe's CAN_CALL.
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign) and any(
            isinstance(t, ast.Name) and t.id == "CAN_CALL" for t in node.targets
        ):
            out.append({"id": "ai", "created_by": "helpers.ai.connect",
                        "can_call": _field(node.value)})
            break
    return out
