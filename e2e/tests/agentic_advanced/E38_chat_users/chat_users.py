"""THE PEOPLE DIRECTORY — one record per person, many handles per person.

A person is not an account: one person may hold several accounts (`chat:111` and
`chat:555`), and on another network more (`mail:…`). Each handle keeps what the
network says about it — username, display name, per-server nicks. A gateway
reports every author it sees (`seen`), so a person is created the first time an
unknown handle shows up and kept current after; the operator links handles
of one person by writing `people` on the record.

The AI reads it — `resolve` a name, nick, `<@id>`, handle or person id to the
person(s) it may mean, `list` everyone — and keeps per-person notes (`note`,
`notes`). `seen` is the gateway's verb, never the model's.
"""

from __future__ import annotations

import re

SENTENCE = (
    "The people directory: a person may have several accounts (handles like "
    "`chat:<user id>`). `resolve` a name, nick, <@id>, handle or person id to who it "
    "may be; `list` everyone; `note`/`notes` keep what you know about a person."
)
VERBS = {
    "resolve": (
        "args: query:str (req). Matches a person id, a handle (`chat:333`), a mention "
        "(`<@333>`), a bare user id, a username, a display name or a nick, "
        "case-insensitive → {people:[{person, handles:[{handle, username, display, "
        "nicks, bot}]}]} — every match, so an ambiguous name returns several."
    ),
    "list": "no args. {people:[…]} — everyone, same shape as `resolve`.",
    "note": "args: person:str (req), text:str (req). Keeps one note on that person → {notes:n}.",
    "notes": "args: person:str (req). → {person, notes:[str]}.",
    "seen": (
        "args: handle:str (req), username:str (req), display:str?, nick:str?, "
        "server:str?, bot:bool?. A GATEWAY reports an author: the handle's person is "
        "created if unknown, its fields kept current → {person}."
    ),
}

#: per directory id: person id → {handles: {handle: {...}}, notes: [str]}
_PEOPLE: dict[str, dict[str, dict]] = {}


def _err(verb: str, msg: str, **extra) -> dict:
    return {"error": f"chat_users: {verb}: {msg}", "reason": "bad_arg", **extra}


def _people(agent, id: str) -> dict[str, dict]:
    """The directory, seeded once from the record's `people`."""
    if id not in _PEOPLE:
        book: dict[str, dict] = {}
        for p in (agent.get(id) or {}).get("people") or []:
            book[p["person"]] = {
                "handles": {
                    h: {"handle": h, **(p.get("profiles") or {}).get(h, {})}
                    for h in p.get("handles") or []
                },
                "notes": list(p.get("notes") or []),
            }
        _PEOPLE[id] = book
    return _PEOPLE[id]


def _owner(book: dict, handle: str) -> str | None:
    return next((pid for pid, p in book.items() if handle in p["handles"]), None)


def _view(pid: str, p: dict) -> dict:
    return {
        "person": pid,
        "handles": [
            {
                "handle": h["handle"],
                "username": h.get("username"),
                "display": h.get("display"),
                "nicks": dict(h.get("nicks") or {}),
                "bot": bool(h.get("bot")),
            }
            for h in p["handles"].values()
        ],
    }


def _hits(pid: str, p: dict, q: str) -> bool:
    if q == pid.lower():
        return True
    for h in p["handles"].values():
        uid = h["handle"].split(":", 1)[-1]
        names = [
            h["handle"],
            uid,
            f"<@{uid}>",
            h.get("username"),
            h.get("display"),
            *(h.get("nicks") or {}).values(),
        ]
        if q in {str(n).lower() for n in names if n}:
            return True
    return False


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    book = _people(agent, id)
    if verb == "reflect":
        return {"id": id, "sentence": SENTENCE, "people": len(book)}

    if verb == "seen":
        handle, username = payload.get("handle"), payload.get("username")
        if not isinstance(handle, str) or not handle or not isinstance(username, str):
            return _err("seen", "`handle` and `username` are required strings")
        pid = _owner(book, handle)
        if pid is None:
            pid = username
            while pid in book:
                pid += "_"
            book[pid] = {"handles": {}, "notes": []}
        h = book[pid]["handles"].setdefault(handle, {"handle": handle})
        h["username"] = username
        if payload.get("display"):
            h["display"] = payload["display"]
        h["bot"] = bool(payload.get("bot"))
        if payload.get("nick") and payload.get("server"):
            h.setdefault("nicks", {})[str(payload["server"])] = str(payload["nick"])
        return {"person": pid}

    if verb == "resolve":
        q = payload.get("query")
        if isinstance(q, (int, float)) and not isinstance(q, bool):
            q = str(q)
        if not isinstance(q, str) or not q.strip():
            return _err("resolve", "`query` must be a non-empty string", arg="query")
        q = q.strip().lower().lstrip("@")
        m = re.fullmatch(r"<?@?(\d+)>?", q)
        if m:
            q = m.group(1)
        return {
            "people": [_view(pid, p) for pid, p in book.items() if _hits(pid, p, q)]
        }

    if verb == "list":
        return {"people": [_view(pid, p) for pid, p in book.items()]}

    if verb in ("note", "notes"):
        pid = payload.get("person")
        if pid not in book:
            return _err(
                verb,
                f"no person {pid!r} — `resolve` or `list` names them",
                arg="person",
            )
        if verb == "notes":
            return {"person": pid, "notes": list(book[pid]["notes"])}
        text = payload.get("text")
        if not isinstance(text, str) or not text.strip():
            return _err("note", "`text` must be a non-empty string", arg="text")
        book[pid]["notes"].append(text.strip())
        return {"notes": len(book[pid]["notes"])}

    return {
        "error": f"chat_users: unknown verb {verb!r}",
        "reason": "unhandled",
        "verb": verb,
    }
