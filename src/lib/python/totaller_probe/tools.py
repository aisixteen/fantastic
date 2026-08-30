
VERBS = {"ping": "no args. Returns pong."}
async def handler(id, payload, kernel):
    v = payload.get("type")
    if v in ("boot", "shutdown"):
        return None
    if v == "reflect":
        return {"id": id, "sentence": "probe custom handler", "verbs": {"ping": "no args"}}
    if v == "ping":
        return {"pong": True, "id": id}
    return {"error": f"unknown {v!r}"}
