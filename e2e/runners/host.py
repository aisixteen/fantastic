"""Run a SHARED scenario against a compiled kernel (rust | swift), FROM e2e.

The peer of `inproc_js.mjs`: same scenario file, same
assertions, different substrate. Which kernel is a PLUG — `KERNEL=rust` or
`KERNEL=swift` — exactly as the model is a plug in `backends/`.

ROLES, NOT BUNDLE NAMES. A compiled kernel runs only what is compiled into it,
so `memory` is `text_file` here while js registers a handler in
process. The scenario declares the role; each runner supplies the implementation
it has. That is what lets one scenario be shared across four kernels rather than
copied four times — and a role with no shipped equivalent is refused BY NAME,
because a number that looks comparable and is not is worse than an honest skip.
"""

from __future__ import annotations

import os
import sys
import tempfile
import time
from pathlib import Path

E2E = next(d for d in Path(__file__).resolve().parents if (d / ".e2e-root").exists())
sys.path.insert(0, str(E2E))

from backends import THINK_BUDGET
from helpers import verdict  # noqa: E402
from helpers.host.driver import BASE, HostKernel, ensure_runtime, runtime  # noqa: E402
from helpers.proxy import DEFAULT_QUESTION_ANSWER, find_question  # noqa: E402

REPO = E2E.parent

#: role → the SHIPPED bundle that plays it on a compiled kernel.
#:
#: `dispenser`, `flaky_plant` and `purchase_desk` are stateful fixtures with
#: bespoke verbs — python and js register them in process; a compiled kernel
#: would need a probe bundle compiled in. That is a REAL boundary.
ROLE_BUNDLE = {
    "memory": "text_file.tools",
    "render_sink": "cli.tools",
    "compute": "exec_bridge.tools",
    # STATEFUL FIXTURES. TEST-ONLY on every runtime, and gated so a default
    # build carries none of them: a rust feature (`scenario-probes`), a swift
    # define (`SCENARIO_PROBES`), a `PYTHONPATH` entry for python. The MODULE
    # NAMES are shared, which is what lets one map serve all three.
    "dispenser": "dispenser_probe.tools",
    "flaky_plant": "plant_probe.tools",
    "purchase_desk": "purchase_probe.tools",
}


def _scenario_path(scenario_id: str) -> Path:
    """Find a scenario by its BARE id inside its numbered `E<nn>_<name>` dir."""
    root = E2E / "tests" / "scenario"
    for d in sorted(root.iterdir()):
        if d.is_dir() and d.name.split("_", 1)[-1] == scenario_id:
            return d / "scenario.yaml"
    raise SystemExit(f"no scenario {scenario_id!r} under {root}")


def load(scenario_id: str) -> dict:
    import yaml

    return substitute(yaml.safe_load(_scenario_path(scenario_id).read_text()))


def substitute(sc: dict) -> dict:
    """Fill the per-run placeholders a scenario declares.

    `$NONCE` is minted HERE and `$DIGEST` derived from it, so the expected answer
    exists in no model's weights and no cache — which is what makes `compose`
    unfakeable rather than merely checked.
    """
    import hashlib
    import json as _json
    import secrets

    nonce = secrets.token_hex(8)
    digest = hashlib.sha256(nonce.encode()).hexdigest()[:12]
    raw = _json.dumps(sc).replace("$NONCE", nonce).replace("$DIGEST", digest)
    return _json.loads(raw)


def _extract_action(text: str) -> dict | None:
    """Pull the `action` object out of the model's proposal line.

    Walks to the MATCHING brace rather than regexing to the first `}` — the
    payload is nested, and a lazy match truncates it into unparseable JSON.
    """
    import json as _json

    start = text.find("{")
    if start < 0:
        return None
    depth = in_str = esc = 0
    for i, c in enumerate(text[start:], start):
        if esc:
            esc = 0
        elif c == "\\":
            esc = 1
        elif c == '"':
            in_str ^= 1
        elif not in_str:
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    try:
                        return _json.loads(text[start : i + 1]).get("action")
                    except Exception:
                        return None
    return None


def main(scenario_id: str) -> int:
    sc = load(scenario_id)
    rt, _ = runtime(REPO)
    # Make the kernel runnable WITH its scenario fixtures. A stale binary
    # without them reports `cannot play ['dispenser']`, which reads like a
    # design boundary when it is really a missing build flag — so it happens
    # here and a skip only ever means what it says.
    spec = ensure_runtime(REPO, rt)

    endpoint, model = os.environ.get("ENDPOINT"), os.environ.get("FANTASTIC_AI_MODEL")
    key = os.environ.get("ANTHROPIC_API_KEY")
    if not (endpoint and model and key):
        print("  ENDPOINT / FANTASTIC_AI_MODEL / ANTHROPIC_API_KEY required")
        verdict(False, "backend not named")
        return 1

    roster = sc.get("roster") or []
    missing = sorted({e["role"] for e in roster if e["role"] not in ROLE_BUNDLE})
    if missing:
        print(f"  SKIP — no compiled-kernel bundle plays {missing}.")
        print("  python and js register these in process; rust and swift would")
        print("  need a probe bundle compiled in.")
        print(f"\nE2E-SKIP: {rt} cannot play {missing}")
        return 0

    limits = sc.get("limits") or {}
    # Every id the driver will need to call, declared BEFORE boot — authority
    # flows from above, and `rest` may not raise its own (see driver docstring).
    reach = ["store", "ai", *[e["id"] for e in roster]]
    k = HostKernel(
        spec, Path(tempfile.mkdtemp(prefix=f"{rt}_{scenario_id}_")), reach=reach
    ).boot()
    try:
        print("=" * 74)
        print(f"  {rt.upper()} kernel  ·  {model}  ·  {sc['title']}  ·  :{k.port}")
        print("=" * 74)

        kv_ids = [e["id"] for e in roster if e["role"] == "memory"]
        store = k.create(
            parent_id=k.root_id,
            id="store",
            handler_module="file_bridge.tools",
            root=".fantastic",
            ingress_rule="allow_all_ingress",
            callable_by={**BASE, "rest": "*", "ai": "*", **{i: "*" for i in kv_ids}},
        )
        assert store.get("id"), store

        for entry in roster:
            meta = {
                "parent_id": k.root_id,
                "id": entry["id"],
                "handler_module": ROLE_BUNDLE[entry["role"]],
                "callable_by": {**BASE, "rest": "*", "ai": "*"},
            }
            if entry["role"] == "memory":
                # The text editor writes THROUGH the store — both gate halves.
                meta["target_id"] = store["id"]
                meta["can_call"] = {store["id"]: "*"}
            if entry["role"] == "compute":
                # The COMMAND allowlist is the only gate — regexes over the
                # full command line; the runner permits its own python.
                meta["allowed"] = [f"{sys.executable} -c .*"]
                meta["ingress_rule"] = "allow_all_ingress"
            rec = k.create(**meta)
            assert rec.get("id"), rec

        can_call = {**BASE, store["id"]: "*"}
        for peer in (sc.get("grants") or {}).get("ai_can_call", []):
            can_call[peer] = "*"
        ai = k.create(
            parent_id=k.root_id,
            id="ai",
            handler_module="anthropic_backend.tools",
            # VERBATIM on every runtime now — `endpoint` is the URL.
            # This used to strip `/messages` because rust and swift
            # appended it while python did not.
            endpoint=endpoint,
            model=model,
            file_bridge_id=store["id"],
            # ONE BUDGET (backends.THINK_BUDGET), not a second default here.
            max_tokens=int(os.environ.get("MAX_TOKENS") or THINK_BUDGET),
            can_call=can_call,
            callable_by={**BASE, "rest": "*"},
        )
        assert ai.get("id"), ai
        # NOTHING TO DELIVER. Every backend reads its key from the ENVIRONMENT,
        # and the kernel was spawned with this process's env — so the key is
        # already where it will be looked for. This used to reflect the agent
        # and push the key through `set_api_key` on whichever runtimes had it.
        assert key, "ANTHROPIC_API_KEY is unset — the backend will refuse"

        first = k.send("ai", {"type": "send", "text": sc["task"], "client_id": "probe"})
        print(f"\n  pass 1: {str(first)[:300]}")

        # THE HUMAN, where a scenario asks for one. Presses the button TWICE
        # with the id the MODEL minted. The AI holds no grant on the desk — that
        # it cannot buy is half the measurement — so this goes through `rest`,
        # and the ledger must make the second press harmless.
        pressed = None
        if any("human_double_click" in c for c in (sc.get("expect") or [])):
            # FROM THE DURABLE HISTORY, not from the send reply. The `send`
            # reply shape is NOT uniform: python and rust return the finished
            # text (`{response, final}`), while swift returns immediately with
            # `{queued, stream_id, message_id}` and the text lands later. That
            # divergence is real and is reported as such — but the assistant
            # turn reaches the chat store on every runtime, so reading it there
            # asks the same question of all three.
            action, waited = None, time.monotonic() + 60
            while action is None and time.monotonic() < waited:
                h = k.send("ai", {"type": "history", "client_id": "probe"})
                for m in reversed(
                    (h.get("messages") or []) if isinstance(h, dict) else []
                ):
                    if m.get("role") == "assistant":
                        action = _extract_action(str(m.get("content") or ""))
                        if action:
                            break
                if action is None:
                    time.sleep(2.0)
            print(f"\n  proposal: {action}")
            if action and action.get("target") and action.get("id"):
                for _ in range(2):
                    pressed = k.send(
                        action["target"],
                        action.get("payload") or {},
                        exec_id=action["id"],
                    )
                print(
                    f"  pressed twice with id {action['id']!r} -> {str(pressed)[:200]}"
                )

        # WAIT FOR THE CHAIN. The router runs detached, so `send` returns after
        # one pass and the work carries on. Poll the thing the scenario calls
        # falsifiable — never the model's prose.
        deadline = time.monotonic() + float(
            os.environ.get("IDLE", limits.get("idle_seconds", 100))
        )
        ok, seen, answered = False, {}, set()
        while time.monotonic() < deadline:
            # The model asked the human — answer so the chain RESUMES (once per
            # question). Options ignored: the answer is the model's call to make.
            h = k.send("ai", {"type": "history", "client_id": "probe"})
            for m in reversed((h.get("messages") or []) if isinstance(h, dict) else []):
                if m.get("role") == "assistant":
                    q = find_question(str(m.get("content") or ""))
                    if q:
                        rid = str(q.get("request_id") or q.get("question") or "q")
                        if rid not in answered:
                            answered.add(rid)
                            k.send(
                                "ai",
                                {
                                    "type": "send",
                                    "text": DEFAULT_QUESTION_ANSWER,
                                    "client_id": "probe",
                                },
                            )
                    break
            ok = True
            for check in sc.get("expect") or []:
                if "human_double_click" in check:
                    # Pressed twice, above. The assertion is that the SECOND
                    # press was harmless — measured on the desk's own tally by
                    # the `side_effects` check the scenario pairs with this one,
                    # so all that is required here is that a press happened.
                    if pressed is None or (
                        isinstance(pressed, dict) and pressed.get("error")
                    ):
                        ok = False
                    continue
                if "side_effects" in check:
                    # Ask the ROLE its own tally. Every probe answers `count`
                    # precisely so an assertion can read the side effect rather
                    # than the model's prose — a model that claims it bought once
                    # cannot fake the desk's counter.
                    q = check["side_effects"]
                    got = k.send(q["agent"], {"type": "count"})
                    n = got.get("count") if isinstance(got, dict) else None
                    seen[q["agent"]] = got
                    if (
                        n is None
                        or "exactly" in q
                        and n != q["exactly"]
                        or "at_least" in q
                        and n < q["at_least"]
                    ):
                        ok = False
                    continue
                q = check["read"]
                # memory is FILES now: the scenario's `key` is the document
                # path, and the value lives in the text_file `read` reply's
                # lines — match against the whole reply, not a `value` field.
                got = k.send(q["agent"], {"type": "read", "path": q["path"]})
                seen[q["agent"]] = got
                if check["contains"] not in str(got):
                    ok = False
            if ok:
                break
            time.sleep(2.0)

        # THE TRANSCRIPT. The chain ran where this runner could not see it, so a
        # failure reads as silence unless the agent is asked for its own history.
        hist = k.send("ai", {"type": "history", "client_id": "probe"})
        msgs = hist.get("messages") if isinstance(hist, dict) else None
        if isinstance(msgs, list):
            print(f"\n  --- {len(msgs)} turns ---")
            for m in msgs:
                print(
                    f"\n  {str(m.get('role', '?')).upper()}: {str(m.get('content', ''))[:700]}"
                )

        for agent, got in seen.items():
            print(f"\n  {agent}.read -> {got}")
        print(f"  RESULT: {'PASS' if ok else 'FAIL'}")
        verdict(ok, f"{rt} kernel")
        return 0 if ok else 1
    finally:
        k.close()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1] if len(sys.argv) > 1 else "dialog"))
