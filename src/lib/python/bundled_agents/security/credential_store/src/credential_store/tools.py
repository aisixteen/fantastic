"""credential_store — secrets an agent USES but never keeps (owner, 2026-10-01).

A credential is addressed by a REF (`openai/main`) and is bound to ONE HOST: a value
plus the host it may be sent to. A bundle that needs one names it on its own record
(`credentials: {store, ref}`), reads it at USE time and refuses to send it to any
other host — so rotating a key is rewriting the ref, and no restart is ever needed.

WHERE THE VALUES LIVE — `backend` on the record, one of:
  * `file` (the default): a YAML file at an ABSOLUTE path —
    `~/.fantastic/credentials/<store id>.yaml` unless `file` names another one
    (`/…` or `~…`; a relative path is refused at the write). It is written 0600
    and READ ONLY IF it is a regular file owned by this user with no group/other
    bits — anything else is refused, loud. It is judged on the ONE open it is read
    from (`fs.read_private`: `O_NOFOLLOW`, then `fstat` of that descriptor), so no
    swap can land between the check and the read.
  * `keychain`: the OS keychain (macOS Keychain, Secret Service, Windows Credential
    Locker), one item per ref, under the namespace `keychain_namespace` (default
    `fantastic` — macOS calls it the item's "service"), account
    `<store id>/<ref>`. The explicit choice for the strongest at-rest protection.
  * `env`: read-only; each ref maps its fields to environment variables
    (`refs: {ref: {host, env: {api_key: VAR}}}`), for a container or CI.

THE HOST IS STORED WITH THE VALUE (audit S3, 2026-10-03): `set` writes the ref's
declared host beside its fields — the file entry and the keychain item are
`{host, fields}` — and `get` refuses a value whose stored host is not the one its
declaration names, or that carries none: a store elsewhere declaring the same id
and ref with another host reads nothing. The `env` backend keeps no value at rest.

A REF IS DECLARED ONLY BY `update_agent` on the store's own record — the kernel's
privileged verb, granted explicitly, is the one path:
`refs: {<ref>: {host, readers: [...], writers: [...], fields?/env?}}` (it replaces
the whole map; `host` is required — the binding). The DECLARATION is the
operator's and only `update_agent` writes or removes it (audit AV-4/AV-5,
2026-10-03): `set` stores the value of a DECLARED ref, never creates one and takes
no `host` (an undeclared ref answers "no such ref", to the operator too; a `host`
on the call is refused); `forget` drops the value and keeps the declaration.
`set`/`forget` need the caller in `writers`, `get` in `readers`; an entry is a
local agent id, `$subtree:<id>` (that agent and every live descendant), or a
qualified `<guid>|<agent>` caller on another kernel. The operator passes every
reader and writer check. A caller with no grant on a ref is answered "no such
ref" — never told the ref exists. `fields` on a ref are the stored value's NAMES:
`set` writes them, `forget` drops them.

NO VALUE IS EVER RECORDED. `set` must be called with `secret: ['payload.fields']`
and `get` with `secret: ['result']` (`kernel/_secret.py`): the call is refused
otherwise, because the value would land in the ledger, the memory store and the
state stream. Either spelling of a mark counts (`['payload', 'fields']` is
`payload.fields`), as does marking every field one by one; a field name holding a
dot is marked only as an array (`['result', 'fields', 'a.b']`). The record holds
only metadata — the host and the field NAMES.

The kernel's call gate still decides who may reach this agent at all.

A SECRET HOLDER, NEVER A MODEL'S TOOL (owner, 2026-10-02): the store stamps
`holds_secrets: true` on its own record at boot, and `validate_meta` refuses any
other value. ai_core refuses every model-parsed call that names an agent so
marked, so an AI agent's backend reads its key here while its model cannot. The
kernel judges first — its shape check refuses anything but `true`/`null` on every
agent, so this text answers `null`; its gate lets only the operator lower a mark.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from urllib.parse import urlsplit

from file_bridge import fs
from kernel_state import yaml_io
from kernel import current_origin, current_remote_caller, current_secret
from kernel._refusal import (
    BAD_ARG,
    NOT_FOUND,
    UNAUTHORIZED,
    UNHANDLED,
    missing,
    refusal,
)

NAME = "credential_store"
BACKENDS = ("file", "keychain", "env")
#: The record field every store carries as `true` — read by ai_core, which refuses
#: a model-parsed call to an agent so marked.
HOLDS_SECRETS = "holds_secrets"
DEFAULT_DIR = "~/.fantastic/credentials"
KEYCHAIN_NAMESPACE = "fantastic"
SUBTREE = "$subtree:"


# ─── who is calling, and what they may do ──────────────────────────


def _caller() -> str | None:
    """The calling agent's id — qualified `<guid>|<agent>` when it is on another
    kernel — or None for the operator."""
    return current_remote_caller() or current_origin()


def _covers(agent, entries, caller: str | None) -> bool:
    """Whether a readers/writers list names `caller`. The operator always passes."""
    if caller is None:
        return True
    for e in entries or []:
        if not isinstance(e, str):
            continue
        if e == caller:
            return True
        if e.startswith(SUBTREE) and "|" not in caller:
            root = e[len(SUBTREE) :]
            a = agent.ctx.agents.get(caller)
            while a is not None:
                if a.id == root:
                    return True
                a = a.parent
    return False


def _refs(agent) -> dict:
    r = agent.record.get("refs")
    return dict(r) if isinstance(r, dict) else {}


def _no_such_ref(ref) -> dict:
    return refusal(NOT_FOUND, f"{NAME}: no such ref {ref!r}", arg="ref")


def _host(h) -> str | None:
    """A host as stored and compared: the `host[:port]` of a URL or of a bare host,
    lowercased."""
    if not isinstance(h, str) or not h.strip():
        return None
    h = h.strip()
    if "://" in h:
        h = urlsplit(h).netloc
    return h.lower() or None


def _names(mark, steps: list[str]) -> bool:
    """Whether one secret mark names exactly `steps` — a dotted string by its
    dots, an array of steps step for step (`kernel/_secret`). A field name holding
    a dot is named only by an array: `result.fields.a.b` is `a` → `b`, never the
    field `a.b`, so it would not mask it."""
    if isinstance(mark, str):
        return mark.split(".") == steps
    return isinstance(mark, list) and mark == steps


def _marked(prefix: list[str], names) -> bool:
    """Whether the caller marked `prefix` (or every `prefix` + `<name>`) secret, in
    either spelling of a mark."""
    marks = current_secret()

    def has(steps: list[str]) -> bool:
        return any(_names(m, steps) for m in marks)

    return has(prefix) or (bool(names) and all(has([*prefix, n]) for n in names))


# ─── where the values live ───────────────────────────────────────


def _relative_file(p: str) -> str | None:
    """The refusal for a `file` that is not absolute (`/…` or `~…`), or None."""
    if p.startswith("/") or p.startswith("~"):
        return None
    return f"file={p!r} must be an absolute path (starting with / or ~) — a relative path is refused"


class _File:
    """The default backend: one YAML document of
    `{refs: {ref: {host, fields: {field: value}}}}`, at an absolute path."""

    def __init__(self, agent):
        p = agent.record.get("file") or f"{DEFAULT_DIR}/{agent.id}.yaml"
        if why := _relative_file(p):  # a record written past `validate_meta`
            raise ValueError(f"{NAME}: {why}")
        full = Path(p).expanduser()
        self.base, self.name, self.shown = str(full.parent), full.name, p

    def load(self) -> dict:
        if not fs.exists(self.base, self.name, external=True):
            return {}
        # ONE open, judged on its own descriptor (`fs.read_private`): checking the
        # path and then reading the path again was a window a swapped leaf slipped
        # through (audit F5, 2026-10-03).
        try:
            raw = fs.read_private(self.base, self.name, external=True)
        except fs.NotPrivate as e:
            raise PermissionError(
                f"{NAME}: credential file {self.shown} must be a regular file owned "
                f"by this user with mode 0600 (is {oct(e.mode)}, uid {e.uid}) — "
                "refused, nothing read"
            ) from None
        doc = yaml_io.load(raw.decode("utf-8"))
        refs = (doc or {}).get("refs") if isinstance(doc, dict) else None
        return refs if isinstance(refs, dict) else {}

    def get(self, ref: str) -> dict | None:
        """The ref's entry at rest, `{host, fields}`."""
        v = self.load().get(ref)
        return v if isinstance(v, dict) else None

    def _save(self, refs: dict) -> None:
        # THE KERNEL'S ONE EMITTER (`kernel_state/yaml_io.py`) — the bytes the rust
        # and swift ports reproduce — every map's keys in sorted order.
        text = yaml_io.dump(_sorted({"refs": refs}))
        fs.write_text(self.base, self.name, text, external=True, mode=0o600)

    def put(self, ref: str, host: str, fields: dict) -> None:
        refs = self.load()
        refs[ref] = {"host": host, "fields": fields}
        self._save(refs)

    def drop(self, ref: str) -> None:
        refs = self.load()
        if refs.pop(ref, None) is not None:
            self._save(refs)


class _Keychain:
    """The OS keychain: one item per ref, `{host, fields}` as a JSON object."""

    def __init__(self, agent):
        try:
            import keyring  # noqa: PLC0415 — only this backend needs it
        except ImportError as e:  # pragma: no cover — a packaging fault, loud
            raise RuntimeError(
                f"{NAME}: the keychain backend needs `keyring`: {e}"
            ) from e
        self.kr = keyring
        # the keychain's own word for it is the item's "service"
        self.namespace = agent.record.get("keychain_namespace") or KEYCHAIN_NAMESPACE
        self.prefix = f"{agent.id}/"

    def get(self, ref: str) -> dict | None:
        """The ref's item, `{host, fields}`."""
        raw = self.kr.get_password(self.namespace, self.prefix + ref)
        if raw is None:
            return None
        v = json.loads(raw)
        return v if isinstance(v, dict) else None

    def put(self, ref: str, host: str, fields: dict) -> None:
        self.kr.set_password(
            self.namespace,
            self.prefix + ref,
            json.dumps({"host": host, "fields": fields}),
        )

    def drop(self, ref: str) -> None:
        if self.kr.get_password(self.namespace, self.prefix + ref) is not None:
            self.kr.delete_password(self.namespace, self.prefix + ref)


class _Env:
    """Read-only: each ref's fields come from the environment variables its record
    entry names (`env: {field: VAR}`). No value at rest, so no host at rest."""

    def __init__(self, agent):
        self.agent = agent

    def get(self, ref: str) -> dict | None:
        names = (_refs(self.agent).get(ref) or {}).get("env")
        if not isinstance(names, dict) or not names:
            return None
        out = {}
        for field, var in names.items():
            val = os.environ.get(var)
            if not val:
                raise LookupError(
                    f"{NAME}: ref {ref!r} reads {field!r} from ${var}, which is unset"
                )
            out[field] = val
        return out


def _backend(agent):
    kind = agent.record.get("backend") or "file"
    return {"file": _File, "keychain": _Keychain, "env": _Env}[kind](agent)


def _sorted(v):
    """Every map's keys in order, all the way down (PyYAML's `sort_keys=True`)."""
    if isinstance(v, dict):
        return {k: _sorted(v[k]) for k in sorted(v)}
    return v


def _host_mismatch(ref: str, bound, other) -> dict:
    """THE host-mismatch refusal: the ref is bound to `bound`, not `other`. One
    shape for a `get {ref, host}` naming another host and a value stored for
    another host — or for none (audit S3)."""
    return refusal(
        BAD_ARG,
        f"{NAME}: ref {ref!r} is bound to host {bound!r}, not {other!r} — refused, "
        "nothing read",
        arg="host",
    )


def _host_at_rest(ref: str, stored, declared: str | None) -> dict | None:
    """The refusal for a stored value whose host is not the declared one — or that
    carries none (audit S3) — or None."""
    at_rest = _host(stored)
    if at_rest is None or at_rest != declared:
        return _host_mismatch(ref, declared, at_rest)
    return None


# ─── verbs ─────────────────────────────────────────────────────────


async def _reflect(id, payload, agent):
    """Identity + posture: the backend, where the file lives, how many refs; never a value or a ref name. No args."""
    backend = agent.record.get("backend") or "file"
    out = {
        "id": id,
        "sentence": "Credential store — secrets by ref, each bound to one host, read "
        "only with a secret mark, behind per-ref read/write grants.",
        "backend": backend,
        "refs": len(_refs(agent)),
        "verbs": {
            n: (f.__doc__ or "").strip().splitlines()[0] for n, f in VERBS.items()
        },
    }
    if backend == "file":
        out["file"] = agent.record.get("file") or f"{DEFAULT_DIR}/{id}.yaml"
    return out


async def _boot(id, payload, agent):
    """Stamps `holds_secrets: true` on this store's record (no model turn may reach it); values are read from the backend when asked for."""
    if agent.record.get(HOLDS_SECRETS) is not True:
        agent.ctx.update(id, **{HOLDS_SECRETS: True})
    return None


async def _set(id, payload, agent):
    """args: ref:str, fields:{name: value} (req). Store or rotate the value of a DECLARED ref — its host is the declaration's (refs are declared by update_agent on this store's record; set never creates one and takes no host). Call it with secret: ['payload.fields']. Returns {ref, host, fields:[names], stored}."""
    ref, fields = payload.get("ref"), payload.get("fields")
    if not isinstance(ref, str) or not ref.strip():
        return missing(f"{NAME} set", "ref")
    if "host" in payload:  # the binding is the declaration's alone (audit AV-4)
        return refusal(
            BAD_ARG,
            f"{NAME} set: takes no host — a ref's host is its declaration, written "
            "by update_agent on this store's refs",
            arg="host",
        )
    if (
        not isinstance(fields, dict)
        or not fields
        or not all(
            isinstance(k, str) and k and isinstance(v, str) and v
            for k, v in fields.items()
        )
    ):
        return refusal(
            BAD_ARG,
            f"{NAME} set: fields must be a non-empty map of field name to string value",
            arg="fields",
        )
    if not _marked(["payload", "fields"], list(fields)):
        return refusal(
            BAD_ARG,
            f"{NAME} set: mark the values secret — secret: ['payload.fields'] — or "
            "they would be recorded in the ledger, the memory store and the state stream",
            arg="secret",
        )
    if (agent.record.get("backend") or "file") == "env":
        return refusal(
            BAD_ARG,
            f"{NAME} set: the env backend is read-only — set the environment variable "
            "and name it in the ref's `env`",
            arg="backend",
        )
    caller = _caller()
    refs = _refs(agent)
    entry = refs.get(ref)
    if entry is None:  # declared by update_agent alone — set never makes one
        return _no_such_ref(ref)
    if not _covers(agent, entry.get("writers"), caller):
        return (
            refusal(
                UNAUTHORIZED,
                f"{NAME}: {caller} may read {ref!r} but not write it",
                arg="ref",
            )
            if _covers(agent, entry.get("readers"), caller)
            else _no_such_ref(ref)
        )
    # the declared host is stored WITH the value (audit S3)
    _backend(agent).put(ref, _host(entry.get("host")), dict(fields))
    refs[ref] = {**entry, "fields": sorted(fields)}  # the value's NAMES, never a value
    agent.ctx.update(id, refs=refs)
    return {
        "ref": ref,
        "host": _host(entry.get("host")),
        "fields": sorted(fields),
        "stored": True,
    }


async def _get(id, payload, agent):
    """args: ref:str (req), host:str? (where you will send it — refused unless the ref is bound to it). The credential: {ref, host, fields:{name: value}}. Call it with secret: ['result']; send the value ONLY to `host`."""
    ref = payload.get("ref")
    if not isinstance(ref, str) or not ref.strip():
        return missing(f"{NAME} get", "ref")
    entry = _refs(agent).get(ref)
    if entry is None or not _covers(agent, entry.get("readers"), _caller()):
        return _no_such_ref(ref)
    if payload.get("host") is not None:
        want = _host(payload.get("host"))
        if want is None or want != _host(entry.get("host")):
            return _host_mismatch(ref, entry.get("host"), want)
    backend = _backend(agent)
    fields = backend.get(ref)
    if fields and not isinstance(backend, _Env):
        # THE HOST AT REST (audit S3): a value is read only for the host it was
        # stored with — the declaration of a store elsewhere sharing this id and
        # ref names its own host, and reads nothing.
        if bad := _host_at_rest(ref, fields.get("host"), _host(entry.get("host"))):
            return bad
        fields = fields.get("fields")
    if not isinstance(fields, dict) or not fields:
        return refusal(
            NOT_FOUND, f"{NAME}: ref {ref!r} has no value in the backend", arg="ref"
        )
    # The marks are judged against the field names the backend RETURNED, never the
    # declared ones: a declaration can name fewer than the backend holds (an `env`
    # map wider than `fields`, a re-declared ref, a hand-edited file), and a
    # returned field no mark covers would be recorded in plain (audit BV-6/S4).
    if not (_marked(["result"], None) or _marked(["result", "fields"], list(fields))):
        return refusal(
            BAD_ARG,
            f"{NAME} get: mark the answer secret — secret: ['result'] — or the value "
            "would be recorded in the ledger, the memory store and the state stream",
            arg="secret",
        )
    return {"ref": ref, "host": _host(entry.get("host")), "fields": fields}


async def _list(id, payload, agent):
    """No args. The refs YOU may read or write — metadata only: {refs:[{ref, host, fields:[names], read, write}]}. Never a value."""
    caller = _caller()
    out = []
    for ref, entry in sorted(_refs(agent).items()):
        r = _covers(agent, entry.get("readers"), caller)
        w = _covers(agent, entry.get("writers"), caller)
        if not (r or w):
            continue
        row = {
            "ref": ref,
            "host": _host(entry.get("host")),
            "fields": entry.get("fields") or sorted((entry.get("env") or {})),
            "read": r,
            "write": w,
        }
        if caller is None:
            row["readers"] = entry.get("readers") or []
            row["writers"] = entry.get("writers") or []
        out.append(row)
    return {"refs": out}


async def _forget(id, payload, agent):
    """args: ref:str (req). Drop a ref's VALUE from the backend; its declaration (host, readers, writers) stays — update_agent alone undeclares a ref. Writers and the operator only. Returns {forgotten, ref}."""
    ref = payload.get("ref")
    if not isinstance(ref, str) or not ref.strip():
        return missing(f"{NAME} forget", "ref")
    if (agent.record.get("backend") or "file") == "env":
        return refusal(
            BAD_ARG,
            f"{NAME} forget: the env backend is read-only — unset the environment "
            "variable its ref names",
            arg="backend",
        )
    refs = _refs(agent)
    entry = refs.get(ref)
    if entry is None or not _covers(agent, entry.get("writers"), _caller()):
        return _no_such_ref(ref)
    _backend(agent).drop(ref)
    if "fields" in entry:  # the value's names go with the value (audit AV-5)
        refs[ref] = {k: v for k, v in entry.items() if k != "fields"}
        agent.ctx.update(id, refs=refs)
    return {"forgotten": True, "ref": ref}


VERBS = {
    "reflect": _reflect,
    "boot": _boot,
    "set": _set,
    "get": _get,
    "list": _list,
    "forget": _forget,
}


def validate_meta(meta: dict, record: dict) -> str | None:
    """The record's own fields, refused at the write when malformed."""
    if HOLDS_SECRETS in meta and meta[HOLDS_SECRETS] is not True:
        return "holds_secrets must be true — a credential_store always holds secrets"
    b = meta.get("backend")
    if b is not None and b not in BACKENDS:
        return f"backend={b!r} is not one of {', '.join(BACKENDS)}"
    for k in ("file", "keychain_namespace"):
        if (
            k in meta
            and meta[k] is not None
            and (not isinstance(meta[k], str) or not meta[k].strip())
        ):
            return f"{k} must be a non-empty string"
    if meta.get("file") is not None and (why := _relative_file(meta["file"])):
        return why
    refs = meta.get("refs")
    if refs is not None:
        if not isinstance(refs, dict):
            return "refs must be a map of ref to {readers, writers, host, fields, env}"
        for ref, e in refs.items():
            if not isinstance(ref, str) or not ref.strip():
                return "a ref must be a non-empty string"
            if not isinstance(e, dict):
                return f"refs[{ref!r}] must be a map"
            for k in ("readers", "writers", "fields"):
                v = e.get(k)
                if v is not None and not (
                    isinstance(v, list) and all(isinstance(x, str) and x for x in v)
                ):
                    return f"refs[{ref!r}].{k} must be a list of strings"
            if e.get("host") is None:
                return (
                    f"refs[{ref!r}].host is required — the one host this ref's value "
                    "may be sent to"
                )
            if _host(e.get("host")) is None:
                return f"refs[{ref!r}].host must be a non-empty host"
            env = e.get("env")
            if env is not None and not (
                isinstance(env, dict)
                and all(
                    isinstance(k, str) and isinstance(v, str) and v
                    for k, v in env.items()
                )
            ):
                return f"refs[{ref!r}].env must map field names to variable names"
    return None


async def handler(id: str, payload: dict, agent) -> dict | None:
    t = payload.get("type")
    fn = VERBS.get(t)
    if fn is None:
        return refusal(UNHANDLED, f"{NAME}: unknown type {t!r}", verb=t)
    return await fn(id, payload, agent)
