"""The INBOUND verbs of a wire leg (`web_ws`, `web_rest`) — how an admitted frame
becomes the leg's own call.

IDENTITY COMES ONLY FROM DISPATCH (owner 2026-10-04). A transport's loop (the
uvicorn server, a websocket's receive loop) runs outside every dispatch, so what
it sends is the operator's. It never sends a frame onward itself: it hands the
admitted frame to its LEG as one of these verbs, and the leg's handler performs
the onward call — so that call is the leg's, gated as the leg (its `can_call`,
the target's `callable_by`). Whoever is on the other end of the wire is not an
identity in this kernel; the leg is.

The leg's `ingress_rule` has already judged the frame (`gate_inbound`) before the
transport hands it in. These verbs are gated like any other: the transport's
hand-in is the operator's (ungated); an agent calling them needs the grant on the
leg like any verb.
"""

from __future__ import annotations

from typing import Any

from kernel._refusal import bad


async def inbound_call(id: str, payload: dict, kernel: Any):
    """args: item:{target, payload, id?} (req). Sends `item` onward AS THIS LEG and returns the onward call's reply — how an admitted inbound call frame becomes the leg's own call, gated as the leg."""
    item = payload.get("item")
    if not isinstance(item, dict):
        return bad("inbound_call", "item", "a batch item {target, payload, id?}")
    return (await kernel.send([item]))[0]["result"]


async def inbound_emit(id: str, payload: dict, kernel: Any):
    """args: target:str (req), payload:dict (req). Emits `payload` on `target` AS THIS LEG (gated as the leg, verb `emit`) — how an admitted inbound emit frame becomes the leg's own emit."""
    target = payload.get("target")
    inner = payload.get("payload")
    if not isinstance(target, str) or not target:
        return bad("inbound_emit", "target", "an agent id")
    if not isinstance(inner, dict):
        return bad("inbound_emit", "payload", "an object")
    await kernel.emit(target, inner)
    return None


async def inbound_watch(id: str, payload: dict, kernel: Any):
    """args: src:str (req — an agent id), reader:str (req — a registered inbox). Mirrors `src`'s traffic into `reader` AS THIS LEG (gated as the leg, verb `watch`). Returns {watching:src} or the refusal."""
    src = payload.get("src")
    reader = payload.get("reader")
    if not isinstance(src, str) or not src:
        return bad("inbound_watch", "src", "an agent id")
    if not isinstance(reader, str) or not reader:
        return bad("inbound_watch", "reader", "a registered inbox id")
    denial = kernel.watch(src, reader)
    return denial if denial is not None else {"watching": src}


async def inbound_state_subscribe(id: str, payload: dict, kernel: Any):
    """No args. May THIS LEG tap the kernel-wide state stream (gated on the root, verb `state_subscribe`)? Returns {subscribed:true} or the refusal; the transport then wires the tap."""
    denial = kernel.authorize_state_subscribe(id)
    return denial if denial is not None else {"subscribed": True}


async def hand_in(kernel: Any, leg_id: str, payload: dict):
    """THE TRANSPORT'S ONE SEND: hand one admitted frame to the leg `leg_id` as an
    inbound verb and return the leg's reply. Made from the transport's loop —
    outside every dispatch, so it is the operator's (`kernel` is any agent of the
    tree; the send's identity is the context's, and here there is none)."""
    return (await kernel.send([{"target": leg_id, "payload": payload}]))[0]["result"]
