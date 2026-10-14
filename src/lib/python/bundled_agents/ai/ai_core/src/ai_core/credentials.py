"""A backend's KEY, read from a credential store at USE time.

The record names WHERE the key lives — `credentials: {store, ref}` — never the
value, and nothing reads the environment. Before EVERY request a provider calls
its reader with the endpoint it is about to call; the reader asks the store for
`api_key`, pinned to that endpoint's host (the store refuses a ref bound to
another host). The request runs in this agent's own turn (its dispatch, or a task
it spawned), so the read is this agent's send, gated as it. Nothing is cached: a
key rotated in the store is used by the next request, with no restart.

No `credentials` on the record ⇒ no key — for a keyless local server. A backend
that cannot run without one marks the field required in its `RECORD_SCHEMA`, and
the reader refuses its absence loud as well.

Wiring, by the operator: the agent's `can_call` names the store, the store's
`callable_by` names the agent."""

from __future__ import annotations

#: The credential field a backend sends as its key.
FIELD = "api_key"


class CredentialError(RuntimeError):
    """The key could not be read — the turn fails, naming the store and the ref."""


def header_safe(key: str) -> bool:
    """Whether `key` can ride `Authorization: Bearer <key>` as it is: visible ASCII,
    spaces or tabs only BETWEEN visible characters. An HTTP client refuses anything
    else, so a key pasted with a trailing newline is refused here, named by its
    ref."""
    if key != key.strip(" \t"):
        return False
    return all(c in " \t" or "!" <= c <= "~" for c in key)


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
        out = (
            await kernel.send(
                [
                    {
                        "target": store,
                        "payload": {"type": "get", "ref": ref, "host": endpoint},
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
