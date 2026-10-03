"""A backend's KEY, read from a credential store at USE time (owner, 2026-10-01).

The record names WHERE the key lives — `credentials: {store, ref}` — never the
value, and nothing reads the environment. Before EVERY request a provider calls
its reader with the endpoint it is about to call; the reader asks the store AS
THIS AGENT for `api_key`, the answer marked secret (the value lands in no row,
ledger or stream) and pinned to that endpoint's host (the store refuses a ref
bound to another host). Nothing is cached: a key rotated in the store is used by
the next request, with no restart.

No `credentials` on the record ⇒ no key — for a keyless local server. A backend
that cannot run without one marks the field required in its `RECORD_SCHEMA`, and
the reader refuses its absence loud as well.

Wiring, by the operator: the agent's `can_call` names the store, the store's
`callable_by` names the agent, and the ref's `readers` name it — the store
answers "no such ref" otherwise.

A MODEL NEVER RECEIVES A SECRET (owner, 2026-10-02, option B). The grant that lets
the backend read its key would let the agent's MODEL send the same `get` and read
the value into its prompt and memory. So an agent whose record carries
`holds_secrets: true` (a `credential_store` stamps it on itself; an operator may
set it on any agent) is unreachable from a model turn: `core._route_reply` — the
ONE place a model-parsed call is dispatched — asks `secret_holder_refusal` for
every call's target and `reply_to` and refuses a holder, loud, before anything is
sent. The address is resolved the way the kernel's `send` resolves it first, so
`<this kernel's guid>|<holder>` is the holder, not an unknown id. The reader below
is untouched: it calls `kernel.send` itself, never through the model's tool
dispatch.

Nor does a model JOIN a secret (audit F2, 2026-10-03): a model call whose
`<exec_id>` names an execution that carried `secret` marks is refused the same way
(`secret_join_refusal`) — the backend's key read is such an execution, and the
kernel hands a join by the SAME caller the real reply."""

from __future__ import annotations

from kernel import sender_context
from kernel._refusal import UNAUTHORIZED, refusal
from kernel._secret import PLACEHOLDER

#: The credential field a backend sends as its key.
FIELD = "api_key"

#: The record field that marks an agent a SECRET HOLDER — reachable by backends,
#: never by a model turn.
HOLDS_SECRETS = "holds_secrets"

#: What the model reads when a call of its names a secret holder.
SECRET_HOLDER_TEXT = (
    "{agent} holds secrets — a secret holder answers backends, never a model "
    "turn; this call was not dispatched"
)

#: What the model reads when its `<exec_id>` names an execution that carried
#: secret values (a key read, or any call whose item was marked `secret`).
SECRET_JOIN_TEXT = (
    "exec_id {exec_id} names an execution that carried secret values — a model "
    "never joins one; this call was not dispatched"
)


def _local_record(kernel, address: str) -> dict | None:
    """The record `address` names on THIS kernel, resolved the way `send` resolves
    a target (`Agent.send` → `_qualified_target`): a local id is itself; the
    reserved `kernel` (no agent may take that id) is the root; `<this kernel's
    guid>|<id>` is that local agent (`<this kernel's guid>|kernel` the root). A
    connected peer's `<guid>|<id>` and an unknown id have no record here: None."""
    rec = kernel.get(address)
    if rec is not None:
        return rec
    root = kernel.ctx.root
    if address == "kernel":
        return root.record
    guid, bar, local = address.partition("|")
    if not bar or not guid or not local or "|" in local:
        return None
    if guid != root.record.get("guid"):
        return None
    return root.record if local == "kernel" else kernel.get(local)


def secret_holder_refusal(kernel, agent_id: str) -> dict | None:
    """The refusal for a model-parsed call that names `agent_id` (as its target or
    its `reply_to`), or None when the call may go. Record-driven: only an agent
    whose record says `holds_secrets: true` is refused.

    The address is resolved FIRST, as the kernel resolves it (`_local_record`):
    `<this kernel's guid>|<holder>` reaches the holder, so it is refused like the
    bare id. Only what THIS kernel can see is judged: a connected peer's
    `<guid>|<agent>` lives on another kernel and its record is not here, so it
    passes; that kernel's own AI layer guards its own holders, a holder's value
    is only ever sent to a caller that marked it secret, and a model's join of
    that marked read is `secret_join_refusal`'s."""
    rec = _local_record(kernel, agent_id)
    if not isinstance(rec, dict) or rec.get(HOLDS_SECRETS) is not True:
        return None
    return refusal(
        UNAUTHORIZED, SECRET_HOLDER_TEXT.format(agent=agent_id), agent=agent_id
    )


def secret_join_refusal(kernel, exec_id) -> dict | None:
    """The refusal for a model-parsed call whose `<exec_id>` names an execution
    that carried `secret` marks, or None when the call may go.

    A JOIN is a receive: the kernel hands the joiner that execution's outcome. A
    model cannot mark a call secret, so such an execution was never its own — it
    is its backend's key read (same agent: the kernel would hand back the REAL
    reply) or another caller's (the kernel hands back the shown one). Either way a
    model does not join it. This is what closes a holder on a CONNECTED PEER,
    which `secret_holder_refusal` cannot judge: the backend's read of
    `<peer guid>|creds` is a local execution, and joining it by its id never
    reaches the peer. The id is resolved as the kernel's `send` resolves it."""
    if not kernel.ctx.carried_secret(exec_id):
        return None
    return refusal(
        UNAUTHORIZED, SECRET_JOIN_TEXT.format(exec_id=exec_id), exec_id=exec_id
    )


class CredentialError(RuntimeError):
    """The key could not be read — the turn fails, naming the store and the ref."""


def header_safe(key: str) -> bool:
    """Whether `key` can ride `Authorization: Bearer <key>` as it is: visible ASCII,
    spaces or tabs only BETWEEN visible characters. An HTTP client refuses anything
    else and QUOTES the header it refused in its error (h11: `Illegal header value
    b'Bearer <key>\\n'`) — a key pasted with a trailing newline would ride that
    text into the caller's reply, the ledger and the logs (audit S1)."""
    if key != key.strip(" \t"):
        return False
    return all(c in " \t" or "!" <= c <= "~" for c in key)


def without_key(text: str, key: str | None) -> str:
    """`text` with every occurrence of `key` replaced by the placeholder a recorded
    secret reads as. A provider HOLDS its key, so every error text it raises
    passes through here first: an HTTP client may quote the header it sent and an
    endpoint may echo the request in its error body (audit S1). The key is
    matched as sent AND as a Python literal shows it (h11 quotes the refused
    header as `b'Bearer <key>\\n'`, its newline escaped)."""
    if not key:
        return text
    forms = {key, repr(key)[1:-1], repr(key.encode())[2:-1]}
    for form in sorted(forms, key=len, reverse=True):
        text = text.replace(form, PLACEHOLDER)
    return text


def shape_error(v) -> str | None:
    """Why a `credentials` value is malformed, or None: a map of exactly a
    non-empty `store` id and a non-empty `ref`."""
    if not isinstance(v, dict) or set(v) != {"store", "ref"}:
        return "credentials must be {store: <credential_store id>, ref: <ref>}"
    for k in ("store", "ref"):
        if not isinstance(v[k], str) or not v[k].strip():
            return f"credentials.{k} must be a non-empty string"
    return None


def key_reader(self_id: str, kernel, backend: str, *, required: bool):
    """The async `read(endpoint) -> key | None` a provider awaits before each
    request. Raises `CredentialError` when the store refuses, the field is missing
    or is not a legal HTTP header value (`header_safe`), and when a required key
    has no `credentials` on the record."""

    async def read(endpoint: str) -> str | None:
        cred = (kernel.get(self_id) or {}).get("credentials")
        if cred is None:
            if required:
                raise CredentialError(
                    f"{backend}: no `credentials` on {self_id} — name the key's "
                    "credential store and ref (update_agent "
                    f"{self_id} credentials={{store: <id>, ref: <ref>}})"
                )
            return None
        bad = shape_error(cred)
        if bad:
            raise CredentialError(f"{backend}: {bad}")
        store, ref = cred["store"], cred["ref"]
        # AS ITSELF: the store's per-ref `readers` name this agent, whatever task
        # the request runs in.
        with sender_context(self_id):
            out = (
                await kernel.send(
                    [
                        {
                            "target": store,
                            "payload": {"type": "get", "ref": ref, "host": endpoint},
                            "secret": ["result"],
                        }
                    ]
                )
            )[0]
        res = out.get("result")
        if (
            out.get("status") != "completed"
            or not isinstance(res, dict)
            or res.get("error")
        ):
            err = res.get("error") if isinstance(res, dict) else out.get("status")
            raise CredentialError(f"{backend}: credentials {store}/{ref}: {err}")
        key = (res.get("fields") or {}).get(FIELD)
        if not isinstance(key, str) or not key:
            raise CredentialError(
                f"{backend}: credential {ref!r} in {store!r} has no {FIELD!r} field"
            )
        if not header_safe(key):  # named by its ref, never by its value
            raise CredentialError(
                f"{backend}: credential {ref!r} in {store!r}: {FIELD!r} is not a legal "
                "HTTP header value (a control or non-ASCII character, or whitespace at "
                "an edge) — store it again without"
            )
        return key

    return read
