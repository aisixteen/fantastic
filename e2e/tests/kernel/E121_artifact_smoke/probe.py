"""kernel/artifact_smoke — the PACKAGED kernel boots and describes itself. NO MODEL.

On the packaged artifact of `RUNTIME` (python | rust):

  1. ONE-SHOT IN AN EMPTY DIR: `<artifact> reflect` in a fresh empty directory
     exits 0 and prints the root reflect (`id: kernel_state`, `runtime: RUNTIME`);
  2. BOOTED (the driver's seed, every call over `web_rest`), the distilled root
     reflect carries the documented keys (`runtime → env → version`, `peers`,
     `persistence {provider}` naming the seeded record store, a minted `guid`,
     `self_extension`, `tree`) and nothing that says how to call (`verbs`,
     `emits`, `cancel`, `kernel_verbs`);
  3. `detail: "full"` adds the doc frame: `readmes` (the topic index) + `docs`;
  4. `readme: "index"` is the token-annotated topic tree `[{key, desc, tokens}]`;
     every top-level key drills (`readme: "<key>"`) to a body; `readme: true` is
     the short orientation; an unknown key → `{error, keys}`;
  5. `docs of=kernel` lists the kernel verbs;
  6. `reflect bundles: "all"` lists the shipped bundle kinds, each with its
     `handler_module` (an AI backend kind with its record `schema`); `docs of=<handler_module>` answers for EVERY
     listed kind with a non-empty verb table that includes `reflect`;
  7. VS PYTHON (the reference, a second packaged python kernel booted beside it):
     the same set of bundle kinds — a kind only one side ships is a finding unless
     it is documented as runtime-specific (`RUNTIME_ONLY`); per shared kind the
     same `docs` verb names and the same `schema` field names.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from helpers.host.driver import HostKernel, ensure_runtime, runtime
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")

#: Kinds DOCUMENTED as runtime-specific: kind → the runtimes that ship it.
#: `foundation_models_backend` — Apple's on-device model, one swift implementation
#: rust links through a C ABI (src/lib/rust/README.md).
#: `sqlite_store` — the durable memory store ships in the python kernel
#: (`bundled_agents/memory/sqlite_store`, #59); rust has it only in the `fantastic`
#: CLI product (`src/tui/bundles/fantastic-sqlite-store`), not in the headless
#: kernel binary.
RUNTIME_ONLY = {
    "foundation_models_backend": {"rust"},
    "sqlite_store": {"python"},
    "cabinet": {"python", "rust"},
}

#: The distilled root reflect (kernel_state readme + CLAUDE.md "Universal patterns").
ROOT_KEYS = {
    "id", "sentence", "parent_id", "handler_module", "display_name", "can_call",
    "callable_by", "runtime", "peers", "env", "version", "persistence", "guid",
    "self_extension", "tree",
}
#: A reflect says what an agent IS, never how to call it.
NEVER_ON_REFLECT = {"verbs", "emits", "cancel", "more", "kernel_verbs"}
#: The kernel verbs (CLAUDE.md "Architecture": create/delete/update/list_agents,
#: ps/cancel, docs).
KERNEL_VERBS = {"create_agent", "delete_agent", "update_agent", "list_agents",
                "ps", "cancel", "docs"}


def one_shot(spec: dict) -> tuple[int, dict | None, str]:
    """`<artifact> reflect` in a fresh empty dir → (exit code, parsed reply, raw)."""
    d = Path(tempfile.mkdtemp(prefix="e2e-artifact-empty-"))
    try:
        r = subprocess.run([*spec["cmd"], "reflect"], cwd=d, capture_output=True, check=False,
                           text=True, timeout=120, env={**os.environ, **(spec.get("env") or {})})
        try:
            return r.returncode, json.loads(r.stdout), r.stdout
        except ValueError:
            return r.returncode, None, (r.stdout + r.stderr)[-600:]
    finally:
        shutil.rmtree(d, ignore_errors=True)


def catalog(hk: HostKernel) -> dict[str, dict]:
    """`reflect bundles=all` → {kind name: entry}."""
    r = hk.send(hk.root_id, {"type": "reflect", "bundles": "all", "tree": "none"})
    return {b.get("name"): b for b in r.get("bundles") or [] if isinstance(b, dict)}


def schema_fields(entry: dict) -> set[str]:
    sch = entry.get("schema") or {}
    return {f.get("name") for f in sch.get("fields") or [] if isinstance(f, dict)}


def main() -> int:
    name, _ = runtime(REPO, RUNTIME)
    spec = ensure_runtime(REPO, name)
    bad: list[str] = []

    def need(cond: bool, what: str) -> None:
        print(f"  {'ok ' if cond else 'BAD'} {what}", flush=True)
        if not cond:
            bad.append(what)

    # 1 · one-shot in an empty dir
    code, rep, raw = one_shot(spec)
    need(code == 0 and isinstance(rep, dict) and rep.get("id") == "kernel_state"
         and rep.get("runtime") == name,
         f"one-shot `reflect` in an empty dir: exit {code}, "
         f"id={None if rep is None else rep.get('id')!r} "
         f"runtime={None if rep is None else rep.get('runtime')!r}"
         + ("" if rep is not None else f" raw={raw[-200:]!r}"))

    work = Path(tempfile.mkdtemp(prefix="e2e-artifact-smoke-"))
    hk = HostKernel(spec, work, reach=[]).boot()
    ref: HostKernel | None = None
    ref_work: Path | None = None
    root = hk.root_id
    try:
        # 2 · the distilled root reflect
        rf = hk.send(root, {"type": "reflect"})
        keys = set(rf) if isinstance(rf, dict) else set()
        need(ROOT_KEYS <= keys, f"root reflect has the documented keys; missing "
             f"{sorted(ROOT_KEYS - keys)}")
        need(not (keys & NEVER_ON_REFLECT),
             f"no how-to-call keys on reflect: {sorted(keys & NEVER_ON_REFLECT)}")
        order = [k for k in rf if k in ("runtime", "env", "version")]
        need(order == ["runtime", "env", "version"], f"key order runtime → env → version: {order}")
        need(rf.get("runtime") == name, f"runtime = {name}: {rf.get('runtime')!r}")
        need(rf.get("env") == os.environ.get("FANTASTIC_ENV", "host"),
             f"env from $FANTASTIC_ENV (default host): {rf.get('env')!r}")
        need(rf.get("version") == os.environ.get("FANTASTIC_VERSION"),
             f"version from $FANTASTIC_VERSION (default null): {rf.get('version')!r}")
        need(rf.get("persistence") == {"provider": "records"},
             f"persistence names the seeded record store: {rf.get('persistence')}")
        need(isinstance(rf.get("guid"), str) and len(rf["guid"]) >= 16,
             f"a guid minted at boot: {rf.get('guid')!r}")
        need(rf.get("peers") == [], f"no peers: {rf.get('peers')}")
        tree_ids = {c.get("id") for c in (rf.get("tree") or {}).get("children") or []}
        need({"web", "records"} <= tree_ids, f"tree nests the seeded children: {sorted(tree_ids)}")

        # 3 · the full reflect carries the doc frame
        full = hk.send(root, {"type": "reflect", "detail": "full", "tree": "none"})
        need(isinstance(full.get("readmes"), list) and bool(full["readmes"])
             and isinstance(full.get("docs"), str) and bool(full["docs"]),
             f"detail=full carries readmes + docs: readmes={type(full.get('readmes')).__name__} "
             f"docs={type(full.get('docs')).__name__}")

        # 4 · the doc tree
        idx = hk.send(root, {"type": "reflect", "readme": "index", "tree": "none"})
        index = (idx.get("readme") or {}).get("index") if isinstance(idx.get("readme"), dict) else None
        need(isinstance(index, list) and bool(index) and all(
            isinstance(e, dict) and {"key", "desc", "tokens"} <= set(e) for e in index),
            f"readme=index is [{{key, desc, tokens}}]: {str(idx.get('readme'))[:200]}")
        topic_keys = [e["key"] for e in index or []]
        need(topic_keys == (full.get("readmes") and [e.get("key") for e in full["readmes"]]),
             f"the full reflect's readmes is that index: {topic_keys}")
        undrilled = []
        for k in topic_keys:
            t = hk.send(root, {"type": "reflect", "readme": k, "tree": "none"})
            body = t.get("readme")
            if not (isinstance(body, dict) and isinstance(body.get("body"), str) and body["body"].strip()):
                undrilled.append((k, str(body)[:80]))
        need(not undrilled, f"every one of {len(topic_keys)} topics drills to a body: {undrilled[:3]}")
        orient = hk.send(root, {"type": "reflect", "readme": True, "tree": "none"})
        need(isinstance(orient.get("readme"), str) and len(orient["readme"]) > 100,
             f"readme=true is the orientation: {str(orient.get('readme'))[:80]!r}")
        unk = hk.send(root, {"type": "reflect", "readme": "no-such-topic", "tree": "none"})
        ur = unk.get("readme") if isinstance(unk.get("readme"), dict) else unk
        need(bool(ur.get("error")) and isinstance(ur.get("keys"), list),
             f"an unknown topic → {{error, keys}}: {str(ur)[:160]}")

        # 5 · docs of=kernel
        dk = hk.send("kernel", {"type": "docs", "of": "kernel"})
        kv = set(dk.get("verbs") or {})
        need(KERNEL_VERBS <= kv, f"docs of=kernel lists the kernel verbs; missing "
             f"{sorted(KERNEL_VERBS - kv)}")

        # 6 · the shipped catalog, and every kind's docs
        cat = catalog(hk)
        print(f"     {name} ships {len(cat)} kinds: {sorted(cat)}", flush=True)
        need(len(cat) > 0 and all(e.get("handler_module") == f"{k}.tools" for k, e in cat.items()),
             f"each kind names its handler_module: "
             f"{[(k, e.get('handler_module')) for k, e in cat.items() if e.get('handler_module') != f'{k}.tools']}")
        # A backend KIND publishes its record schema (ai_core/schema.py RECORD_SCHEMA).
        backends = [k for k in cat if k.endswith("_backend")]
        need(bool(backends) and all(isinstance(cat[k].get("schema"), dict) for k in backends),
             f"each AI backend kind carries a schema: "
             f"{[(k, isinstance(cat[k].get('schema'), dict)) for k in backends]}")
        docs: dict[str, set[str]] = {}
        undoc = []
        for k, e in sorted(cat.items()):
            d = hk.send("kernel", {"type": "docs", "of": e.get("handler_module")})
            verbs = d.get("verbs") if isinstance(d, dict) else None
            if d.get("error") or not isinstance(verbs, dict) or "reflect" not in verbs:
                undoc.append((k, str(d)[:120]))
            docs[k] = set(verbs or {})
        need(not undoc, f"docs of=<handler_module> answers for every kind: {undoc}")

        # 7 · vs python
        if name != "python":
            ref_work = Path(tempfile.mkdtemp(prefix="e2e-artifact-ref-"))
            ref = HostKernel(ensure_runtime(REPO, "python"), ref_work, reach=[]).boot()
            pcat = catalog(ref)
            pdocs = {k: set(ref.send("kernel", {"type": "docs", "of": e.get("handler_module")})
                             .get("verbs") or {}) for k, e in pcat.items()}
        else:
            pcat, pdocs = cat, docs
        only_here = {k for k in set(cat) - set(pcat) if name not in RUNTIME_ONLY.get(k, set())}
        only_py = {k for k in set(pcat) - set(cat) if "python" not in RUNTIME_ONLY.get(k, set())}
        documented = sorted((set(cat) ^ set(pcat)) - only_here - only_py)
        print(f"     documented runtime-specific: {documented}", flush=True)
        need(not only_here and not only_py,
             f"the same bundle kinds as python: only on {name} {sorted(only_here)}, "
             f"only on python {sorted(only_py)}")
        shared = sorted(set(cat) & set(pcat))
        verb_diff = {k: {"missing": sorted(pdocs[k] - docs[k]), "extra": sorted(docs[k] - pdocs[k])}
                     for k in shared if docs[k] != pdocs[k]}
        need(not verb_diff, f"per kind, the same docs verbs as python: {json.dumps(verb_diff)[:600]}")
        no_schema = [k for k in shared if isinstance(pcat[k].get("schema"), dict)
                     != isinstance(cat[k].get("schema"), dict)]
        need(not no_schema, f"a schema exactly where python has one: {no_schema}")
        field_diff = {k: {"missing": sorted(schema_fields(pcat[k]) - schema_fields(cat[k])),
                          "extra": sorted(schema_fields(cat[k]) - schema_fields(pcat[k]))}
                      for k in shared if schema_fields(cat[k]) != schema_fields(pcat[k])}
        need(not field_diff, f"per kind, the same schema fields as python: {json.dumps(field_diff)[:600]}")
    finally:
        hk.close()
        if ref is not None:
            ref.close()
        shutil.rmtree(work, ignore_errors=True)
        if ref_work is not None:
            shutil.rmtree(ref_work, ignore_errors=True)

    verdict(not bad, f"{RUNTIME}: {len(bad)} failed" + (f" — first: {bad[0]}" if bad else ""))
    return 0 if not bad else 1


if __name__ == "__main__":
    sys.exit(main())
