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

import re
from dataclasses import dataclass, field, replace
from pathlib import Path
from typing import Any

import pytest
import yaml
from kernel_state.tools import read_tree

from _testkit import boot_root, persist
from kernel import sender_context

# ─── requirement registry (a2a-tck's ID discipline) ─────────────
#
# Stable ids, one table, duplicate detection AT IMPORT. The point is not
# bookkeeping: it is that a hand-written validator can name the invariant it is
# pinning (`check("C23", …)`) and that the name cannot silently come to mean two
# things. MUST asserts; SHOULD/MAY xfail, so a known-soft expectation reports as
# a coverage hole instead of a red build.

_LEVELS = frozenset({"MUST", "SHOULD", "MAY"})


@dataclass(frozen=True)
class Requirement:
    """One named invariant this suite can pin. `level` decides what a failure
    DOES: MUST raises, SHOULD/MAY xfail (a2a-tck's mapping)."""

    id: str
    level: str
    title: str


# (id, level, title). Ordered: the five core preconfig invariants first, then the
# per-scenario items they do not cover.
_ROWS: tuple[tuple[str, str, str], ...] = (
    # ── the five CORE preconfig invariants, asserted for EVERY config ──
    ("PC-1", "MUST", "the running tree matches the declared shape, both directions"),
    (
        "PC-2",
        "MUST",
        "every declared edge is reachable, every refused edge names its side",
    ),
    ("PC-3", "MUST", "the roster is closed after boot — deny-all holds, no self-raise"),
    ("PC-4", "MUST", "a reboot rehydrates the identical roster, nothing duplicates"),
    (
        "PC-5",
        "MUST",
        "the hydration verdict matches the declaration, on the loud channel",
    ),
    # ── per-scenario items (validators tag their own assertions) ──
    #
    # The `C<n>` ids are NOT this suite's invention: they are row numbers in the
    # canonical-assertion table of `tmp/agentic_development.md`, so a gap in the
    # sequence is a checkpoint nobody has claimed rather than a missing row. This
    # suite closes C14–C18, C20 and C23 out of that table's "built but unasserted"
    # bucket. C21 and C24 wanted scenarios this manifest cannot declare and are
    # asserted in `tests/conformance/` instead (2026-08-26): C21 =
    # test_c21_transport_symmetry (five transports, one envelope, byte-diffed);
    # C24 = test_stream_semantics (in-proc order/terminal/multi-subscriber) +
    # test_c24_stream_transport (the streaming subscriber ACROSS a transport).
    # Rows kept here so the registry stays the one index of the canonical 26.
    (
        "C1",
        "MUST",
        "resume after suspension continues the run — script minds: test_mind_plumbed ASK; AI chains: tests/conformance/test_c1_suspend_resume",
    ),
    ("C14", "MUST", "cancel stops a live dispatch before its next step, observably"),
    ("C15", "MUST", "a terminal task and a deleted agent both absorb further calls"),
    ("C16", "MUST", "unknown id / malformed / unknown verb / denial stay distinct"),
    ("C17", "MUST", "a refused mutation mutates nothing and says which half refused"),
    ("C18", "MUST", "delete removes the whole owned roster and spares its neighbours"),
    ("C20", "MUST", "child state is namespaced by position, not by convention"),
    ("C21", "MUST", "one semantics, many transports — see tests/conformance/"),
    (
        "C24",
        "MUST",
        "stream order/terminal/fan-out — see tests/conformance/ + test_stream_semantics",
    ),
    (
        "C23",
        "MUST",
        "an undeclared capability errors — sealed by default, no byte moves",
    ),
    # ── two_sided_gate ──
    ("TSG-1", "MUST", "an edge exists only when BOTH halves grant it"),
    ("TSG-2", "MUST", "a denial names the half that refused, and egress is first"),
    ("TSG-3", "MUST", "an edge carries only the verbs BOTH halves name"),
    ("TSG-4", "MUST", "no undeclared edge exists — co-residency grants nothing"),
    ("TSG-5", "MUST", "the missing half comes from ABOVE; a peer cannot supply it"),
    ("TSG-6", "MUST", "opening one half never silently closes another"),
)


def _index(rows) -> dict[str, Requirement]:
    """Build the id→Requirement table, refusing a duplicate id or an unknown
    level AT IMPORT. A duplicate id is the one failure mode that makes the whole
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


REQUIREMENTS: dict[str, Requirement] = _index(_ROWS)

# The five the GENERAL runner pins on every config, by construction. A config
# never declares them: they are not a property of the scenario, they are the
# price of being a config at all, and listing them per file made the field mean
# three different things across eight of them. `requirements:` therefore names
# ONLY what a scenario's own validator pins — enforced in `load_config`.
CORE_REQUIREMENTS: frozenset[str] = frozenset({"PC-1", "PC-2", "PC-3", "PC-4", "PC-5"})

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

CONFIG_DIR = Path(__file__).resolve().parent / "configs"
# Specs are the suite's index: a config is a deployable kernel document and does
# not know it is being tested.
SPEC_DIR = Path(__file__).resolve().parent / "specs"

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
    {"hydration", "error_contains", "reboot", "edges", "closed_roster_sender"}
)
_EDGE_KEYS = frozenset({"name", "from", "to", "send", "allow", "side"})

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
class Edge:
    """One reachability probe: an agent-origin send and the verdict it must get.
    `side` is what proves the gate is TWO-sided — a declared `can_call` with no
    matching `callable_by` denies on `ingress`, the reverse on `egress`."""

    sender: str
    target: str
    send: dict
    allow: bool
    side: str | None = None
    name: str = ""

    def __str__(self) -> str:
        return self.name or f"{self.sender} → {self.target} {self.send.get('type')!r}"


@dataclass(frozen=True)
class Expectation:
    hydration: str = "ok"
    error_contains: str = ""
    reboot: bool = True
    edges: tuple[Edge, ...] = ()
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


def _parse_edges(raw) -> tuple[Edge, ...]:
    if raw is None:
        return ()
    if not isinstance(raw, list):
        raise ConfigError("expect.edges: must be a list")
    edges: list[Edge] = []
    for i, item in enumerate(raw):
        item = _reject_unknown(f"expect.edges[{i}]", item, _EDGE_KEYS)
        for req in ("from", "to", "send", "allow"):
            if req not in item:
                raise ConfigError(f"expect.edges[{i}]: `{req}:` is required")
        if not isinstance(item["send"], dict) or not item["send"].get("type"):
            raise ConfigError(f"expect.edges[{i}]: `send:` needs a payload with a type")
        allow = item["allow"]
        if not isinstance(allow, bool):
            raise ConfigError(f"expect.edges[{i}]: `allow:` must be true/false")
        if allow and item.get("side"):
            raise ConfigError(
                f"expect.edges[{i}]: `side:` only means something on a denial"
            )
        edges.append(
            Edge(
                sender=item["from"],
                target=item["to"],
                send=dict(item["send"]),
                allow=allow,
                side=item.get("side"),
                name=item.get("name", ""),
            )
        )
    return tuple(edges)


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
    raw = {**cfg_raw, **spec_raw}
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
            edges=_parse_edges(exp_raw.get("edges")),
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
