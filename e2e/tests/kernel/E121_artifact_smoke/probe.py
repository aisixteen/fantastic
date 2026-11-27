"""kernel/artifact_smoke — the PACKAGED kernel boots and describes itself. NO MODEL.

On the packaged rust artifact:

  1. ONE-SHOT IN AN EMPTY DIR: `<artifact> reflect` in a fresh empty directory
     exits 0 and prints the root reflect (`id: kernel_state`, `runtime: RUNTIME`);
  2. BOOTED (the driver's seed, every call over `web_rest`): the root's runtime
     frame is the operator's — `rest`'s root reflect carries none of `runtime`,
     `env`, `version`, `peers`, `guid`; after the stop the operator's one-shot
     `<artifact> reflect` on the same directory carries the documented keys
     (`runtime → env → version`, `peers`, `persistence {provider}` naming the
     seeded record store, a minted `guid`, `tree` nesting `web` and `records`) and
     nothing that says how to call (`emits`, `cancel`, `kernel_verbs`);
  5. THE KERNEL VERBS: the operator's one-shot `kernel reflect part=docs` lists
     the ten; `rest` (a `call: [reflect]` line on the root, a `create` line on
     its own tree) sees in its own `reflect kernel part=docs` exactly the tree
     verbs that `create` line gives (`TREE_VERBS`) and none of the others;
  6. `reflect rest bundles: "all"` (the leg's own tree: the whole catalog) lists
     the shipped bundle kinds, each with its `handler_module` (an AI backend kind
     with its record `schema`); one instance of EVERY listed kind is created under
     `rest` from its schema's required fields, and its `reflect part=docs` gives a
     oneliner for every verb it declares, `reflect` included (a kind that cannot
     be instantiated is named in `NO_INSTANCE` with its reason, catalog row only);
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

from helpers.host.driver import HostKernel, ensure_runtime, excluded, free_port, runtime
from helpers.verdict import verdict

REPO = E2E.parent
RUNTIME = os.environ.get("RUNTIME", "")

#: Kinds DOCUMENTED as runtime-specific: kind → the runtimes that ship it.
#: `foundation_models_backend` — Apple's on-device model, a Foundation Models bridge
#: rust links through a C ABI (src/lib/rust/README.md).
#: `user` — the person's agent (blueprint export and import): a `fantastic` CLI
#: product bundle in rust (`src/tui/bundles/fantastic-user`), not in the headless
#: kernel binary; python ships it in `bundled_agents/io/user`.
RUNTIME_ONLY = {
    "foundation_models_backend": {"rust"},
    "user": {"python"},
}

#: The distilled root reflect (kernel_state readme + CLAUDE.md "Universal patterns"),
#: as the operator sees it.
ROOT_KEYS = {
    "id", "sentence", "parent_id", "handler_module", "display_name",
    "runtime", "peers", "env", "version", "persistence", "guid", "tree",
}
#: The root's runtime frame: the operator's only.
OPERATOR_FRAME = {"runtime", "env", "version", "peers", "guid"}
#: A reflect says what an agent IS, never how to call it.
NEVER_ON_REFLECT = {"emits", "cancel", "more", "kernel_verbs"}
#: The kernel verbs, exactly ten (`await_set`/`await_cancel` removed, #795 Q1).
KERNEL_VERBS = {"create_agent", "update_agent", "delete_agent", "ps", "ledger",
                "ack_deadletter", "kernel_start", "cancel", "shutdown_agents",
                "kernel_stop"}
#: The root's tree verbs: the kernel gate judges each on its operand, so a viewer
#: holding a `create` line sees them in the root's docs (reflect.rs `may_tree_verb`).
TREE_VERBS = {"create_agent", "update_agent", "delete_agent"}
#: Minimal record values per kind, beyond what `minimal()` derives from the
#: schema's required fields. `{port}` is replaced with a free port.
MINIMAL = {
    "file_bridge": {"root": ".", "allow": ["."]},
    "sqlite_store": {"path": "smoke.sqlite"},
    "web": {"port": "{port}"},
    # a backend validates its record at create: a context window it cannot look up
    # (no server is ever reached here), a key by reference in the seeded `creds`
    "anthropic_backend": {"endpoint": "http://127.0.0.1:9/v1/messages", "context_window": 8192},
    "responses_backend": {"credentials": {"store": "creds", "ref": "responses"}},
    "vllm_backend": {"endpoint": "http://127.0.0.1:9/v1", "kv_slots": 1},
}
#: Kinds whose instance cannot be created under `rest`: kind → reason. They get
#: the catalog-row check only. A kind refused that is not named here is a finding.
NO_INSTANCE = {
    "kernel_state": "the root loader: one per kernel, never a child",
    "py_agent": "a base, not an agent (kernel/E117)",
}


def one_shot(spec: dict, cwd: Path | None = None, *args: str) -> tuple[int, dict | None, str]:
    """`<artifact> [args…]` → (exit code, parsed reply, raw); in a fresh empty dir
    when `cwd` is None."""
    d = cwd or Path(tempfile.mkdtemp(prefix="e2e-artifact-empty-"))
    try:
        r = subprocess.run([*spec["cmd"], *(args or ("reflect",))], cwd=d, capture_output=True,
                           check=False, text=True, timeout=120,
                           env={**os.environ, **(spec.get("env") or {})})
        try:
            return r.returncode, json.loads(r.stdout), r.stdout
        except ValueError:
            return r.returncode, None, (r.stdout + r.stderr)[-600:]
    finally:
        if cwd is None:
            shutil.rmtree(d, ignore_errors=True)


def catalog(hk: HostKernel, target: str) -> dict[str, dict]:
    """`reflect bundles=all` on `target` → {kind name: entry}."""
    r = hk.send(target, {"type": "reflect", "bundles": "all", "tree": "none"})
    return {b.get("name"): b for b in r.get("bundles") or [] if isinstance(b, dict)}


def schema_fields(entry: dict) -> set[str]:
    sch = entry.get("schema") or {}
    return {f.get("name") for f in sch.get("fields") or [] if isinstance(f, dict)}


def minimal(kind: str, entry: dict) -> dict:
    """The record fields for one instance of `kind`: `MINIMAL[kind]`, then a
    value per remaining REQUIRED schema field, by its type."""
    out = {k: (free_port() if v == "{port}" else v) for k, v in MINIMAL.get(kind, {}).items()}
    by_type = {"string": "x", "str": "x", "integer": 1, "int": 1, "number": 1,
               "boolean": False, "bool": False, "array": [], "list": [], "object": {}, "map": {}}
    for f in (entry.get("schema") or {}).get("fields") or []:
        if not isinstance(f, dict) or not f.get("required") or f.get("name") in out:
            continue
        full_ty = str(f.get("type") or "string")
        ty = full_ty.split()[0].lower()
        if "default" in f:
            out[f["name"]] = f["default"]
        elif ty.startswith("'"):  # a literal or a choice of literals: the first
            out[f["name"]] = ty.split("'")[1]
        elif "https?://" in full_ty:  # a URL by pattern: one nothing listens on
            out[f["name"]] = "http://127.0.0.1:9/"
        else:
            out[f["name"]] = by_type.get(ty, "x")
    return out


def operator_checks(spec: dict, work: Path, name: str, need) -> None:
    """After the stop, the OPERATOR's one-shots on the directory the kernel ran:
    the root's whole reflect (step 2) and the kernel verbs (step 5)."""
    code, rf, raw = one_shot(spec, work, "reflect")
    rf = rf if isinstance(rf, dict) else {}
    keys = set(rf)
    need(code == 0 and ROOT_KEYS <= keys,
         f"the operator's root reflect has the documented keys; exit {code}, missing "
         f"{sorted(ROOT_KEYS - keys)}" + ("" if rf else f" raw={raw[-200:]!r}"))
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
    code, dk, raw = one_shot(spec, work, "kernel", "reflect", "part=docs")
    kv = set((dk or {}).get("verbs") or {})
    need(code == 0 and KERNEL_VERBS <= kv,
         f"the operator's `kernel reflect part=docs` lists the kernel verbs; exit {code}, "
         f"missing {sorted(KERNEL_VERBS - kv)}" + ("" if dk else f" raw={raw[-200:]!r}"))


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
    hk = HostKernel(spec, work).boot()
    ref: HostKernel | None = None
    ref_work: Path | None = None
    root = hk.root_id
    try:
        # 2 · rest's root reflect: no operator frame
        rf = hk.send(root, {"type": "reflect"})
        keys = set(rf) if isinstance(rf, dict) else set()
        need(rf.get("id") == root and not (keys & OPERATOR_FRAME),
             f"rest's root reflect carries none of the operator frame: {sorted(keys & OPERATOR_FRAME)}")

        # 5 · rest's view of the kernel verbs
        rk = hk.send("kernel", {"type": "reflect", "part": "docs"})
        rkv = set(rk.get("verbs") or {}) if isinstance(rk, dict) else set()
        need(rkv & KERNEL_VERBS == TREE_VERBS,
             f"rest's reflect kernel part=docs lists exactly the tree verbs its `create` "
             f"line gives: {sorted(rkv & KERNEL_VERBS)} of {sorted(rkv)}, want {sorted(TREE_VERBS)}")

        # 6 · the shipped catalog, and every kind's docs on an instance
        cat = catalog(hk, "rest")
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
        undoc, uncreated = [], []
        for k, e in sorted(cat.items()):
            if k in NO_INSTANCE:
                need(e.get("handler_module") == f"{k}.tools",
                     f"{k}: catalog row only ({NO_INSTANCE[k]})")
                continue
            iid = f"smoke_{k}"
            made = hk.send(root, {"type": "create_agent", "parent_id": "rest", "id": iid,
                                  "handler_module": e.get("handler_module"), **minimal(k, e)})
            if not isinstance(made, dict) or made.get("id") != iid:
                uncreated.append((k, str(made)[:160]))
                continue
            d = hk.send(iid, {"type": "reflect", "part": "docs"})
            verbs = d.get("verbs") if isinstance(d, dict) else None
            if (d.get("error") or not isinstance(verbs, dict) or "reflect" not in verbs
                    or not all(isinstance(v, str) and v.strip() for v in verbs.values())):
                undoc.append((k, str(d)[:160]))
            docs[k] = set(verbs or {})
        need(not uncreated, f"one instance of every kind not in NO_INSTANCE is created: {uncreated}")
        need(not undoc, f"reflect <instance> part=docs gives a oneliner for every verb: {undoc}")

        # 2 + 5 · the operator's view, after the stop (one-shots take the lock)
        hk.close()
        operator_checks(spec, work, name, need)

        # 7 · vs python
        if name != "python" and excluded("python"):
            # Comparing the kernel with itself would pass vacuously: SKIP, never PASS.
            # A real failure in steps 1-6 still wins (run.py reads E2E-SKIP first, so it
            # is printed only when nothing failed).
            if bad:
                verdict(False, f"{RUNTIME}: {len(bad)} failed — first: {bad[0]}")
                return 1
            print("E2E-SKIP: step 7 (vs python) needs the python runtime, which the run excludes",
                  flush=True)
            return 0
        elif name != "python":
            ref_work = Path(tempfile.mkdtemp(prefix="e2e-artifact-ref-"))
            ref = HostKernel(ensure_runtime(REPO, "python"), ref_work).boot()
            pcat = catalog(ref, ref.root_id)
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
                     for k in shared if k in docs and docs[k] != pdocs[k]}
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
