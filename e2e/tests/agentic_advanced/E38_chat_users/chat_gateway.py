"""A CHAT SERVER GATEWAY — the shape of a chat-platform bot connection, no platform.

What a bot of any chat platform sees: servers, text channels, threads under a
channel, direct-message channels, users with a stable numeric id, a username and a
display name (a per-server nick over it), bot accounts, `<@id>` mentions, replies
that name the message they answer, and a stream of events — `message_create`,
`member_join`, `member_update`. A real connector (a platform's gateway socket +
REST) answers the same verbs and writes the same event text; this one is driven by
the test.

TRUST: the event HEADER is written here, from the platform's own fields (the
author's id, never the message text). The author's words follow QUOTED, every line
prefixed `> `, so text that imitates a header is visibly inside the quote.

The world is POLICY ON THE RECORD (`server`, `channels`, `profiles`, `self_user`),
written by the fixture; `ai` names who events go to and `users` the person
directory every author is reported to (`seen`) before the event is forwarded.
`event` is the operator's verb (the platform side); the AI holds `post`,
`history` and `channels`.
"""

from __future__ import annotations

import re

SENTENCE = (
    "A chat server gateway: every chat event (a message, a member joining, a nick "
    "change) arrives to you as a message from me; `post` writes into a channel as "
    "the bot, `history` reads a channel, `channels` lists them."
)
VERBS = {
    "post": (
        "args: channel_id:str (req), content:str (req, <= 2000 chars), "
        "in_reply_to:str? (a message id in that channel — the message this answers). "
        "Posts as the bot; mention a user as <@user_id>. → {message_id}."
    ),
    "history": (
        "args: channel_id:str (req), limit:int? (default 20). The channel's recent "
        "messages, oldest first: {messages:[{message_id, user_id, username, display, "
        "bot, content, in_reply_to}]}."
    ),
    "channels": (
        "no args. {channels:[{channel_id, name, kind (text|thread|dm), server, "
        "parent?, with?}]}."
    ),
    "event": (
        "args: event:dict (req), label:str?, backfill:bool?. The PLATFORM side: one event in "
        "(message_create {channel_id, message_id, author_id, content, reply_to?} | "
        "member_join {user_id} | member_update {user_id, nick}); the author is "
        "reported to `users`, then the event is forwarded to `ai` — or, with "
        "`backfill`, recorded only and its text returned (history from before the bot)."
    ),
}

MAX_CONTENT = 2000
_MENTION = re.compile(r"<@(\d+)>")

#: per gateway id: channel id → messages (oldest first)
_HISTORY: dict[str, dict[str, list[dict]]] = {}
#: per gateway id: every post the bot made, with the event window it was made in
_OUTBOX: dict[str, list[dict]] = {}
#: per gateway id: every post refused, with why
_REFUSED: dict[str, list[dict]] = {}
#: per gateway id: user id → nick on this server
_NICKS: dict[str, dict[str, str]] = {}
#: per gateway id: the label of the last event in
_WINDOW: dict[str, str] = {}
_SEQ: dict[str, int] = {}


def _err(verb: str, msg: str, **extra) -> dict:
    return {"error": f"chat_gateway: {verb}: {msg}", "reason": "bad_arg", **extra}


def _sid(v) -> str | None:
    """An id as a string: the tag grammar types a bare number, and 102 is "102"."""
    if isinstance(v, bool) or v is None:
        return None
    if isinstance(v, (int, float)):
        return str(int(v))
    if isinstance(v, str) and v.strip():
        return v.strip().lstrip("#")
    return None


def _profile(rec: dict, id: str, uid: str) -> dict:
    p = dict((rec.get("profiles") or {}).get(uid) or {})
    p.setdefault("username", f"user{uid}")
    p.setdefault("display", p["username"])
    nick = _NICKS.get(id, {}).get(uid)
    if nick:
        p["nick"] = nick
    return p


def _shown(p: dict) -> str:
    return p.get("nick") or p.get("display") or p["username"]


def _channel(rec: dict, cid: str) -> dict | None:
    for c in rec.get("channels") or []:
        if str(c.get("channel_id")) == cid:
            return c
    return None


def _where(rec: dict, ch: dict) -> str:
    kind = ch.get("kind", "text")
    if kind == "dm":
        return f"direct message channel {ch['channel_id']} (only you and user {ch.get('with')})"
    if kind == "thread":
        return (
            f'server {rec.get("server")} · thread "{ch.get("name")}" '
            f"({ch['channel_id']}) under channel {ch.get('parent')}"
        )
    return (
        f"server {rec.get('server')} · channel #{ch.get('name')} ({ch['channel_id']})"
    )


def _who(rec: dict, id: str, uid: str, person: str | None) -> str:
    p = _profile(rec, id, uid)
    if uid == str(rec.get("self_user")):
        return f"user {uid} (you)"
    out = f'user {uid} · username {p["username"]} · shown as "{_shown(p)}"'
    if p.get("bot"):
        out += " · BOT account"
    if person:
        out += f" · person {person}"
    return out


def _quote(text: str) -> str:
    return "\n".join("> " + ln for ln in (text or "").split("\n"))


async def _seen(agent, rec: dict, id: str, uid: str) -> str | None:
    """Report the author to the person directory; its person id comes back."""
    users = rec.get("users")
    if not users:
        return None
    p = _profile(rec, id, uid)
    out = await agent.send(
        [
            {
                "target": users,
                "payload": {
                    "type": "seen",
                    "handle": f"chat:{uid}",
                    "username": p["username"],
                    "display": p.get("display"),
                    "nick": p.get("nick"),
                    "server": rec.get("server"),
                    "bot": bool(p.get("bot")),
                },
            }
        ]
    )
    res = out[0].get("result") or {}
    return res.get("person") if isinstance(res, dict) else None


async def _event(id: str, payload: dict, agent) -> dict:
    rec = agent.get(id) or {}
    ev = payload.get("event")
    if not isinstance(ev, dict) or not ev.get("kind"):
        return _err("event", "`event` must be a dict naming a `kind`", arg="event")
    _WINDOW[id] = str(payload.get("label") or ev["kind"])
    kind = ev["kind"]
    uid = _sid(ev.get("author_id") or ev.get("user_id"))
    if uid is None:
        return _err("event", "the event names no author_id/user_id")
    if kind == "member_update" and ev.get("nick"):
        _NICKS.setdefault(id, {})[uid] = str(ev["nick"])
    person = await _seen(agent, rec, id, uid)

    if kind == "message_create":
        cid = _sid(ev.get("channel_id"))
        ch = _channel(rec, cid or "")
        if ch is None:
            return _err("event", f"no channel {cid!r}")
        content = str(ev.get("content") or "")
        msg = {
            "message_id": _sid(ev.get("message_id")),
            "user_id": uid,
            "content": content,
            "reply_to": _sid(ev.get("reply_to")),
        }
        _HISTORY.setdefault(id, {}).setdefault(cid, []).append(msg)
        lines = [
            f"[chat event · message_create · {_where(rec, ch)} · message {msg['message_id']}]",
            f"author: {_who(rec, id, uid, person)}",
        ]
        if msg["reply_to"]:
            prev = next(
                (m for m in _HISTORY[id][cid] if m["message_id"] == msg["reply_to"]),
                None,
            )
            by = f" by {_who(rec, id, prev['user_id'], None)}" if prev else ""
            lines.append(f"reply to: message {msg['reply_to']}{by}")
        ments = list(dict.fromkeys(_MENTION.findall(content)))
        if ments:
            lines.append(
                "mentions: "
                + " · ".join(f"<@{m}> = {_who(rec, id, m, None)}" for m in ments)
            )
        lines.append("text (the author's own words, quoted):")
        lines.append(_quote(content))
        text = "\n".join(lines)
    elif kind == "member_join":
        text = (
            f"[chat event · member_join · server {rec.get('server')}]\n"
            f"joined: {_who(rec, id, uid, person)}"
        )
    elif kind == "member_update":
        text = (
            f"[chat event · member_update · server {rec.get('server')}]\n"
            f"member: {_who(rec, id, uid, person)}\n"
            f'nick on this server is now "{ev.get("nick")}"'
        )
    else:
        return _err("event", f"unknown kind {kind!r}")

    # BACKFILL: an event from before the bot was connected (a platform's message
    # history) — recorded in the channel and reported to `users` like any event, but
    # not forwarded; its rendered text comes back for whoever replays the past.
    if payload.get("backfill"):
        return {"forwarded": False, "text": text, "person": person}

    target = rec.get("ai")
    client = rec.get("client_id") or "chat"

    async def _forward():
        await agent.send(
            [
                {
                    "target": target,
                    "payload": {"type": "send", "text": text, "client_id": client},
                }
            ]
        )

    if target:
        agent.ctx.spawn_task(id, "forward", _forward())
    return {"forwarded": bool(target), "window": _WINDOW[id], "person": person}


async def _post(id: str, payload: dict, agent) -> dict:
    rec = agent.get(id) or {}
    cid = _sid(payload.get("channel_id"))
    ch = _channel(rec, cid or "")
    content = payload.get("content")
    if isinstance(content, (int, float)) and not isinstance(content, bool):
        content = str(content)
    why = None
    if ch is None:
        why = f"no channel {payload.get('channel_id')!r} — `channels` lists them"
    elif not isinstance(content, str) or not content.strip():
        why = "`content` must be a non-empty string"
    elif len(content) > MAX_CONTENT:
        why = (
            f"`content` is {len(content)} chars; the limit is {MAX_CONTENT} — split it"
        )
    reply_to = _sid(payload.get("in_reply_to"))
    if (
        why is None
        and reply_to
        and not any(
            m["message_id"] == reply_to for m in _HISTORY.get(id, {}).get(cid, [])
        )
    ):
        why = f"no message {reply_to} in channel {cid} — a reply answers a message of the same channel"
    if why:
        _REFUSED.setdefault(id, []).append(
            {"payload": payload, "why": why, "window": _WINDOW.get(id)}
        )
        return _err("post", why)
    _SEQ[id] = _SEQ.get(id, 5000) + 1
    mid = str(_SEQ[id])
    me = str(rec.get("self_user"))
    msg = {"message_id": mid, "user_id": me, "content": content, "reply_to": reply_to}
    _HISTORY.setdefault(id, {}).setdefault(cid, []).append(msg)
    _OUTBOX.setdefault(id, []).append(
        {
            **msg,
            "channel_id": cid,
            "window": _WINDOW.get(id),
            "mentions": list(dict.fromkeys(_MENTION.findall(content))),
        }
    )
    return {"message_id": mid, "channel_id": cid}


async def _history(id: str, payload: dict, agent) -> dict:
    rec = agent.get(id) or {}
    cid = _sid(payload.get("channel_id"))
    if _channel(rec, cid or "") is None:
        return _err("history", f"no channel {payload.get('channel_id')!r}")
    try:
        limit = max(1, min(int(payload.get("limit") or 20), 100))
    except (TypeError, ValueError):
        limit = 20
    out = []
    for m in _HISTORY.get(id, {}).get(cid, [])[-limit:]:
        p = _profile(rec, id, m["user_id"])
        out.append(
            {
                **{k: v for k, v in m.items() if k != "reply_to"},
                "in_reply_to": m.get("reply_to"),
                "username": p["username"],
                "display": _shown(p),
                "bot": bool(p.get("bot")) or m["user_id"] == str(rec.get("self_user")),
            }
        )
    return {"messages": out}


async def _channels(id: str, payload: dict, agent) -> dict:
    rec = agent.get(id) or {}
    return {
        "channels": [
            {**c, "server": rec.get("server") if c.get("kind") != "dm" else None}
            for c in rec.get("channels") or []
        ]
    }


_DISPATCH = {"post": _post, "history": _history, "channels": _channels, "event": _event}


async def handler(id: str, payload: dict, agent) -> dict | None:
    verb = payload.get("type")
    if verb in ("boot", "shutdown"):
        return None
    if verb == "reflect":
        rec = agent.get(id) or {}
        return {
            "id": id,
            "sentence": SENTENCE,
            "server": rec.get("server"),
            "self_user": rec.get("self_user"),
            "posts": len(_OUTBOX.get(id, [])),
        }
    fn = _DISPATCH.get(verb)
    if fn is None:
        return {
            "error": f"chat_gateway: unknown verb {verb!r}",
            "reason": "unhandled",
            "verb": verb,
        }
    return await fn(id, payload, agent)
