"""ai_chat bundle — the AI agent apart from any provider (twin of chat_bundle.rs).

It holds the conversation (the turn loop, the `<call>` parse and route, the memory,
the folds) and completes each turn through the provider agent its record names
(`provider_id`), by `complete`. Model, endpoint and key are the provider's.
"""

from __future__ import annotations

from ai_core import build, on_cancel, on_delete  # noqa: F401 — the hooks
from ai_core.remote_provider import CHAT_HANDLER, PROVIDER_VERBS, chat_provider
from kernel._refusal import UNHANDLED, refusal

NAME = "ai_chat"
HANDLER_MODULE = CHAT_HANDLER
SENTENCE = (
    "An AI chat: holds its conversation and completes each turn through its "
    "provider agent (`provider_id`)."
)


def _reflect_extra(id, kernel):
    return {"provider_id": (kernel.get(id) or {}).get("provider_id")}


_verbs, _core = build(
    sentence=SENTENCE,
    make_provider=chat_provider,
    name=NAME,
    reflect_extra=_reflect_extra,
    provider_cache=True,
)

# a provider's verbs are not a chat's
VERBS = {k: v for k, v in _verbs.items() if k not in PROVIDER_VERBS}


async def handler(id: str, payload: dict, kernel) -> dict | None:
    t = payload.get("type")
    if t in PROVIDER_VERBS:
        return refusal(
            UNHANDLED,
            f"ai_chat: `{t}` is a provider's verb; this chat completes through its "
            "provider_id (ask it there)",
        )
    return await _core(id, payload, kernel)


from ai_core import CANCEL, EMITS, NOTES  # noqa: E402, F401
