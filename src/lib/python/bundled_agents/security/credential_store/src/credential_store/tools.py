"""credential_store — secrets an agent USES but never keeps (owner, 2026-10-01).

A credential is addressed by a REF (`openai/main`) and is bound to ONE HOST: a value
plus the host it may be sent to. A bundle that needs one names it on its own record
(`credentials: {store, ref}`), reads it at USE time and refuses to send it to any
other host — so rotating a key is rewriting the ref, and no restart is ever needed.

WHERE THE VALUES LIVE — `backend` on the record, one of:
  * `file` (the default): a YAML file in the user's home,
    `~/.fantastic/credentials/<store id>.yaml` unless `file` names another path. It
    is written 0600 and READ ONLY IF it is a regular file owned by this user with
    no group/other bits — anything else is refused, loud.
  * `keychain`: the OS keychain (macOS Keychain, Secret Service, Windows Credential
    Locker), one item per ref — service `keychain_service` (default `fantastic`),
    account `<store id>/<ref>`. The explicit choice for the strongest at-rest
    protection.
  * `env`: read-only; each ref maps its fields to environment variables
    (`refs: {ref: {host, env: {api_key: VAR}}}`), for a container or CI.

WHO MAY DO WHAT — per ref, on the store's own record (the OPERATOR manages it with
`update_agent`): `refs: {<ref>: {readers: [...], writers: [...], host, fields}}`.
An entry is a local agent id, `$subtree:<id>` (that agent and every live descendant),
or a qualified `<guid>|<agent>` caller on another kernel. The operator may do
everything. An agent listed in `creators` may create a NEW ref, which it then reads
and writes. A caller with no grant on a ref is answered "no such ref" — never told
the ref exists.

NO VALUE IS EVER RECORDED. `set` must be called with `secret: ['payload.fields']`
and `get` with `secret: ['result']` (`kernel/_secret.py`): the call is refused
otherwise, because the value would land in the ledger, the memory store and the
state stream. The record holds only metadata — the host and the field NAMES.

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
import stat
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
KEYCHAIN_SERVICE = "fantastic"
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


def _marked(prefix: str, names) -> bool:
    """Whether the caller marked `prefix` (or every `prefix.<name>`) secret."""
    paths = current_secret()
    return prefix in paths or (
        bool(names) and all(f"{prefix}.{n}" in paths for n in names)
    )


# ─── where the values live ───────────────────────────────────────


class _File:
    """The default backend: one YAML document of `{refs: {ref: {field: value}}}`."""

    def __init__(self, agent):
        p = agent.record.get("file") or f"{DEFAULT_DIR}/{agent.id}.yaml"
        if p.startswith("~") or p.startswith("/"):
            full = Path(p).expanduser()
            self.base, self.name, self.external = str(full.parent), full.name, True
        else:  # relative: under the kernel's running dir, clamped
            rel = Path(p)
            self.base = fs.Root(agent.ctx.workdir, str(rel.parent))
            self.name, self.external = rel.name, False
        self.shown = p

    def _check(self) -> None:
        st = fs.lstat(self.base, self.name, external=self.external)
        mode = stat.S_IMODE(st.st_mode)
        if not stat.S_ISREG(st.st_mode) or st.st_uid != os.getuid() or mode & 0o077:
            raise PermissionError(
                f"{NAME}: credential file {self.shown} must be a regular file owned "
                f"by this user with mode 0600 (is {oct(mode)}, uid {st.st_uid}) — "
                "refused, nothing read"
            )

    def load(self) -> dict:
        if not fs.exists(self.base, self.name, external=self.external):
            return {}
        self._check()
        doc = yaml_io.load(fs.read_text(self.base, self.name, external=self.external))
        refs = (doc or {}).get("refs") if isinstance(doc, dict) else None
        return refs if isinstance(refs, dict) else {}

    def get(self, ref: str) -> dict | None:
        v = self.load().get(ref)
        return v if isinstance(v, dict) else None

    def _save(self, refs: dict) -> None:
        # THE KERNEL'S ONE EMITTER (`kernel_state/yaml_io.py`) — the bytes the rust
        # and swift ports reproduce — over refs and fields in sorted order.
        text = yaml_io.dump(
            {"refs": {r: dict(sorted(refs[r].items())) for r in sorted(refs)}}
        )
        fs.write_text(self.base, self.name, text, external=self.external, mode=0o600)

    def put(self, ref: str, fields: dict) -> None:
        refs = self.load()
        refs[ref] = fields
        self._save(refs)

    def drop(self, ref: str) -> None:
        refs = self.load()
        if refs.pop(ref, None) is not None:
            self._save(refs)


class _Keychain:
    """The OS keychain: one item per ref, the fields as a JSON object."""

    def __init__(self, agent):
        try:
            import keyring  # noqa: PLC0415 — only this backend needs it
        except ImportError as e:  # pragma: no cover — a packaging fault, loud
            raise RuntimeError(
                f"{NAME}: the keychain backend needs `keyring`: {e}"
            ) from e
        self.kr = keyring
        self.service = agent.record.get("keychain_service") or KEYCHAIN_SERVICE
        self.prefix = f"{agent.id}/"

    def get(self, ref: str) -> dict | None:
        raw = self.kr.get_password(self.service, self.prefix + ref)
        if raw is None:
            return None
        v = json.loads(raw)
        return v if isinstance(v, dict) else None

    def put(self, ref: str, fields: dict) -> None:
        self.kr.set_password(self.service, self.prefix + ref, json.dumps(fields))

    def drop(self, ref: str) -> None:
        if self.kr.get_password(self.service, self.prefix + ref) is not None:
            self.kr.delete_password(self.service, self.prefix + ref)


class _Env:
    """Read-only: each ref's fields come from the environment variables its record
    entry names (`env: {field: VAR}`)."""

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
    """args: ref:str, host:str (where the value may be sent), fields:{name: value} (req). Store or rotate a credential. Call it with secret: ['payload.fields']. Returns {ref, host, fields:[names], stored}."""
    ref, fields = payload.get("ref"), payload.get("fields")
    if not isinstance(ref, str) or not ref.strip():
        return missing(f"{NAME} set", "ref")
    host = _host(payload.get("host"))
    if host is None:
        return missing(f"{NAME} set", "host")
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
    if not _marked("payload.fields", list(fields)):
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
    if entry is None:
        creators = agent.record.get("creators") or []
        if caller is not None and not _covers(agent, creators, caller):
            return _no_such_ref(ref)
        owner = [caller] if caller is not None else []
        entry = {"readers": owner, "writers": list(owner)}
    elif not _covers(agent, entry.get("writers"), caller):
        return (
            refusal(
                UNAUTHORIZED,
                f"{NAME}: {caller} may read {ref!r} but not write it",
                arg="ref",
            )
            if _covers(agent, entry.get("readers"), caller)
            else _no_such_ref(ref)
        )
    _backend(agent).put(ref, dict(fields))
    refs[ref] = {**entry, "host": host, "fields": sorted(fields)}
    agent.ctx.update(id, refs=refs)
    return {"ref": ref, "host": host, "fields": sorted(fields), "stored": True}


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
            return refusal(
                BAD_ARG,
                f"{NAME}: ref {ref!r} is bound to host {entry.get('host')!r}, not "
                f"{want!r} — refused, nothing read",
                arg="host",
            )
    names = entry.get("fields") or list((entry.get("env") or {}).keys())
    if not (_marked("result", None) or _marked("result.fields", names)):
        return refusal(
            BAD_ARG,
            f"{NAME} get: mark the answer secret — secret: ['result'] — or the value "
            "would be recorded in the ledger, the memory store and the state stream",
            arg="secret",
        )
    fields = _backend(agent).get(ref)
    if not fields:
        return refusal(
            NOT_FOUND, f"{NAME}: ref {ref!r} has no value in the backend", arg="ref"
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
    """args: ref:str (req). Drop a credential — its value and its grants. Writers and the operator only. Returns {forgotten, ref}."""
    ref = payload.get("ref")
    if not isinstance(ref, str) or not ref.strip():
        return missing(f"{NAME} forget", "ref")
    refs = _refs(agent)
    entry = refs.get(ref)
    caller = _caller()
    if entry is None or not _covers(agent, entry.get("writers"), caller):
        return _no_such_ref(ref)
    if (agent.record.get("backend") or "file") != "env":
        _backend(agent).drop(ref)
    refs.pop(ref)
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
    for k in ("file", "keychain_service"):
        if (
            k in meta
            and meta[k] is not None
            and (not isinstance(meta[k], str) or not meta[k].strip())
        ):
            return f"{k} must be a non-empty string"
    if (
        "creators" in meta
        and meta["creators"] is not None
        and not (
            isinstance(meta["creators"], list)
            and all(isinstance(x, str) and x for x in meta["creators"])
        )
    ):
        return "creators must be a list of agent ids"
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
            if e.get("host") is not None and _host(e.get("host")) is None:
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
