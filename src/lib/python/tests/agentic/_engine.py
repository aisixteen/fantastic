"""The engine behind the manifest-driven suite — hydrate, dehydrate, reach.

This suite is the OPPOSITE of the emergence harness. There, an LLM discovers a
running system at runtime and we assert what it managed to build. Here a coding
agent AUTHORS a kernel shape up front in a yaml config, the engine boots exactly
that shape, and the suite asserts the RUNNING system matches. Nothing emerges;
nothing is discovered; no model is called.

Three general pieces, one discipline:

- `hydrate(config, tmp_path)` boots a kernel FROM a config through the real
  `static_loader` create path. It does not reimplement `create_agent`, and it
  does not judge — it returns RECEIPTS (the create reply, the explicit boot
  reply) so a caller can assert the outcome the config declared.
- `dehydrate(kernel)` reads the running system back — the reflect tree, flat,
  plus the on-disk records when asked. Medium-agnostic, no assertions.
- `reach(kernel, sender, target, payload)` is THE primitive: an AGENT-ORIGIN
  send, returning the raw reply.

**Assert reachability, never a config field.** A field check stops tracking the
property the moment a second gate exists: an agent can carry
`ingress_rule=allow_all_ingress` and still be unreachable (no `callable_by`), and
an agent whose `can_call` names a target is still refused when the target's
`callable_by` does not name it back — measured, and the reply says
`side: "ingress"`. So every claim about wiring is made by ATTEMPTING the
operation through `reach()` and asserting what the caller actually gets.

**The hydrator re-sends `boot`, and that is not belt-and-braces.**
`_verb_create_agent` boots the child it just made, DISCARDS the reply and
swallows the exception (`kernel/_agent.py:1231-1236`), while `static_loader._boot`
never raises — it RETURNS `{"error": …, "booted": False}`. A manifest with a bad
entry therefore yields a clean-looking create reply, a silently PARTIAL roster
(entries before the bad one exist, entries after it never do), and no complaint
anywhere. The explicit `send(loader, {"type": "boot"})` is the only channel on
which that truth surfaces.

Two deliberate deviations from the `fantastic init` path
(`kernel/modes/dispatch.py:86-133`), both for determinism:

- the ROOT's own `boot` is NOT sent. It starts the debounced flush loop
  (`kernel_state/tools.py:694-698`, 0.15s), which would leave a background task
  running past the test. `_testkit.persist(root)` is the synchronous stand-in,
  exactly as every other disk test in this repo does it.
- no `FantasticLock`, no argv, no `sys.exit` — the whole init path is plain
  `kernel.send`, so the two-send sequence is strictly better in-process.
"""

from __future__ import annotations

import asyncio
import re
import time
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

import pytest
import yaml
from kernel_state.tools import read_tree

from _testkit import boot_root, persist
from kernel import sender_context

# THE SHARED CORPUS, at the repo root. The rust and swift runners read these
# same files: a scenario is DATA, and the runner is the only per-runtime part.
# The corpus lives at the REPO ROOT, shared with the rust and swift runners —
# it is not this runner's private data.
CONTRACT_DIR = Path(__file__).resolve().parents[5] / "contract"


# ─── requirement registry (a2a-tck's ID discipline) ─────────────
#
# Stable ids, one table, duplicate detection AT IMPORT. The point is not
# bookkeeping: it is that a hand-written validator can name the invariant it is
# pinning (`check("C23", …)`) and that the name cannot silently come to mean two
# things. MUST asserts; SHOULD/MAY xfail, so a known-soft expectation reports as
# a coverage hole instead of a red build.


@dataclass(frozen=True)
class Requirement:
    """One named invariant this suite can pin. `level` decides what a failure
    DOES: MUST raises, SHOULD/MAY xfail (a2a-tck's mapping)."""

    id: str
    level: str
    title: str


# The registry is DATA: `contract/requirements.yaml`, read by every runtime's
# runner. It used to be a python tuple in this file, which made it unreadable to
# the rust and swift runners that must pin the SAME ids.
REQUIREMENTS_FILE = CONTRACT_DIR / "requirements.yaml"


_LEVELS = frozenset({"MUST", "SHOULD", "MAY"})


def _index(rows) -> dict[str, Requirement]:
    """Build the id→Requirement table, refusing a duplicate id or an unknown
    level AT LOAD. A duplicate id is the one failure mode that makes the whole
    registry worthless — two validators would pin different things under one
    name and the coverage matrix would read as covered either way."""
    out: dict[str, Requirement] = {}
    for rid, level, title in rows:
        if rid in out:
            raise ValueError(
                f"duplicate requirement id {rid!r}: {out[rid].title!r} vs {title!r}"
            )
        if level not in _LEVELS:
            raise ValueError(f"{rid}: level {level!r} not in {sorted(_LEVELS)}")
        out[rid] = Requirement(rid, level, title)
    return out


def _load_requirements() -> tuple[dict[str, Requirement], frozenset[str]]:
    doc = yaml.safe_load(REQUIREMENTS_FILE.read_text()) or {}
    rows = tuple(
        (r["id"], r["level"], r["title"]) for r in (doc.get("requirements") or [])
    )
    return _index(rows), frozenset(doc.get("core") or ())


REQUIREMENTS, CORE_REQUIREMENTS = _load_requirements()


# id → how many times a check was made against it in THIS process. The coverage
# matrix a2a-tck keeps: an id with 0 checks is a requirement nobody pins.
EXERCISED: dict[str, int] = {}


def requirement(rid: str) -> Requirement:
    """Look one up. An unknown id raises HERE, naming the id — a typo in a
    validator's tag must not read as a silently untagged assertion."""
    try:
        return REQUIREMENTS[rid]
    except KeyError:
        raise KeyError(
            f"unknown requirement id {rid!r} — known: {sorted(REQUIREMENTS)}"
        ) from None


def because(rid: str, detail: str = "") -> str:
    """The assertion message for `rid`, for a plain `assert x, because(...)`.
    Records the check in the coverage matrix."""
    req = requirement(rid)
    EXERCISED[rid] = EXERCISED.get(rid, 0) + 1
    msg = f"{req.id} [{req.level}] {req.title}"
    return f"{msg}\n  {detail}" if detail else msg


def check(rid: str, condition: object, detail: str = "") -> None:
    """Tag one assertion with the requirement it pins. MUST raises on failure;
    SHOULD/MAY xfail, so a soft expectation shows up as a coverage hole rather
    than a red build (a2a-tck's mapping)."""
    msg = because(rid, detail)
    if condition:
        return
    if requirement(rid).level == "MUST":
        raise AssertionError(msg)
    pytest.xfail(msg)


def coverage_matrix() -> dict[str, dict[str, Any]]:
    """Every registered requirement and how many checks pinned it this run."""
    return {
        rid: {"level": r.level, "title": r.title, "checks": EXERCISED.get(rid, 0)}
        for rid, r in REQUIREMENTS.items()
    }


# ─── the config format ──────────────────────────────────────────
#
# One yaml per scenario (openfga's sample-stores layout: the declaration and the
# assertions live in the SAME file, discovery is a glob). See configs/README.md
# for the full grammar. Parsing is STRICT — an unknown key raises. That is a
# deliberate inversion of `static_loader`, which silently drops every top-level
# key but `agents:` and never objects to a misspelled meta; a contract file that
# quietly ignores half of what you wrote is the exact failure this suite exists
# to catch.

CONFIG_DIR = CONTRACT_DIR / "agentic" / "configs"
# Specs are the suite's index: a config is a deployable kernel document and does
# not know it is being tested.
SPEC_DIR = CONTRACT_DIR / "agentic" / "specs"

HYDRATIONS = frozenset({"ok", "refused_at_write", "boot_error"})

# TWO FILES, TWO VOCABULARIES. A kernel config declares a SYSTEM and must be
# deployable as written — no test metadata may leak into it, or the thing under
# test stops being the thing you would ship. A spec declares what this suite
# ASSERTS about that system, and names its config.
# A config declares LOADERS and nothing else. There was a `prelude:`/`overlay:` pair
# for operator ops that ran either side of them, and its ONLY real use was standing
# up the persistence provider — because `_find_store` scanned one level and a
# manifest-declared store lands a grandchild, so it was declarable but never
# discovered. `_find_store` walks the subtree now, so the store is an ordinary roster
# entry and there is nothing left for a layering step to do.
_CONFIG_KEYS = frozenset({"loaders"})
_SPEC_KEYS = frozenset({"config", "name", "description", "requirements", "expect"})
_LOADER_KEYS = frozenset(
    {"id", "file", "manifest", "manifest_text", "missing_manifest", "meta"}
)
_EXPECT_KEYS = frozenset(
    {"hydration", "error_contains", "reboot", "steps", "closed_roster_sender"}
)
# One step = exactly one verb key + optional `expect`/`because`/`name`.
_STEP_VERBS = frozenset(
    {
        "reach",  # send AS an agent — gated; the reachability primitive
        "operator",  # send with OPERATOR authority — setup, and side-effect reads
        "grant",  # sugar: operator update_agent on can_call / callable_by
        "revoke",  # sugar: the same, removing an entry
        "reboot",  # persist + rehydrate
        "assert_tree",  # the dehydrated shape
        "start",  # fire a send WITHOUT awaiting it, bound to a name
        "await_task",  # block until a bound send is visible in `ps`
        "collect",  # await a bound send and assert its Outcome
        # COMPUTED SWEEPS — derived from the manifest, not hand-listed. A hand
        # written probe list can only confirm the edges its author thought of; these
        # walk what the config actually declares, so a grant somebody added and
        # forgot to assert is still probed.
        "reconcile_grants",  # every declared (sender,target,verb) triple
        "sweep_undeclared",  # every ordered pair: nothing rides along
    }
)
_STEP_META = frozenset({"expect", "because", "name"})
_EXPECT_MATCHERS = frozenset(
    {
        "allow",
        "side",
        "reason",
        "status",
        "contains",
        "not_contains",
        "error_contains",
    }
)

# `<bundle>_<6 hex>` — the shape `Agent.create` mints when a spec carries no id
# (`kernel/_agent.py:885-892`). An id-less manifest entry duplicates on EVERY
# boot, so this shape appearing where it did not before IS the duplicate.
MINTED_ID = re.compile(r"^[A-Za-z0-9_.]+_[0-9a-f]{6}$")


class ConfigError(ValueError):
    """A config file this engine refuses to run."""


class HydrationError(RuntimeError):
    """A hydration whose outcome contradicts what its config declared."""


@dataclass(frozen=True)
class LoaderSpec:
    """One `static_loader` agent and the manifest it hydrates. Exactly one of
    `manifest` / `manifest_text` / `missing_manifest` is set — a mapping written
    out as yaml, a verbatim string (for the malformed-yaml cases), or nothing at
    all written, so `validate_meta` refuses the loader at write time."""

    id: str
    file: str
    manifest: dict | None = None
    manifest_text: str | None = None
    missing_manifest: bool = False
    meta: dict = field(default_factory=dict)

    @property
    def declared_agents(self) -> list[dict]:
        """The roster entries this loader is expected to build. Empty when the
        manifest is raw text or absent — the shape is then not declarable and a
        shape check must say so rather than assert on nothing."""
        if not isinstance(self.manifest, dict):
            return []
        return [a for a in self.manifest.get("agents", []) if isinstance(a, dict)]

    @property
    def declares_roster(self) -> bool:
        return isinstance(self.manifest, dict)


@dataclass(frozen=True)
class Step:
    """ONE ordered action and the verdict it must produce.

    A flat edge list could state a verdict but never a CAUSE. The two-sided gate
    is the case that forced this: opening one grant half at a time and watching
    the denial MOVE from `egress` to `ingress` is what proves the second half was
    refusing independently the whole time — invisible behind the first. That is a
    sequence, not a table.

    `kind` is the verb; `args` its payload; `expect` the matchers. `because` tags
    the requirement id this step pins, so the coverage matrix survives the move
    out of python.
    """

    kind: str
    args: dict
    expect: dict = field(default_factory=dict)
    because: str = ""
    name: str = ""

    def __str__(self) -> str:
        if self.name:
            return self.name
        if self.kind in ("reach", "start"):
            return (
                f"{self.args.get('from')} → {self.args.get('to')} "
                f"{(self.args.get('send') or {}).get('type')!r}"
            )
        return f"{self.kind} {self.args}"


@dataclass(frozen=True)
class Expectation:
    hydration: str = "ok"
    error_contains: str = ""
    reboot: bool = True
    steps: tuple[Step, ...] = ()
    closed_roster_sender: str | None = None


@dataclass(frozen=True)
class Config:
    path: Path
    name: str
    description: str = ""
    requirements: tuple[str, ...] = ()
    loaders: tuple[LoaderSpec, ...] = ()
    expect: Expectation = Expectation()

    @property
    def loader_ids(self) -> list[str]:
        return [ln.id for ln in self.loaders]

    @property
    def roster_ids(self) -> list[str]:
        """Every id the manifests declare, in declaration order."""
        return [
            a["id"] for ln in self.loaders for a in ln.declared_agents if a.get("id")
        ]

    def expected_children(self, parent_id: str) -> set[str]:
        """Who this parent must own once hydration finishes: the manifest roster."""
        return {
            a["id"]
            for ln in self.loaders
            if ln.id == parent_id
            for a in ln.declared_agents
            if a.get("id")
        }


def _reject_unknown(where: str, got, allowed: frozenset[str]) -> dict:
    if not isinstance(got, dict):
        raise ConfigError(f"{where}: must be a mapping, got {type(got).__name__}")
    extra = set(got) - allowed
    if extra:
        raise ConfigError(
            f"{where}: unknown key(s) {sorted(extra)}; known {sorted(allowed)}"
        )
    return got


def _parse_steps(raw) -> tuple[Step, ...]:
    """Parse `expect.steps`. STRICT: exactly one verb per step, known matchers
    only. A typo'd verb must fail at load, not read as a step that did nothing."""
    if raw is None:
        return ()
    if not isinstance(raw, list):
        raise ConfigError("expect.steps: must be a list")
    steps: list[Step] = []
    for i, item in enumerate(raw):
        where = f"expect.steps[{i}]"
        if not isinstance(item, dict):
            raise ConfigError(f"{where}: must be a mapping")
        verbs = sorted(set(item) & _STEP_VERBS)
        extra = set(item) - _STEP_VERBS - _STEP_META
        if extra:
            raise ConfigError(
                f"{where}: unknown key(s) {sorted(extra)};"
                f" verbs {sorted(_STEP_VERBS)}, meta {sorted(_STEP_META)}"
            )
        if len(verbs) != 1:
            raise ConfigError(f"{where}: needs exactly ONE verb, got {verbs or 'none'}")
        kind = verbs[0]
        args = item[kind]
        if args is None:
            args = {}
        if not isinstance(args, dict):
            raise ConfigError(f"{where}: `{kind}:` must be a mapping")
        exp = item.get("expect") or {}
        if not isinstance(exp, dict):
            raise ConfigError(f"{where}: `expect:` must be a mapping")
        bad = set(exp) - _EXPECT_MATCHERS
        if bad:
            raise ConfigError(
                f"{where}.expect: unknown matcher(s) {sorted(bad)};"
                f" known {sorted(_EXPECT_MATCHERS)}"
            )
        if exp.get("allow") is True and exp.get("side") is not None and "side" in exp:
            raise ConfigError(f"{where}: `side:` only means something on a denial")
        if kind in ("reach", "start"):
            for req in ("from", "to", "send"):
                if req not in args:
                    raise ConfigError(f"{where}: `{kind}:` needs `{req}:`")
        if kind == "operator" and "to" not in args:
            raise ConfigError(f"{where}: `operator:` needs `to:`")
        if kind in ("grant", "revoke"):
            for req in ("agent", "field", "to"):
                if req not in args:
                    raise ConfigError(f"{where}: `{kind}:` needs `{req}:`")
            if args["field"] not in ("can_call", "callable_by"):
                raise ConfigError(
                    f"{where}: `field:` is can_call or callable_by,"
                    f" got {args['field']!r}"
                )
        if kind in ("await_task", "collect") and "name" not in args:
            raise ConfigError(f"{where}: `{kind}:` needs `name:` (a bound send)")
        rid = item.get("because", "")
        if rid:
            requirement(rid)  # a typo'd tag fails at LOAD, not at report time
        steps.append(
            Step(
                kind=kind,
                args=dict(args),
                expect=dict(exp),
                because=rid,
                name=item.get("name", ""),
            )
        )
    return tuple(steps)


def _parse_loaders(raw) -> tuple[LoaderSpec, ...]:
    if not isinstance(raw, list) or not raw:
        raise ConfigError("loaders: must be a non-empty list")
    out: list[LoaderSpec] = []
    seen: set[str] = set()
    for i, item in enumerate(raw):
        item = _reject_unknown(f"loaders[{i}]", item, _LOADER_KEYS)
        lid = item.get("id")
        if not isinstance(lid, str) or not lid:
            raise ConfigError(
                f"loaders[{i}]: `id:` is required (it is the loader's agent id)"
            )
        if lid in seen:
            raise ConfigError(f"loaders[{i}]: duplicate loader id {lid!r}")
        seen.add(lid)
        picked = [
            k for k in ("manifest", "manifest_text", "missing_manifest") if k in item
        ]
        if len(picked) != 1:
            raise ConfigError(
                f"loaders[{i}]: needs exactly one of `manifest:` / `manifest_text:` /"
                f" `missing_manifest: true`, got {picked}"
            )
        if "missing_manifest" in item and item["missing_manifest"] is not True:
            raise ConfigError(f"loaders[{i}]: `missing_manifest:` is only ever `true`")
        out.append(
            LoaderSpec(
                id=lid,
                file=item.get("file") or f"{lid}.yaml",
                manifest=item.get("manifest"),
                manifest_text=item.get("manifest_text"),
                missing_manifest=bool(item.get("missing_manifest")),
                meta=dict(item.get("meta") or {}),
            )
        )
    return tuple(out)


# THIS RUNTIME'S root id. `$root` anywhere in a config or spec means exactly
# this — python's root is `kernel_state`, rust's and swift's is `core`. A grant
# or a target that named one runtime's root by literal id could never be read by
# the others, so the corpus names it abstractly and each runner substitutes.
ROOT_ID = "kernel_state"


def _subst_root(value):
    if isinstance(value, str):
        return ROOT_ID if value == "$root" else value
    if isinstance(value, dict):
        return {_subst_root(k): _subst_root(v) for k, v in value.items()}
    if isinstance(value, list):
        return [_subst_root(v) for v in value]
    return value


def load_config(spec_path: str | Path) -> Config:
    """Parse one scenario: a SPEC file plus the kernel CONFIG it names.

    STRICT both ways — an unknown key in either file raises, and a test key
    appearing in a config (or a system key in a spec) is exactly the mixing this
    split exists to prevent, so it surfaces as an unknown-key error naming the
    file it was found in.
    """
    spec_path = Path(spec_path)
    spec_raw = yaml.safe_load(spec_path.read_text()) or {}
    spec_raw = _reject_unknown(str(spec_path), spec_raw, _SPEC_KEYS)

    cfg_name = spec_raw.get("config")
    if not cfg_name:
        raise ConfigError(
            f"{spec_path}: `config:` is required — name the kernel config"
        )
    cfg_path = spec_path.parent.parent / "configs" / str(cfg_name)
    if not cfg_path.exists():
        raise ConfigError(f"{spec_path}: config {cfg_name!r} not found at {cfg_path}")
    cfg_raw = yaml.safe_load(cfg_path.read_text()) or {}
    cfg_raw = _reject_unknown(str(cfg_path), cfg_raw, _CONFIG_KEYS)

    # One runtime object; the split is on disk, not in the validators' hands.
    raw = _subst_root({**cfg_raw, **spec_raw})
    raw.pop("config", None)
    path = spec_path
    reqs = tuple(raw.get("requirements") or ())
    for rid in reqs:
        requirement(rid)  # a typo'd tag fails at load, not silently at report time
        if rid in CORE_REQUIREMENTS:
            raise ConfigError(
                f"{path}: `requirements:` names {rid!r}, which the general runner"
                " pins on EVERY config — list only what this scenario's own"
                f" validator pins (core: {sorted(CORE_REQUIREMENTS)})"
            )
    exp_raw = _reject_unknown(f"{path}: expect", raw.get("expect") or {}, _EXPECT_KEYS)
    hydration = exp_raw.get("hydration", "ok")
    if hydration not in HYDRATIONS:
        raise ConfigError(
            f"{path}: expect.hydration {hydration!r} not in {sorted(HYDRATIONS)}"
        )
    if hydration != "ok" and not exp_raw.get("error_contains"):
        raise ConfigError(
            f"{path}: expect.hydration={hydration!r} must name the substring the"
            " refusal"
            " carries (`error_contains:`) — a failure that is not pinned to its message"
            " passes for the wrong reason"
        )
    return Config(
        path=path,
        name=raw.get("name") or path.stem,
        description=raw.get("description", ""),
        requirements=reqs,
        loaders=_parse_loaders(raw.get("loaders")),
        expect=Expectation(
            hydration=hydration,
            error_contains=exp_raw.get("error_contains", ""),
            reboot=bool(exp_raw.get("reboot", True)),
            steps=_parse_steps(exp_raw.get("steps")),
            closed_roster_sender=exp_raw.get("closed_roster_sender"),
        ),
    )


def config_paths(directory: Path | None = None) -> list[Path]:
    """Every scenario SPEC, sorted. Discovery is a glob — adding a scenario is
    adding a file (openfga's sample-stores model).

    Specs, not configs: a config is a deployable kernel document and says nothing
    about being tested, so the suite is enumerated by what ASSERTS, and each spec
    names the config it runs against.
    """
    d = directory or SPEC_DIR
    return sorted(p for p in d.glob("*.yaml"))


# ─── hydrate: boot a kernel FROM the config ─────────────────────


@dataclass
class Hydration:
    """A booted kernel plus the RECEIPTS of how it was booted.

    Receipts, not verdicts: `creates` is what `create_agent` replied for each
    loader (the only loud channel for a write-time refusal) and `boots` is what
    an explicit `boot` replied (the only loud channel for a manifest that parses
    but cannot build). A caller asserts against them; the engine does not."""

    config: Config
    kernel: Any  # the root Agent (id "kernel_state")
    cwd: Path
    creates: dict[str, dict]  # loader id → create_agent reply
    boots: dict[str, dict]  # agent id → explicit boot reply

    @property
    def agents(self) -> dict:
        return self.kernel.ctx.agents

    @property
    def ids(self) -> list[str]:
        return sorted(self.kernel.ctx.agents)

    def loader_boot(self, loader_id: str) -> dict:
        return self.boots.get(loader_id, {})

    def created_loaders(self) -> list[str]:
        """The loaders that actually became agents (a write-time refusal means
        the id is absent from the roster entirely)."""
        return [lid for lid in self.config.loader_ids if lid in self.kernel.ctx.agents]

    def outcome(self) -> str:
        """What this hydration ACTUALLY did, in the config's own vocabulary."""
        for lid in self.config.loader_ids:
            rec = self.creates.get(lid) or {}
            if not rec.get("id"):
                return "refused_at_write"
        for lid in self.config.loader_ids:
            if (self.boots.get(lid) or {}).get("booted") is not True:
                return "boot_error"
        return "ok"

    def outcome_error(self) -> str:
        """The message the refusal carried, whichever channel refused."""
        for lid in self.config.loader_ids:
            rec = self.creates.get(lid) or {}
            if not rec.get("id"):
                return str(rec.get("error", ""))
        for lid in self.config.loader_ids:
            boot = self.boots.get(lid) or {}
            if boot.get("booted") is not True:
                return str(boot.get("error", ""))
        return ""

    def require_declared_outcome(self) -> "Hydration":
        """Raise unless the hydration did what the config declared. A scenario
        validator calls this so it can never run its assertions against a
        half-built kernel — the exact trap `create_agent`'s swallowed boot sets."""
        want = self.config.expect.hydration
        got = self.outcome()
        if got != want:
            raise HydrationError(
                f"{self.config.name}: declared hydration {want!r}, got {got!r} — "
                f"creates={self.creates!r} boots={self.boots!r}"
            )
        return self


def _write_manifests(cfg: Config, cwd: Path) -> None:
    """Materialize each loader's manifest in the cwd.

    It has to be the cwd and it has to be relative: `static_loader` reads through
    `fs.read_text(Path.cwd(), path)` (`static_loader/tools.py:52`), clamped to
    cwd by the running-dir law — so a config living in `configs/` can NEVER be
    handed over by absolute path, and `fs.resolve` silently reinterprets a
    leading `/` as cwd-relative rather than refusing it."""
    for ln in cfg.loaders:
        if ln.missing_manifest:
            continue
        text = (
            ln.manifest_text
            if ln.manifest_text is not None
            else yaml.safe_dump(ln.manifest, sort_keys=False)
        )
        (cwd / ln.file).write_text(text)


async def hydrate(config: str | Path | Config, tmp_path: str | Path) -> Hydration:
    """Boot a kernel from a scenario config, in `tmp_path`, and report receipts.

    The caller must ALREADY be chdir'd into `tmp_path` (the `agentic_cwd`
    fixture does it) — the manifest read is clamped to `Path.cwd()`.

    Sequence, all in-process, no subprocess and no lock:
      1. write each loader's manifest into the cwd
      2. `boot_root()` — the production bootstrap, minus the flush loop
      3. per loader: `create_agent parent_id=<root> handler_module=static_loader.tools` — the
         real init path, which hydrates the roster inside its swallowed boot
      4. per created loader: an EXPLICIT `boot`, the only channel on which a
         manifest that parses but cannot build says so

    Nothing is layered on top. Every agent a scenario needs — the persistence
    provider included — is a roster entry, so the config IS the system.
    """
    cfg = config if isinstance(config, Config) else load_config(config)
    cwd, want = Path.cwd().resolve(), Path(tmp_path).resolve()
    if cwd != want:
        raise HydrationError(
            f"hydrate must run inside {want} (cwd is {cwd}) — the manifest read is"
            " clamped to the cwd by the running-dir law"
        )
    _write_manifests(cfg, cwd)
    kernel = boot_root()

    creates: dict[str, dict] = {}
    boots: dict[str, dict] = {}
    for ln in cfg.loaders:
        reply = (
            await kernel.send(
                [
                    {
                        "target": kernel.id,
                        "payload": {
                            "type": "create_agent",
                            "parent_id": kernel.id,
                            "handler_module": "static_loader.tools",
                            "id": ln.id,
                            "manifest": ln.file,
                            **ln.meta,
                        },
                    }
                ]
            )
        )[0]["result"]
        creates[ln.id] = reply if isinstance(reply, dict) else {}
        if creates[ln.id].get("id"):
            boot = (
                await kernel.send([{"target": ln.id, "payload": {"type": "boot"}}])
            )[0]["result"]
            boots[ln.id] = boot if isinstance(boot, dict) else {}

    return Hydration(config=cfg, kernel=kernel, cwd=cwd, creates=creates, boots=boots)


async def reboot(h: Hydration) -> Hydration:
    """Persist the live tree, drop the kernel, bootstrap the same dir again and
    run the production boot pass over it.

    `Kernel.load` fires NO boot (`kernel/_kernel.py:520-522`) — the bootstrap
    owns that — so the pass mirrors `longrun._boot_all_agents`. It skips the
    ROOT deliberately: the root's boot starts the debounced flush loop, and a
    background task outliving the test is not determinism."""
    persist(h.kernel)
    k2 = boot_root()
    boots: dict[str, dict] = {}
    for agent in list(k2.ctx.agents.values()):
        if agent.parent is None:
            continue
        reply = (await k2.send([{"target": agent.id, "payload": {"type": "boot"}}]))[0][
            "result"
        ]
        boots[agent.id] = reply if isinstance(reply, dict) else {}
    return replace(h, kernel=k2, boots=boots)


# ─── dehydrate: read the RUNNING system back ────────────────────


def _flatten(node: dict, agents: dict, children: dict) -> None:
    kids = node.get("children") or []
    agents[node["id"]] = {k: v for k, v in node.items() if k != "children"}
    children[node["id"]] = [c["id"] for c in kids]
    for c in kids:
        _flatten(c, agents, children)


def _disk_files(root: Path) -> list[str]:
    """Every persisted file under `.fantastic`, minus the seeded documentation
    (`readme.md` + `docs/`, copied per agent by `write_record`) which would bury
    the records and sidecars a scenario actually reads."""
    if not root.is_dir():
        return []
    out: list[str] = []
    for p in sorted(root.rglob("*")):
        if not p.is_file():
            continue
        rel = p.relative_to(root)
        if "docs" in rel.parts or rel.name == "readme.md":
            continue
        out.append(str(rel))
    return out


async def dehydrate(kernel, *, disk: bool = False) -> dict:
    """Read the running system back as a plain dict. No assertions, no opinion.

    The tree comes from ONE root reflect (`detail=full, tree=all`) — the same
    round-trip a client gets, so the suite reconciles against what the system
    SAYS about itself rather than against private attributes. `disk=True` adds
    the persisted records (`read_tree`) and the file layout, for the scenarios
    that care where a child's state landed; a kernel that has not been
    `persist()`ed reports only what `boot_root` materialized (the root record).
    """
    reply = (
        await kernel.send(
            [
                {
                    "target": "kernel",
                    "payload": {"type": "reflect", "detail": "full", "tree": "all"},
                }
            ]
        )
    )[0]["result"]
    tree = reply.get("tree") or {}
    agents: dict[str, dict] = {}
    children: dict[str, list[str]] = {}
    if tree:
        _flatten(tree, agents, children)
    out: dict[str, Any] = {
        "root": kernel.id,
        "ids": sorted(agents),
        "agents": agents,
        "children": children,
        "tree": tree,
        "persistence_provider": (reply.get("persistence") or {}).get("provider"),
    }
    if disk:
        root_dir = Path(".fantastic")
        records = read_tree(root_dir)
        out["disk"] = {
            r["id"]: r for r in records if isinstance(r, dict) and r.get("id")
        }
        out["disk_ids"] = sorted(out["disk"])
        out["disk_files"] = _disk_files(root_dir)
    return out


# ─── reach: THE reachability primitive ──────────────────────────


def _decode_bytes_fields(payload: dict) -> dict:
    """`bytes_utf8: "text"` in a spec becomes real `bytes: b"text"` at dispatch.

    The corpus is YAML — cross-runtime DATA — and YAML/JSON cannot carry bytes,
    but the stream verbs (`write_stream`) take nothing else. This is the one
    translation, applied at the engine's dispatch points, so a spec stays
    expressible in every runtime's runner without a string body ever crossing
    an envelope.
    """
    if "bytes_utf8" in payload:
        out = {k: v for k, v in payload.items() if k != "bytes_utf8"}
        out["bytes"] = str(payload["bytes_utf8"]).encode("utf-8")
        return out
    return payload


async def reach(kernel, sender: str, target: str, payload: dict) -> dict:
    """Send `payload` to `target` AS `sender` and return the raw reply.

    Every claim this suite makes about wiring goes through here. Outside a
    dispatch `_current_sender` is None, which makes a bare `kernel.send` an
    OPERATOR send — ungated, and therefore worthless as evidence about an
    agent's reach. `sender_context` is what turns the driver into an agent
    (the gate in `Agent._launch`); the block wraps ONLY the send under test, so
    setup keeps operator authority.

    A handler that answers `None` is reported as `{}` — a non-denial. Denials
    are always dicts, so `.get("reason")` stays the one question a caller asks.
    """
    payload = _decode_bytes_fields(payload)
    with sender_context(sender):
        reply = (await kernel.send([{"target": target, "payload": payload}]))[0][
            "result"
        ]
    return reply if isinstance(reply, dict) else {}


def denied(reply: dict) -> bool:
    """The one machine-readable question about a reply. Never `"error" in r`:
    a bundle error and a gate denial are different animals, and only the gate
    sets `reason`."""
    return isinstance(reply, dict) and reply.get("reason") == "unauthorized"


def denial_side(reply: dict) -> str | None:
    """Which half refused: `egress` (the sender may not call) / `ingress` (the
    target does not accept) / `self-grant` / `delegation`. This field is the
    evidence that the gate is two-sided at all."""
    return reply.get("side") if isinstance(reply, dict) else None


# ─── the STEP EXECUTOR — what makes a spec runnable ─────────────
#
# Every runtime implements this same loop over the same YAML. It is the only
# per-runtime part of the agentic suite; the scenarios themselves are data.


class StepFailure(AssertionError):
    """A step whose verdict contradicted its spec. Carries the step's own name
    so a failure reads as the scenario's sentence, not as a python traceback."""


def _dig(obj: Any, path: str) -> list[Any]:
    """Resolve a dotted path, `*` meaning "every element of this list".

    `files.*.name` over `{"files": [{"name": "a"}]}` → `["a"]`. Used by the
    `contains` / `not_contains` matchers so a spec can assert a side effect
    (a directory that must NOT have been created) without the runner knowing
    anything about the bundle that produced it.
    """
    cur: list[Any] = [obj]
    for part in path.split("."):
        nxt: list[Any] = []
        for c in cur:
            if part == "*":
                if isinstance(c, list):
                    nxt.extend(c)
            elif isinstance(c, dict) and part in c:
                nxt.append(c[part])
        cur = nxt
    return cur


def _canon_tree(node: Any) -> Any:
    """A tree with children in a canonical order.

    A reboot rebuilds the roster from persisted records and hands the children
    back sorted by id, while the first hydration builds them in MANIFEST order.
    That is an ordering difference, not a roster difference, and "the roster came
    back identical" is a claim about CONTENT — comparing raw order would fail on
    a system that lost nothing.
    """
    if isinstance(node, dict):
        out = {k: _canon_tree(v) for k, v in node.items()}
        kids = out.get("children")
        if isinstance(kids, list):
            out["children"] = sorted(
                kids, key=lambda c: c.get("id", "") if isinstance(c, dict) else ""
            )
        return out
    if isinstance(node, list):
        return [_canon_tree(v) for v in node]
    return node


def _resolve(value: Any, bound: dict[str, Any]) -> Any:
    """Substitute `$name.field` references from earlier steps.

    A `start` step binds its outcome under a name; a later `cancel` has to say
    WHICH task, and only the kernel knows the pid it minted. Without bindings a
    spec could not name what it had just created.
    """
    if isinstance(value, str) and value.startswith("$"):
        ref = value[1:]
        name, _, field_ = ref.partition(".")
        if name not in bound:
            raise ConfigError(f"${ref}: nothing bound under {name!r}")
        target = bound[name]
        if not field_:
            return target
        got = _dig(target, field_)
        if not got:
            raise ConfigError(f"${ref}: {field_!r} not present in {target!r}")
        return got[0]
    if isinstance(value, dict):
        return {k: _resolve(v, bound) for k, v in value.items()}
    if isinstance(value, list):
        return [_resolve(v, bound) for v in value]
    return value


def _match(step: Step, reply: dict, outcome: dict | None = None) -> None:
    """Apply a step's matchers to what it got back. Raises `StepFailure` naming
    the step, the matcher and the actual reply — a spec failure has to be
    readable without opening the runner."""
    exp = step.expect

    def fail(why: str) -> None:
        raise StepFailure(f"{step}: {why}\n  reply: {reply!r}")

    if "allow" in exp:
        was_denied = denied(reply)
        if exp["allow"] and was_denied:
            fail("expected the edge to be OPEN, it was denied")
        if not exp["allow"] and not was_denied:
            fail("expected a DENIAL, the call went through")
    if "side" in exp:
        got = denial_side(reply)
        if exp["side"] is None:
            # `side: null` — the refusal must carry NO side. That is what tells
            # an io SEAL from the call gate: both answer `unauthorized`, but only
            # the gate names a half, because only the gate is about the caller.
            if got is not None:
                fail(f"expected NO side (an io seal), got {got!r} (the call gate)")
        elif got != exp["side"]:
            fail(f"expected the {exp['side']!r} half to refuse, got {got!r}")
    if "reason" in exp and reply.get("reason") != exp["reason"]:
        fail(f"expected reason {exp['reason']!r}, got {reply.get('reason')!r}")
    if "error_contains" in exp:
        if exp["error_contains"] not in str(reply.get("error", "")):
            fail(f"error does not contain {exp['error_contains']!r}")
    if "status" in exp:
        got = (outcome or {}).get("status")
        if got != exp["status"]:
            fail(f"expected status {exp['status']!r}, got {got!r}")
    for path, want in (exp.get("contains") or {}).items():
        # The stream verbs answer BYTES; a YAML spec can only write text. The
        # mirror of `bytes_utf8` on the send side: a contains path ending in
        # `bytes` compares against the utf-8 decoding, so a spec can assert on
        # streamed content without a string body ever crossing an envelope.
        if path.split(".")[-1] == "bytes":
            got = [
                v.decode("utf-8", "replace") if isinstance(v, (bytes, bytearray)) else v
                for v in _dig(reply, path)
            ]
            if not any(want in g for g in got if isinstance(g, str)):
                fail(f"{path} does not contain {want!r} (got {got!r})")
            continue
        if want not in _dig(reply, path):
            fail(f"{path} does not contain {want!r} (got {_dig(reply, path)!r})")
    for path, unwanted in (exp.get("not_contains") or {}).items():
        got = _dig(reply, path)
        # A path that resolves to NOTHING is not proof of absence — it is proof
        # the assertion never looked. A typo'd path would otherwise pass
        # silently forever, which is the one failure mode a corpus cannot
        # afford: a green step asserting nothing.
        if not got:
            fail(
                f"not_contains {path!r} resolved to nothing — the path is wrong,"
                " or the reply shape changed; absence here proves nothing"
            )
        if unwanted in got:
            fail(f"{path} contains {unwanted!r} and must not")


async def _grant_step(h: "Hydration", step: Step, remove: bool) -> dict:
    """`grant`/`revoke` — sugar over an OPERATOR `update_agent`.

    Sugar, not a shortcut: `update_agent` REPLACES a field rather than merging
    it (#6), so a spec that hand-wrote the payload would have to restate the
    whole grant map every time and would silently drop the structural baseline.
    The runner reads the live map, edits one entry, and writes it back whole.
    """
    agent_id = step.args["agent"]
    field_ = step.args["field"]
    other = step.args["to"]
    agent = h.kernel.ctx.agents[agent_id]
    current = dict(agent._meta.get(field_) or {})
    if remove:
        current.pop(other, None)
    else:
        current[other] = list(step.args.get("verbs") or ["*"])
    return (
        await h.kernel.ctx.send(
            [
                {
                    "target": "kernel",
                    "payload": {
                        "type": "update_agent",
                        "id": agent_id,
                        field_: current,
                    },
                }
            ]
        )
    )[0]["result"]


async def run_steps(h: "Hydration", steps: tuple[Step, ...]) -> "Hydration":
    """Execute a spec's steps in order. Returns the (possibly rebooted) hydration.

    Bindings live for the whole scenario, so a `start` early on can be cancelled
    and collected much later.
    """
    bound: dict[str, Any] = {}
    tasks: dict[str, Any] = {}
    tree_before: dict | None = None

    for idx, step in enumerate(steps):
        args = _resolve(step.args, bound)
        reply: dict = {}
        outcome: dict | None = None

        if step.kind == "reach":
            reply = await reach(h.kernel, args["from"], args["to"], args["send"])

        elif step.kind == "operator":
            # OPERATOR authority: no `sender_context`, so the gate is not
            # consulted. This is setup and side-effect READS — never evidence
            # about an agent's reach, which is what `reach` is for.
            out = (
                await h.kernel.ctx.send(
                    [
                        {
                            "target": args["to"],
                            "payload": _decode_bytes_fields(args.get("send") or {}),
                        }
                    ]
                )
            )[0]
            outcome = out
            reply = out["result"] if isinstance(out["result"], dict) else {}

        elif step.kind in ("grant", "revoke"):
            reply = await _grant_step(h, step, remove=step.kind == "revoke")

        elif step.kind == "reboot":
            tree_before = await dehydrate(h.kernel)
            h = await reboot(h)
            reply = {"rebooted": True}

        elif step.kind == "assert_tree":  # noqa: PLR2004
            got = await dehydrate(h.kernel, disk=bool(args.get("disk")))
            if args.get("same_as") == "before_reboot":
                if tree_before is None:
                    raise ConfigError(
                        f"{step}: `same_as: before_reboot` with no preceding reboot"
                    )
                if _canon_tree(got.get("tree")) != _canon_tree(tree_before.get("tree")):
                    raise StepFailure(
                        f"{step}: the roster did not come back identical\n"
                        f"  before: {tree_before.get('tree')!r}\n"
                        f"  after:  {got.get('tree')!r}"
                    )
            reply = got

        elif step.kind == "start":
            # Fire WITHOUT awaiting. The cancel-semantics rows (C14/C15/C16)
            # need a dispatch that is still running when the next step acts on
            # it; nothing else in the DSL can express "in flight".
            task = asyncio.ensure_future(
                reach_outcome(h.kernel, args["from"], args["to"], args["send"])
            )
            tasks[args["name"]] = task
            reply = {"started": True}

        elif step.kind == "await_task":
            # Block until the started send is visible in `ps`. Replaces the
            # suite's one wall-clock sleep: a fixed sleep is a race that passes
            # on a fast machine and fails on a loaded one.
            name = args["name"]
            deadline = time.monotonic() + float(args.get("timeout", 5))
            agent_id = None
            while time.monotonic() < deadline:
                rows = (
                    await h.kernel.ctx.send(
                        [{"target": "kernel", "payload": {"type": "ps"}}]
                    )
                )[0]["result"]
                live = rows.get("tasks") or []
                match = [t for t in live if t.get("agent") == args.get("agent")]
                if match:
                    bound[name] = match[0]
                    agent_id = match[0]
                    break
                await asyncio.sleep(0.005)
            if agent_id is None:
                raise StepFailure(
                    f"{step}: no task for agent {args.get('agent')!r} appeared"
                    f" in `ps` within the timeout"
                )
            reply = agent_id

        elif step.kind in ("reconcile_grants", "sweep_undeclared"):
            # The sweeps need a concrete payload per verb: `list` wants a path,
            # `reflect` wants nothing. The table is DATA in the step so a
            # scenario using other verbs does not need runner changes.
            table = dict(args.get("payloads") or {})

            def payload_for(verb: str, _t=table) -> dict:
                return {"type": verb, **dict(_t.get(verb) or {})}

            if step.kind == "reconcile_grants":
                await _sweep_reconcile(h, step, payload_for)
            else:
                await _sweep_undeclared(h, step, payload_for, prior=steps[:idx])
            reply = {"swept": True}

        elif step.kind == "collect":
            name = args["name"]
            if name not in tasks:
                raise ConfigError(f"{step}: nothing started under {name!r}")
            outcome = await tasks.pop(name)
            reply = outcome["result"] if isinstance(outcome["result"], dict) else {}
            # MERGE, never clobber. `await_task` bound the ps row (`id`) and this
            # binds the outcome (`pid`) — the SAME id, by the one-id-space rule —
            # so a later step can still name either. Replacing the binding would
            # silently break every `$job.id` written before the collect.
            merged = dict(bound.get(name) or {})
            merged.update(outcome)
            bound[name] = merged

        else:  # pragma: no cover — the parser rejects unknown verbs
            raise ConfigError(f"{step}: unknown step verb {step.kind!r}")

        # Bindings resolve in the MATCHERS too: a step that asserts on the pid
        # an earlier step minted has to be able to name it.
        _match(replace(step, expect=_resolve(step.expect, bound)), reply, outcome)
        if step.because:
            check(step.because, True, str(step))

    for name, task in tasks.items():
        task.cancel()
    return h


async def reach_outcome(kernel, sender: str, target: str, payload: dict) -> dict:
    """`reach`, but returning the whole OUTCOME rather than just the result —
    `start`/`collect` assert on `status`, which only the outcome carries."""
    with sender_context(sender):
        return (
            await kernel.send(
                [{"target": target, "payload": _decode_bytes_fields(payload)}]
            )
        )[0]


# ─── the computed sweeps ────────────────────────────────────────
#
# Both are reconciliations: they walk the DECLARATION (the manifest's grant
# maps, the running tree's parentage) and probe the running system against it.
# That is the step the borrowed corpora do not have — openfga and a2a-tck both
# assert behaviour with a declarative file as INPUT, never that a declared shape
# equals a running one.


def _manifest_specs(cfg) -> dict[str, dict]:
    """The manifest entries by id — the DECLARATION, never evidence."""
    return {
        spec["id"]: spec
        for ln in cfg.loaders
        for spec in ln.declared_agents
        if spec.get("id")
    }


def _declared_verbs(spec: dict, field_: str, other: str) -> set[str]:
    """The verbs `spec` declares for `other`. A `"*"` is REFUSED rather than
    interpreted: a wildcard makes the per-verb intersection unobservable, and
    that intersection is the whole point of the walk."""
    grant = (spec.get(field_) or {}).get(other)
    if grant is None:
        return set()
    if not isinstance(grant, list):
        raise ConfigError(
            f"reconcile_grants: {field_}[{other}] is {grant!r}; this sweep needs"
            " explicit verb lists, a wildcard hides the intersection"
        )
    return set(grant)


def _structural_pairs(children: dict, universe: set[str]) -> set:
    """The edges the kernel stamps and nobody declares: `$self`, `$parent`,
    `$children` — each agent, plus each parent↔child pair, both directions.
    Derived from the RUNNING tree, which PC-1 already reconciled."""
    pairs = {(a, a) for a in universe}
    for parent, kids in children.items():
        for kid in kids:
            if parent in universe and kid in universe:
                pairs |= {(parent, kid), (kid, parent)}
    return pairs


async def _sweep_reconcile(h, step: Step, payload_for) -> None:
    """For every pair either half mentions, the edge's verb set is the
    INTERSECTION of the two declared lists, and a verb outside it is refused by
    the half that omitted it — checked in order, so `side` is not symmetric
    bookkeeping."""
    specs = _manifest_specs(h.config)
    probed = 0
    for sender, s_spec in specs.items():
        for target, t_spec in specs.items():
            if sender == target:
                continue
            egress = _declared_verbs(s_spec, "can_call", target)
            ingress = _declared_verbs(t_spec, "callable_by", sender)
            for verb in sorted(egress | ingress):
                probed += 1
                reply = await reach(h.kernel, sender, target, payload_for(verb))
                both = verb in egress and verb in ingress
                if denied(reply) == both:
                    raise StepFailure(
                        f"{step}: {sender} -{verb}-> {target}: declared"
                        f" egress={sorted(egress)} ingress={sorted(ingress)} —"
                        f" expected {'reachable' if both else 'refused'},"
                        f" got {reply!r}"
                    )
                if both:
                    continue
                want = "egress" if verb not in egress else "ingress"
                if denial_side(reply) != want:
                    raise StepFailure(
                        f"{step}: {sender} -{verb}-> {target}: the half that"
                        f" omitted {verb!r} is {want!r}, the denial named"
                        f" {denial_side(reply)!r}"
                    )
    # A guard on the WALK, not a claim about the kernel: a manifest edited down
    # to nothing would otherwise reconcile perfectly against no probes at all.
    floor = int(step.args.get("min_probes", 1))
    if probed < floor:
        raise StepFailure(
            f"{step}: only {probed} declared triples to walk (min {floor})"
        )


async def _sweep_undeclared(h, step: Step, payload_for, prior=()) -> None:
    """The whole roster, every ordered pair, every named verb. The reachable set
    must be EXACTLY the structural baseline plus the spec's own allowed reaches
    — nothing rides along.

    Two properties fall out that no single probe shows: co-residency grants
    nothing (siblings on one loader are not reachable unless both halves name
    it), and authority is not transitive (`$children` is direct children, a set
    membership, not a subtree).
    """
    d = await dehydrate(h.kernel)
    universe = [h.kernel.id, *h.config.loader_ids, *h.config.roster_ids]
    missing = set(universe) - set(d["ids"])
    if missing:
        raise StepFailure(f"{step}: sweep universe is not running: {sorted(missing)}")
    structural = _structural_pairs(d["children"], set(universe))

    for verb in step.args.get("verbs") or ["reflect"]:
        # Only the reaches declared open BEFORE this sweep. The steps are
        # ORDERED and grants happen mid-scenario, so a sweep asserts the state
        # AS OF ITS POSITION — counting an edge the ladder opens later would
        # demand it be open before the grant that opens it.
        declared = {
            (s.args["from"], s.args["to"])
            for s in prior
            if s.kind == "reach"
            and s.expect.get("allow") is True
            and (s.args.get("send") or {}).get("type") == verb
        }
        want = structural | declared
        got: set = set()
        sides: dict = {}
        for sender in universe:
            for target in universe:
                reply = await reach(h.kernel, sender, target, payload_for(verb))
                if denied(reply):
                    sides[(sender, target)] = denial_side(reply)
                else:
                    got.add((sender, target))
        if got != want:
            raise StepFailure(
                f"{step}: verb {verb!r}: undeclared edges {sorted(got - want)},"
                f" declared-and-missing {sorted(want - got)}"
            )
        bad = {s for s in sides.values() if s not in ("egress", "ingress")}
        if bad:
            raise StepFailure(
                f"{step}: verb {verb!r}: a refusal did not name a call-gate half"
                f" — {sorted(bad)} (a sealed io leg denies with"
                " reason=unauthorized and NO side, which is a DIFFERENT gate)"
            )
