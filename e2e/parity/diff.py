#!/usr/bin/env python
"""The ONE diff tool: `self | port | status`, plus `--check-proofs`.

    diff.py self  --a <baseline>/a --b <baseline>/b
    diff.py port  --base <baseline>/a --other <stamp>/rust --rules … --residuals …
                  --proofs … --landed … [--root-alias core]
    diff.py status --diff <stamp>/diff

Three things it will not do, each for a reason the round-1/2 probes taught:

  * it does not normalise anything `rules.yaml` does not name, and every rule it
    applies is recorded on the line it changed, so a suppressed difference can
    always be traced to the rule that suppressed it;
  * it does not treat a MISSING FILE as agreement. A capture that produced fewer
    files than python is a diff line per file;
  * it does not let a residual pass silently. A residual that matches nothing is
    stale and fails the run; a residual whose items have all landed turns the
    line it matches into a REGRESSION.

`self` is the harness's own test: two python captures of the same immutable tree
must be byte-identical after normalisation, and every line it prints is a harness
defect — a missing rule, or a step that is not deterministic.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import re
import sys
from pathlib import Path

import yaml

sys.path.insert(0, str(Path(__file__).resolve().parent))
from record import HARNESS_FIELDS  # noqa: E402

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[1]
PLAN = REPO / "tmp/py_parity_plan.md"
#: The run root every capture is written under, kept in step with `spec.RUNS`
#: (not imported: `spec` boots a kernel environment, and `diff.py` reads files).
RUNS = REPO / "e2e/runs/parity"

# ── item ids ────────────────────────────────────────────────────────────────
ITEM_RE = re.compile(r"\b((?:A\d{1,2}|B\d|C)\.?\d*\.\d+|C\d+)\b")


def plan_items(plan: Path) -> set:
    """Every item id the plan defines. `--check-proofs` measures coverage against
    this set, so the harness cannot quietly stop asking about an item."""
    if not plan.exists():
        return set()
    text = plan.read_text()
    ids = set()
    # Stage A/B items are bullets: `- **A0.1** [both] …`.
    for m in re.finditer(r"^- \*\*((?:A|B)\d{1,2}\.\d+)\*\*", text, re.M):
        ids.add(m.group(1))
    # Stage C items are headings: `### C12 [both] [gate] …`. `C0` is explicitly
    # not counted (§0: conditional on D1(a)).
    for m in re.finditer(r"^### (C\d+) ", text, re.M):
        if m.group(1) != "C0":
            ids.add(m.group(1))
    return ids


# ── normalisation ───────────────────────────────────────────────────────────
class Norm:
    """rules.yaml, applied. One instance per FILE, because N4's id renaming is
    per file by first appearance."""

    TIME_KEYS = {"ts", "started_at", "queued_at", "created_at", "next_run", "last_run",
                 "elapsed", "duration_ms"}
    ID_KEYS = {"id", "ids", "parent", "task_id", "job_id", "message_id", "pid", "refs", "expand",
               "after", "next", "bundles_next", "view_ids", "killed", "covers",
               # an AI status event's `msg` — the id of the message it is about.
               # Measured 2026-09-30 on the python self-diff: `ai_paths` events
               # `msg` 1177 against 1180, every relationship the same.
               "msg",
               # a streaming part's anchor — the message id the turn answers
               # (`part_start.re`, owner 2026-10-04); measured on the python
               # self-diff: `re` 187 against 186, every relationship the same.
               "re"}
    COUNT_KEYS = {"bytes", "size", "body_len", "content-length", "__bytes_len__"}
    OSPID_KEYS = {"pid"}
    GUID_KEYS = {"guid", "owner_guid", "partner_guid"}

    HARNESS_ID = re.compile(r"^(wirecap-|p-|p:|x-|s-|kv-|CASE=|reuse-|parity-|probe$)")

    #: N8 — a port in TEXT only where a HOST or the word `port` puts one. The
    #: round-3 form was a bare `:(\d{4,5})`, which turned `"max_tokens":4096` into
    #: `<PORT:?>` in every prompt the capture recorded: a rule wide enough to eat
    #: an item. Measured on the python self-diff, 2026-09-20.
    PORT_AFTER_HOST = re.compile(
        r"(?<![\w.])((?:\d{1,3}(?:\.\d{1,3}){3}|localhost|\[::1\])):(\d{4,5})\b")
    PORT_AFTER_WORD = re.compile(
        r"((?<![\w])port[\"']?\s*(?:[:=]|\s)\s*[\"']?)(\d{4,5})\b")

    def __init__(self, ports: dict, root_alias: str | None, runtime: str) -> None:
        self.ports = {}
        for role, plist in (ports or {}).items():
            for p in (plist if isinstance(plist, list) else [plist]):
                self.ports[int(p)] = role
        self.root_alias = root_alias
        self.runtime = runtime
        self.ids: dict = {}
        self.fired: set = set()

    # ---- scalars ----
    def _id_placeholder(self, v):
        key = (type(v).__name__, str(v))
        if key not in self.ids:
            self.ids[key] = f"<ID#{len(self.ids) + 1}:{'int' if isinstance(v, int) else 'str'}>"
        self.fired.add("N4")
        return self.ids[key]

    def _is_minted_id(self, v) -> bool:
        if isinstance(v, bool):
            return False
        if isinstance(v, int):
            return True
        if isinstance(v, str) and v.isdigit():
            return True
        return False

    #: N4 — the documentation examples the kernel prints into every AI system
    #: prompt (`ai_core/core.py` `_SEND_TO_HOWTO` and the list-shape line), each
    #: with its numbers fenced off as literals (see `string`).
    DOC_EXAMPLES = (
        ("[from solver_1 · to solver_2, solver_3 · msg 412]",
         "[from solver_1 · to solver_2, solver_3 · msg <DOC:412>]"),
        ("<ids><item>12</item><item>15</item></ids>",
         "<ids><item><DOC:12></item><item><DOC:15></item></ids>"),
    )

    #: A capture's own work root is `runs/parity/w/<slot>/` (run.py --slot) — two
    #: characters that name the CAPTURE, never the runtime's behaviour.
    SLOT = re.compile(r"(/e2e/runs/parity/w/)[0-9A-Za-z]{2}(?=/)")

    def string(self, s: str) -> str:
        out = self.SLOT.sub(r"\1__", s)
        if self.root_alias:
            new = re.sub(rf"(?<![\w./-]){re.escape(self.root_alias)}(?![\w.-])",
                         "kernel_state", out)
            if new != out:
                self.fired.add("N1")
            out = new
        # N8 ports — by role, from the map the capture recorded. TWO forms, each
        # anchored: `<host>:<port>` and `port[:= ]<port>` (the second is how a
        # persisted `agent.yaml` and the cli's own lines carry one).
        def port_sub(m):
            p = int(m.group(2))
            if p in (9, 8080, 11434):
                return m.group(0)
            self.fired.add("N8")
            sep = ":" if m.re is self.PORT_AFTER_HOST else ""
            return f"{m.group(1)}{sep}<PORT:{self.ports.get(p, '?')}>"
        out = self.PORT_AFTER_HOST.sub(port_sub, out)
        out = self.PORT_AFTER_WORD.sub(port_sub, out)
        # N2 — the runtime NAMING ITSELF, in text. The key-based pass sees
        # `{"runtime": "rust"}` in a reflect; it never sees `runtime=rust` in the
        # system prompt the same reflect is rendered into, which put the token in
        # every one of the ctx family's 453 files.
        for pat, rep in (
            (r"(?<![\w.])runtime=(python|rust|swift)(?![\w.-])", "runtime=<RUNTIME>"),
            # …and the cli intro banner, whose SHAPE is A9.5 and whose first
            # field is the runtime naming itself.
            (r"(\[fantastic\] )(python|rust|swift)( ·)", r"\1<RUNTIME>\3"),
            # …and the same reflect field as JSON INSIDE TEXT — a cli stdout, a
            # REST body recorded as a string — quoted once or more (2026-10-01:
            # `"runtime": "python"` in `cli/call_reflect_kv.stdout`, 9 lines a port;
            # `\"runtime\":\"python\"` in a `wire/rest` body). The key-based pass
            # already gives the parsed field `<RUNTIME>`; this is the same value
            # where nothing parsed it.
            (r'(\\*"runtime\\*"\s*:\s*\\*")(python|rust|swift)(\\*")', r"\1<RUNTIME>\3"),
        ):
            new = re.sub(pat, rep, out)
            if new != out:
                self.fired.add("N2")
            out = new
        # N26 — the ports' bundle CATALOG names `foundation_models_backend`, Apple's
        # on-device model, which python does not ship (owner 2026-10-04, decision A).
        # Only that one entry of a `bundles=` list goes; every other name, and the
        # list's order, stay compared. The token counts derived from the longer
        # prompt are residual R-C1-1.
        new = re.sub(r"(bundles=[\w,]*?)foundation_models_backend,", r"\1", out)
        new = re.sub(r"(bundles=[\w,]*?),foundation_models_backend(?!\w)", r"\1", new)
        if new != out:
            self.fired.add("N26")
        out = new
        # N3 — a minted guid inside TEXT. The key-based pass sees `{"guid": …}` in
        # a reply; it never sees `guid=<hex32>` in the system prompt, `"guid":
        # "<hex32>"` in cli stdout, or the same inside an HTTP body recorded as a
        # string. Measured: 664 of the 1855 python self-diff lines were this one
        # token in a stub request body.
        new = re.sub(r"(guid[\"']?\s*[:=]\s*[\"']?)[0-9a-f]{32}\b", r"\1<GUID:hex32>", out)
        # …and as the kernel half of a QUALIFIED caller, `<guid>|<agent>` — every
        # bridge call names its sender that way (audit 2026-09-28). The agent
        # half stays compared.
        new = re.sub(r"(?<![0-9a-f])[0-9a-f]{32}(?=\|)", "<GUID:hex32>", new)
        if new != out:
            self.fired.add("N3")
        out = new
        # N23 — the vllm KV session table's conversation ids INSIDE TEXT: a reflect
        # rendered into a system prompt (`<conversations> <item>hex16</item> …`) and
        # the same list as embedded json. Each id is a digest over a prompt prefix
        # that embeds values already declared incomparable (N2, N3, N8), so it can
        # only differ. HOW MANY there are, and where, stays compared.
        def _kv_items(m):
            return re.sub(r"[0-9a-f]{16}", "<KV_CONVERSATION:hex16>", m.group(0))
        new = re.sub(r"<conversations>(?:(?:\s|\\n)*<item>[0-9a-f]{16}</item>)+"
                     r"(?:\s|\\n)*</conversations>", _kv_items, out)
        new = re.sub(r'\\?"conversations\\?"\s*:\s*\[(?:\s*\\?"[0-9a-f]{16}\\?"\s*,?)+\s*\]',
                     _kv_items, new)
        if new != out:
            self.fired.add("N23")
        out = new
        # N25 — the port's catalog without `foundation_models_backend`, in TEXT:
        # the comma-joined `bundles=…` a system prompt carries and a `"bundles":
        # [...]` id list inside an embedded json document. The PORT side only:
        # python ships no such bundle (owner, 2026-10-02).
        if self.runtime != "python":
            new = drop_fm_text(out)
            if new != out:
                self.fired.add("N25")
            out = new
        # N7 random tokens, by SHAPE.
        for pat, rep, tag in (
            (r"\bsnd_[0-9a-f]{8}\b", "<SEND_ID:snd_hex8>", "N7"),
            (r"\bsnd_[0-9A-F]{8}\b", "<SEND_ID:snd_HEX8>", "N7"),
            (r"\bsch_[0-9a-f]{6}\b", "<SCH:hex6>", "N7"),
            (r"_ws_[0-9a-f]{6}\b", "<WS_CLIENT:hex6>", "N7"),
            # an agent id may carry digits (`ai2:3d113b94`, a stream id — 2026-10-04)
            (r"\b[a-z_][a-z0-9_]*:[0-9a-f]{8}\b", "<CPID:hex8>", "N7"),
            # a bridge forward's frame id `<leg>-<boot nonce hex12>-<n>` (audit
            # IO-5): the nonce is minted at every boot; the leg and the count stay.
            (r"\b([a-z_][a-z0-9_]*)-[0-9a-f]{12}-(\d+)\b", r"\1-<NONCE:hex12>-\2", "N7"),
            (r"\b([a-z_]+)-(\d+)\b(?!\S)", r"<CORR:\1-#n>", "N7"),
        ):
            new = re.sub(pat, rep, out)
            if new != out:
                self.fired.add(tag)
            out = new
        # N6 ISO-8601 inside text.
        new = re.sub(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?",
                     "<TS:isostr>", out)
        if new != out:
            self.fired.add("N6")
        out = new
        # N5 OS pids inside text — including `pid=<n>` bare (the lock-held
        # complaint) and `"pid":<n>` inside an embedded json document (a terminal
        # reflect answered over HTTP).
        def _ospid(m):
            # A14.10: the stale-lock pids are the HARNESS's own fixtures (999999
            # and 0) and are left literal — a runtime that echoed a different one
            # back has to show up.
            return re.sub(r"\d+", lambda mm: mm.group(0) if mm.group(0) in ("999999", "0")
                          else "<OSPID>", m.group(0))
        for pat in (r"\(pid=\d+\)", r"\bpid=\d+", r"\bpid \d+", r"\bkill \d+",
                    r'"pid"\s*:\s*\d+'):
            new = re.sub(pat, _ospid, out)
            if new != out:
                self.fired.add("N5")
            out = new
        # N7 / N6 — a random token or a kernel timestamp inside an EMBEDDED json
        # document (a ws frame's `text`, an HTTP body, a cli stdout). The
        # key-based pass walks the OUTER document and never sees these; they are
        # the same rules, applied where the value is a string of json rather than
        # json. Each keeps the key, the quoting and the type.
        # `\\?"` — the same document reaches the diff quoted once (a ws frame's
        # `text`) and quoted twice (that frame inside a recorded request body).
        for pat, rep, tag in (
            (r'(\\?"send_id\\?"\s*:\s*\\?")[A-Za-z0-9_-]{11}(\\?")',
             r"\1<SEND_ID:b64url11>\2", "N7"),
            (r'(\\?"serial\\?"\s*:\s*\\?")[0-9a-f]{6}(\\?")', r"\1<SERIAL:hex6>\2", "N7"),
            (r'(\\?"(?:ts|started_at|queued_at|created_at|next_run|last_run|elapsed|'
             r'duration_ms)\\?"\s*:\s*)\d+\.\d+', r"\1<TS:float>", "N6"),
            (r'(\\?"(?:ts|started_at|queued_at|created_at|next_run|last_run|elapsed|'
             r'duration_ms)\\?"\s*:\s*)\d+(?![\d.])', r"\1<TS:int>", "N6"),
        ):
            new = re.sub(pat, rep, out)
            if new != out:
                self.fired.add(tag)
            out = new
        # N4 — a kernel-minted row id inside EMBEDDED json (a state_event's
        # `summary`, a frame's `text`): the same placeholder the key-based pass
        # gives `{"id": 715}`, so N19 pairs the two sides by position.
        new = re.sub(r'(\\?"(?:id|parent)\\?"\s*:\s*)(\d+)(?![\d.])',
                     lambda m: m.group(1) + self._id_placeholder(int(m.group(2))), out)
        if new != out:
            self.fired.add("N4")
        out = new
        # N10 — the recall header's live-task ESTIMATE. It is a token count over a
        # body whose ids have just been renamed, so it cannot be compared and its
        # PRESENCE and place in the sentence can.
        new = re.sub(r"live task: \d+", "live task: <EST>", out)
        if new != out:
            self.fired.add("N10")
        out = new
        # N10 — the `context_recall` header's DIGEST estimate, the same argument
        # as `live task:` above. The sentence is `… your budget for the body is
        # <N> tokens and the digests use\n<M>.`, and `<M>` is a token count over
        # the digest block printed right underneath it — a block of
        # `<id> · <kind> · "<title>" · <n>B · ↑<parent>` lines whose ids N4 has
        # just renamed and whose `<n>B` N10 has just dropped. Measured
        # 2026-09-21b, `ctx/stub/req_108.bin`: 2141 against 2137, with the
        # digest lines themselves IDENTICAL after normalisation — the four
        # tokens are python's four-digit row ids against the ports' three-digit
        # ones. The budget number beside it is NOT touched here: it is a
        # different class (the system block's own size) and it is owned by a
        # residual, so it stays a line.
        new = re.sub(r"(the digests use\s+)\d+", r"\1<EST>", out)
        if new != out:
            self.fired.add("N10")
        out = new
        # N10 — and the BUDGET in the same sentence, for the other half of the
        # reason. It is `window - reserve - the system block's own token count`,
        # and the system block carries `<RUNTIME>` — a placeholder whose
        # ORIGINAL is 4, 5 or 6 characters (N2), so the derived number is off by
        # one on a runtime whose name is shorter. Measured 2026-09-21b,
        # `ctx/req/ai_strip_context_recall.json`: 3869 against 3870, with the
        # rest of the prompt identical. The INPUTS are compared exactly — the
        # window and the reserve in `meta.json` and in the agent record — and so
        # is the SAME number where it is recorded as a FIELD
        # (`$.rows[n].budget`), which is a diff line owned by residual R-N2-1.
        # Only the copy embedded in a prompt, beside a digest block that is
        # already estimate-only, goes here.
        new = re.sub(r"(budget for the body is )\d+( tokens)", r"\1<EST>\2", out)
        if new != out:
            self.fired.add("N10")
        out = new
        # N4 — the kernel's OWN documentation examples are literals, not minted
        # ids. The send-to howto every AI system prompt carries shows a header
        # `[from solver_1 · to solver_2, solver_3 · msg 412]` and a list
        # `<ids><item>12</item><item>15</item></ids>`; both shapes are N4 anchors,
        # so the example numbers were renamed like ids — and a REAL row that
        # happened to be 412 then shared the example's placeholder on one run and
        # not on the other. Measured 2026-10-02 on the python self-diff,
        # `ctx/req/ai_compact_main_t11.json`: `↑<ID#3>` against `↑<ID#x1>`, the two
        # runs' row ids 3 apart. The example text stays compared, verbatim.
        for doc, lit in self.DOC_EXAMPLES:
            if doc in out:
                out = out.replace(doc, lit)
                self.fired.add("N4")
        # N4 text anchors.
        for pat in (r"\[reply to (\d+)", r"↑(\d+)",
                    # the same arrow ESCAPED, as a frame's `text` carries it
                    # (`\u2191118` — 2026-10-01, `wire/frames` 6 lines a port)
                    r"\\+u2191(\d+)",
                    r"in batch (\d+)", r"<pid>(\d+)</pid>",
                    r"· msg (\d+)\]",
                    # …and before the stream a message was previewed as
                    # (`[from ai2 · msg 1395 · stream ai2:…]`, owner 2026-10-04)
                    r"· msg (\d+) · stream ",
                    r"<task_id>(\d+)</task_id>", r"<message_id>(\d+)</message_id>", r"\[fold (\d+) ·", r"\[recall (\d+) ·",
                    r"msg (\d+) · re (\d+)", r"joined execution (\d+)", r"(?m)^(\d+) · ",
                    # the dedup note a settled reply carries — `[identical to the
                    # reply in row 718 — the same 6205 bytes …]`. Measured
                    # 2026-09-30 on the python self-diff (718 against 721).
                    r"reply in row (\d+)",
                    # The same digest line, reached as the RAW request body the
                    # stub recorded: there the newline is still `\` `n` inside a
                    # json string, so `^` never sees it.
                    r"\\n(\d+) · "):
            # BY SPAN, not `.replace`: the id's digits can also sit inside the
            # anchor itself — `\u2191` then id 91 is `\u219191`, and a replace of
            # "91" rewrote the arrow's own code too (2026-10-04, python self-diff).
            out = re.sub(pat, lambda m: (
                m.group(0)[:m.start(1) - m.start(0)]
                + self._id_placeholder(int(m.group(1)))
                + m.group(0)[m.end(1) - m.start(0):]), out)
        # N4 — the id counter's RESERVATION MARK, as the root record renders it
        # into a system prompt (`next_id=1001`). The kernel reserves ids in
        # blocks and persists the block's end, so the mark says how many ids the
        # run had consumed when it was rendered — and the loader's debounced
        # persistence consumes a different number on every run. Measured
        # 2026-09-30 on the python self-diff: `next_id=2001` against `1001` in
        # `store/stub/req_003.bin`, the rest of the prompt identical. Its
        # PRESENCE and place stay compared.
        new = re.sub(r"(\bnext_id=)\d+\b", r"\1<NEXT_ID>", out)
        if new != out:
            self.fired.add("N4")
        out = new
        # N4 — an id LIST in text. Two forms, both kernel-minted row ids:
        #   `<ids><item>1828</item></ids>` (the grep hint the kernel writes into a
        #   settled reply) and `"expand": [2606]` / `"view_ids": [8084, …]` inside
        #   an embedded json document. The `<item>` numbers are renamed ONLY
        #   inside an `<ids>` block, because `<to><item>5</item></to>` and
        #   `<options><item>true</item>` carry values the HARNESS chose.
        def _renumber_items(m):
            return re.sub(r"(?<=<item>)(\d+)(?=</item>)",
                          lambda mm: self._id_placeholder(int(mm.group(1))), m.group(0))
        # `(?:\s|\\n)` — the same block reaches the diff both as parsed text (a
        # real newline) and as the raw request body the stub recorded, where the
        # newline is still the two characters `\` `n` inside a json string.
        out = re.sub(r"<(ids|view_ids|refs|expand)>(?:(?:\s|\\n)*<item>\d+</item>)+"
                     r"(?:\s|\\n)*</\1>", _renumber_items, out)

        def _renumber_list(m):
            return m.group(1) + re.sub(
                r"\d+", lambda mm: self._id_placeholder(int(mm.group(0))), m.group(2)) + m.group(3)
        # `covers` too: a ledger flush row names the executions it wrote, and the
        # memory store's state event quotes that row as embedded json (measured
        # 2026-10-02, `wire/frames.jsonl` `"covers": [717, …]` against `[720, …]`,
        # the runs' ids 3 apart). `\\?"` — the same document quoted once more.
        out = re.sub(r'(\\?"(?:expand|ids|refs|view_ids|covers)\\?"\s*:\s*\[)([\d,\s]*)(\])',
                     _renumber_list, out)
        # N4 — a minted id inside an EMBEDDED json document (the fold prompt
        # quotes the rows it is folding as json). `pid` is left to N5: that one
        # is the operating system's.
        # `msg` too: an origin line quotes the `{to, msg}` it was sent with
        # (`origin: readings reflect {"to": […], "msg": 1414}`) — measured
        # 2026-09-30 on the python self-diff, 1414 against 1408.
        out = re.sub(r'(\\?"(?:id|task_id|job_id|message_id|parent_row|row_id|msg|re)\\?"\s*:\s*)(\d+)(?![\d.])',
                     lambda m: m.group(1) + self._id_placeholder(int(m.group(2))), out)
        # …and a job's id in an `origin` line, written `{job_id: <n>}` — a job's
        # id IS its kernel task id, a counter value like any other here.
        out = re.sub(r"(\bjob_id: )(\d+)\b",
                     lambda m: m.group(1) + self._id_placeholder(int(m.group(2))), out)
        # N10 — the `NNNB` of a MESSAGE DIGEST line. The digest is
        # `<id> · <role> · "<title>" · <n>B · ↑<parent>`: the count is over the
        # row's CONTENT, which is not in this line at all and which carries row
        # ids of its own. Two runs whose ids differ in digit width therefore
        # disagree on the byte count of a row they agree about — measured on
        # `store/stub/req_051..054`. It fires only on a line the id renaming has
        # already touched, so a digest line about a row with no id in it keeps
        # its count.
        new = re.sub(r"(<ID#\d+:(?:int|str)> · (?:(?!\\n)[^\n]){0,300}?· )(\d+)(B · ↑)",
                     r"\1<LEN:renamed-ids>\3", out)
        if new != out:
            self.fired.add("N10")
        out = new
        # N7 tmpdir suffixes.
        new = re.sub(r"(/parity/tmp/[A-Za-z0-9_.-]*?)[a-z0-9_]{8}\b", r"\1<TMPSUFFIX>", out)
        if new != out:
            self.fired.add("N7")
        out = new
        return out

    #: N12 — rewritten while they are read (C47), so neither their sha nor the
    #: size a listing reports is a comparable number.
    REWRITTEN = ("lock.yaml", "schedules.yaml")
    #: Every field of a capture record that is a NUMBER OVER, or a DIGEST OF, one
    #: of those files. `store.py` records each persisted file three ways (a disk
    #: walk, a `read_stream` walk, the reassembled body), so the same moving
    #: length appears as `listed_size`, as the read's `length`, as the reply's
    #: `next_offset` and as the raw sidecar's byte count.
    REWRITTEN_NUM = {"sha256", "__sha256__", "size", "bytes_len", "len", "listed_size",
                     "length", "next_offset", "offset", "__bytes_len__",
                     "__raw_bytes_in_raw_file__"}
    REWRITTEN_TEXT = {"utf8", "__utf8__", "text", "head_hex"}
    #: N16 — a persisted file whose BYTES embed a value the rules have already
    #: declared incomparable: the root `agent.yaml` carries the kernel's minted
    #: guid (N3), a web leg's carries its OS-chosen port and an ai leg's the stub
    #: endpoint (N8). A digest over those bytes can only ever differ. The bytes
    #: themselves stay compared, normalised — in `raw/…`, in `disk_raw/…` and in
    #: the `utf8`/`text` field beside the digest.
    DIGEST_OF_MINTED = ("agent.yaml",)
    DIGEST_KEYS = {"sha256", "__sha256__"}

    _ORIGIN_NUMS = re.compile(r"(bytes )\d+-\d+( of )\d+|((?:offset|length): )\d+")

    @classmethod
    def _rewrite_origin(cls, x):
        if isinstance(x, dict):
            return {k: (cls._ORIGIN_NUMS.sub(
                        lambda m: (f"{m.group(1)}<REWRITTEN>-<REWRITTEN>{m.group(2)}<REWRITTEN>"
                                   if m.group(1) else f"{m.group(3)}<REWRITTEN>"), v)
                        if k == "origin" and isinstance(v, str) else cls._rewrite_origin(v))
                    for k, v in x.items()}
        if isinstance(x, list):
            return [cls._rewrite_origin(e) for e in x]
        return x

    @classmethod
    def _blank(cls, x, num, text, mark: str):
        if isinstance(x, dict):
            return {k: (mark if (k in num or k in text) else cls._blank(v, num, text, mark))
                    for k, v in x.items()}
        if isinstance(x, list):
            return [cls._blank(e, num, text, mark) for e in x]
        return x

    def value(self, key, v):
        if isinstance(v, bool) or v is None:
            return v
        if isinstance(v, str):
            # N7 — the same header names, where a stub recorded the handshake as
            # a DICT rather than as a list of pairs (`bridges/*_frames.json`).
            if isinstance(key, str) and key.lower() in self.HEADER_VALUE:
                self.fired.add("N6" if "TS" in self.HEADER_VALUE[key.lower()] else "N7")
                return self.HEADER_VALUE[key.lower()]
            if key in self.GUID_KEYS and re.fullmatch(r"[0-9a-f]{32}", v):
                self.fired.add("N3")
                return "<GUID:hex32>"
            if key in self.TIME_KEYS:
                self.fired.add("N6")
                return "<TS:str>"
            if key in self.ID_KEYS and v.isdigit() and not self.HARNESS_ID.match(v):
                return self._id_placeholder(v)
            if key == "send_id":
                self.fired.add("N7")
                if re.fullmatch(r"[A-Za-z0-9_-]{11}", v):
                    return "<SEND_ID:b64url11>"
                if re.fullmatch(r"snd_[0-9a-f]{8}", v):
                    return "<SEND_ID:snd_hex8>"
                return f"<SEND_ID:len={len(v)}>"
            if key == "serial" and re.fullmatch(r"[0-9a-f]{6}", v):
                self.fired.add("N7")
                return "<SERIAL:hex6>"
            if key == "runtime":
                self.fired.add("N2")
                return "<RUNTIME>"
            if key == "conversations" and re.fullmatch(r"[0-9a-f]{16}", v):
                # N23 — a KV session-table conversation id (see `string`).
                self.fired.add("N23")
                return "<KV_CONVERSATION:hex16>"
            return self.string(v)
        if isinstance(v, (int, float)):
            if key in self.TIME_KEYS:
                self.fired.add("N6")
                return f"<TS:{'int' if isinstance(v, int) else 'float'}>"
            if key in self.OSPID_KEYS and isinstance(v, int) and v > 1000:
                self.fired.add("N5")
                return "<OSPID>"
            if key in self.ID_KEYS and self._is_minted_id(v):
                return self._id_placeholder(v)
            if key == "port" and isinstance(v, int):
                if v not in (9, 8080, 11434):
                    self.fired.add("N8")
                    return f"<PORT:{self.ports.get(v, '?')}>"
            return v
        return v

    #: Response headers whose VALUE is not comparable but whose PRESENCE and
    #: ORDER are. A header arrives as a `[name, value]` pair, so `value()` never
    #: sees the name — this is where the name is available.
    HEADER_VALUE = {
        "date": "<TS:httpdate>",
        "sec-websocket-accept": "<WS_ACCEPT>",
        # N7 — 16 random bytes the CLIENT mints per connection (RFC 6455 §4.1).
        # The kernel never chooses it; what the kernel does with it (the accept
        # it derives) is `sec-websocket-accept`, one line above.
        "sec-websocket-key": "<WS_KEY>",
        "last-modified": "<TS:httpdate>",
        "etag": "<ETAG>",
    }

    def header_pair(self, pair):
        name = pair[0]
        low = name.lower() if isinstance(name, str) else name
        if low in self.HEADER_VALUE:
            self.fired.add("N6" if "TS" in self.HEADER_VALUE[low] else "N7")
            return [name, self.HEADER_VALUE[low]]
        return [self.value(low, pair[0]) if False else name,
                self.value(low, pair[1]) if isinstance(pair[1], str) else pair[1]]

    def walk(self, x, key=None):
        if isinstance(x, dict):
            # N12 / N16: which FILE this record is about. It is the entry's own
            # `path`/`name` where the family wrote one, and the KEY it hangs
            # under where the family used the path as the key (cli's snapshots).
            name = x.get("path") or x.get("name")
            if not isinstance(name, str) and isinstance(key, str):
                name = key
            if isinstance(name, str) and any(name.endswith(r) for r in self.REWRITTEN):
                # N12: a record of a continuously rewritten file keeps its PATH,
                # its keys and its envelope, and loses every number over its
                # moving bytes and the bytes themselves.
                self.fired.add("N12")
                x = self._blank(x, self.REWRITTEN_NUM, self.REWRITTEN_TEXT, "<REWRITTEN>")
                # …and the read's `origin` line, which REPEATS those numbers in
                # words (`bytes 0-193 of 193 — read_stream {…, length: 193}`):
                # measured 2026-10-01, `schedules.yaml` 193 vs 195 — its float
                # `next_run` printed to a different digit count. The words, the
                # path and the verb stay compared.
                x = self._rewrite_origin(x)
            elif isinstance(name, str) and any(name.endswith(d) for d in self.DIGEST_OF_MINTED):
                self.fired.add("N16")
                x = {k: ("<DIGEST_OF_MINTED>" if k in self.DIGEST_KEYS else v)
                     for k, v in x.items()}
            # N10 — the stub's request index. `body_len`, and the `content-length`
            # the stub echoes beside it, count a body that is recorded WHOLE as
            # `req_<n>.bin` and compared there; that body carries row ids N4 has
            # renamed, and ids of different digit widths make the same prompt a
            # different number of bytes.
            if {"n", "body_len", "headers", "answered"} <= set(x):
                self.fired.add("N10")
                x = {**x, "body_len": "<LEN:req_body>",
                     "headers": [[h[0], "<LEN:req_body>"]
                                 if isinstance(h, list) and len(h) == 2
                                 and str(h[0]).lower() == "content-length" else h
                                 for h in (x.get("headers") or [])]}
            # N10 — the `ctx` family's request CENSUS, the same argument as the
            # stub index one line above. `bytes` is the length of the request
            # body and `system_chars` the length of its system block; BOTH are
            # recorded WHOLE as `ctx/stub/req_<n>.bin` and compared there, line
            # by line, after normalisation. What makes them incomparable AS
            # NUMBERS is measured, and is two things the rules have already
            # declared incomparable: the `runtime=` word (N2 — `runtime=python`
            # is one byte longer than `runtime=swift`, two longer than
            # `runtime=rust`: `system_chars` 23214 vs 23213, `bytes` 23815 vs
            # 23814/23813, on 154 of the 412 ctx diff lines), and the id DIGIT
            # WIDTH (N4 — python executes ~379 `ledger.json` writes against the
            # ports' ~102, so its row ids reach four digits where theirs are
            # three, and a `context_recall` body that quotes 18 of them is 18
            # bytes longer: `ctx/reboot/stub_requests_during_boot.json` 2196 vs
            # 2178).
            # A ZERO IS KEPT AS ZERO on purpose: `system_chars: 0` says the
            # request carried NO system block at all, and a port that stopped
            # sending one must not land in the same bucket as a port that sent a
            # slightly shorter one.
            if {"n", "bytes", "system_chars", "messages", "keys"} <= set(x):
                self.fired.add("N10")
                x = {**x, **{ck: "<LEN:req_body>"
                             for ck in ("bytes", "system_chars")
                             if isinstance(x.get(ck), int)
                             and not isinstance(x.get(ck), bool) and x[ck] > 0}}
            # N10 — a ledger EXECUTION's outcome `bytes`: the size of the reply
            # that settled it. That reply is recorded WHOLE in its own file by
            # the family that made the call and compared there, and it carries
            # row ids N4 renames, so the same reply is a different number of
            # bytes on two runs. Measured 2026-09-30 on the python self-diff:
            # `tree/kernel_ledger_after.json` 450 against 452 and 19970 against
            # 19972. A ZERO IS KEPT AS ZERO (an outcome with no body is a
            # different thing), and `status`/`attempts` beside it stay exact.
            if {"pid", "status", "bytes", "attempts"} <= set(x) \
                    and isinstance(x.get("bytes"), int) and not isinstance(x["bytes"], bool) \
                    and x["bytes"] > 0:
                self.fired.add("N10")
                x = {**x, "bytes": "<LEN:reply>"}
            out = {}
            for k, v in x.items():
                # N14 — everything the harness owns, dropped by SCHEMA: the
                # `harness` container, and the same field names where a stub
                # logger writes them at the top level of its own entry.
                if k == "harness" or k in HARNESS_FIELDS:
                    self.fired.add("N14")
                    continue
                # N25 — a catalog LIST on the port side: the bundle python does
                # not ship leaves it, whether the entry is a name (`bundles=ids`)
                # or a `{name, handler_module, schema}` (`bundles=all`).
                if k == "bundles" and isinstance(v, list) and self.runtime != "python":
                    kept = drop_fm_entries(v)
                    if len(kept) != len(v):
                        self.fired.add("N25")
                        v = kept
                nk = self.string(k) if isinstance(k, str) else k
                if isinstance(v, (dict, list)):
                    out[nk] = self.walk(v, k)
                else:
                    out[nk] = self.value(k, v)
            # N10 — a count beside the TEXT it counts, where that text carries a
            # RENAMED MINTED ID. `<ID#12:int>` stands for a row id whose digits
            # the kernel chose, so the same document is a different number of
            # bytes on two runs; the text itself is compared, character for
            # character, in the same object. A count whose text has no renamed id
            # in it is compared exactly — that is where C44 and A2.21 live.
            # Only the placeholders whose ORIGINAL had no fixed width count here:
            # a renamed row id (a job's id is one), a timestamp and an OS pid. A
            # guid and a send_id are always the same number of characters, so a
            # count over a text carrying one of those is still compared exactly.
            # N10 — a memory-store ROW: `bytes` counts its content and `head` is
            # that content cut at a fixed width. When the head carries a renamed
            # id, the content does too, so the count goes; and the cut lands
            # where the raw digit widths put it, so the head is compared on the
            # prefix BOTH sides show — `compare_json` trims it, and only when the
            # two raw heads are the same length (the kernel's cap), recorded
            # here as `HEAD_RAW_LEN`. Measured 2026-09-30 on the python
            # self-diff: `store/s4/history_final.json` bytes 127 against 126,
            # head ending `</v` against `</ve`.
            if isinstance(out.get("head"), str) and isinstance(x.get("head"), str) \
                    and isinstance(out.get("bytes"), int) and not isinstance(out["bytes"], bool) \
                    and any(m in out["head"] for m in ("<ID#", "<TS:float>", "<TS:int>", "<OSPID>")):
                self.fired.add("N10")
                out["bytes"] = "<LEN:renamed-ids>"
                out[HEAD_RAW_LEN] = len(x["head"])
            texts = [out[t] for t in ("text", "utf8", "body", "stdout")
                     if isinstance(out.get(t), str)]
            body = out.get("body")
            if isinstance(body, dict):
                texts += [body[t] for t in ("text", "utf8") if isinstance(body.get(t), str)]
            # `<RUNTIME>` stands for a name of 4, 5 or 6 characters (N2), so a count
            # over a text carrying it moves with the runtime — measured 2026-10-01:
            # a REST reflect body 4645 vs 4643 (rust) / 4644 (swift) with its
            # content-length, a cli `stdout_bytes` 2996 vs 2994. The text itself is
            # compared beside the count, so a real difference still shows there.
            if texts and any(("<ID#" in t or "<TS:float>" in t or "<TS:int>" in t
                              or "<OSPID>" in t or "<RUNTIME>" in t) for t in texts):
                for ck in ("len", "body_len", "bytes_len", "size", "length", "stdout_bytes"):
                    if isinstance(out.get(ck), (int, float)) and not isinstance(out.get(ck), bool):
                        self.fired.add("N10")
                        out[ck] = "<LEN:renamed-ids>"
                # …and the `content-length` the same exchange carries, which is
                # that count again, one level up from the body it measures.
                hs = out.get("headers")
                if isinstance(hs, list):
                    out["headers"] = [[h[0], "<LEN:renamed-ids>"]
                                      if isinstance(h, list) and len(h) == 2
                                      and str(h[0]).lower() == "content-length" else h
                                      for h in hs]
            return out
        if isinstance(x, list):
            if (key == "headers" and all(isinstance(e, list) and len(e) == 2 for e in x)):
                return [self.header_pair(e) for e in x]
            # N11 — frames from SEVERAL SOCKETS in one list. Order inside one
            # socket is a contract and is kept exactly; the order in which two
            # independent sockets received the same broadcast is not, and the
            # harness recorded them interleaved by arrival. Grouping by `sock`
            # keeps every per-socket sequence and drops only the interleaving.
            if (key == "frames" and x
                    and all(isinstance(e, dict) and isinstance(e.get("sock"), str) for e in x)):
                self.fired.add("N11")
                by: dict = {}
                for e in x:
                    by.setdefault(e["sock"], []).append(self.walk(e, key))
                return {"__by_sock__": {s: by[s] for s in sorted(by)}}
            return [self.walk(e, key) if isinstance(e, (dict, list)) else self.value(key, e)
                    for e in x]
        return self.value(key, x)


# ── comparison ──────────────────────────────────────────────────────────────
def flatten(x, prefix="$"):
    out = {}
    if isinstance(x, dict):
        for k, v in x.items():
            out.update(flatten(v, f"{prefix}.{k}"))
        out[prefix + "\x00keys"] = list(x.keys())
    elif isinstance(x, list):
        for i, v in enumerate(x):
            out.update(flatten(v, f"{prefix}[{i}]"))
        out[prefix + "\x00len"] = len(x)
    else:
        out[prefix] = (type(x).__name__, x)
    return out


#: WHERE ORDER IS NOT A CONTRACT (N15 / N12). Measured by the python self-diff:
#: two runs of the SAME immutable tree disagreed on 4270 lines, and all of them
#: were one of these three.
#:
#:   `ai_paths/**` rows — the family runs two AI agents that call each other, so
#:     the two chains interleave differently every run and every row id after the
#:     first crossing shifts. The items are the content, the cycle depth and
#:     whether a guard fired, none of which is positional. `store/s3` is where the
#:     digest ORDER is an item, and this rule does not touch it.
#:   `ledger.json` / `messages.xml` — rewritten WHILE they are read (C47), so the
#:     row count itself moves between two reads of one run.
#:   `http.json` — the family's whole transport log. Each exchange is already
#:     recorded in its own case file; this is a dump whose order follows whichever
#:     concurrent step answered first.
UNORDERED = [
    ("ai_paths/**", ("rows",)),
    ("**/relay_frames.json", None),
    ("**/peer_frames.json", None),
]

def by_mark_and_sock(entries: list) -> list:
    """N11, on the whole-family frame stream.

    `wire/frames.jsonl` is every frame on every socket in one arrival-ordered
    list. Order inside ONE socket is a contract (C76 — a state stream within one
    subscription, B2.13 — a turn's token/status sequence) and is kept exactly.
    The order in which two INDEPENDENT sockets received the same broadcast is
    not: measured on the python self-diff, two runs of the same immutable tree
    swapped `ctrl` and `proto` at four places out of 400 frames.

    The stream is cut at each `mark` (the family's own phase boundary, written on
    sock `-`), and inside a phase the frames are grouped by socket. A frame that
    crossed a phase boundary, or moved within its own socket, is still a diff
    line.
    """
    out, phase = [], {}

    def flush():
        for s in sorted(phase):
            out.extend(phase[s])
        phase.clear()

    for e in entries:
        if not isinstance(e, dict) or not isinstance(e.get("sock"), str):
            flush()
            out.append(e)
            continue
        if e.get("dir") == "mark":
            flush()
            out.append(e)
            continue
        phase.setdefault(e["sock"], []).append(e)
    flush()
    return out


#: N17 — TERMINAL `output` EVENT COALESCING, owner ruling 2026-09-21.
#:
#: WHAT IT JOINS. A RUN of consecutive terminal `output` events on ONE ws leg
#: becomes ONE frame whose `data` is the concatenation of the run's `data`
#: fields, byte order preserved. A run is consecutive AFTER `by_mark_and_sock`,
#: so it is consecutive WITHIN ITS OWN LEG and WITHIN ONE PHASE. The run key
#: carries the leg (`sock`), the direction (`dir`), the frame kind and EVERY
#: payload field except `data` — so an `output` payload that names a different
#: source agent, or any other field that differs, starts a NEW run.
#:
#: WHAT IT NEVER JOINS. Anything that is not `{"type":"event","payload":
#: {"type":"output","data":<str>}}` breaks the run and is emitted untouched: a
#: reply, an error, a different event type, another leg's frame, a phase mark,
#: and an `output` payload with NO `data` key (python emits one at PTY EOF —
#: joining it would hide the missing key). A run of length 1 goes through the
#: same rebuild as a longer one, so a leg that never coalesced and a leg that
#: did produce the same shape.
#:
#: WHY. A PTY is a BYTE STREAM; where it is cut into `output` events is read
#: timing, not behaviour. Python's single event loop runs the pty fd callback
#: only after the in-flight write dispatch has finished, so one shell turn's
#: echo + output + prompt arrive in one read and are coalesced BY ACCIDENT; the
#: ports read on their own thread and see each write as it lands. No runtime has
#: coalescing code, so there is nothing to mirror.
#:
#: MEASURED (2026-09-21 acceptance captures, `wire/frames.jsonl`): python 3
#: `output` frames for the shell turn, rust 5 and then 4 after its drain fix,
#: swift 6-9. The CONCATENATED BYTES were identical in every capture. Because
#: the frame stream is diffed POSITIONALLY, one extra ctrl frame cascaded into
#: ~200 diff lines — the census read ctrl 175 (python) vs 176 (rust) vs 179
#: (swift) with every other leg matching exactly.
#:
#: WHAT IT MUST NOT HIDE. The concatenated bytes — a run whose joined `data`
#: differs is still a diff line, and that is the measurement. The ORDER of an
#: output run relative to every other frame type, which the run key and the
#: run-breaking rule above keep. A missing or extra output payload FIELD. A leg
#: that emitted no output at all (its run is absent, so the joined frame is
#: absent). The frame TOTAL is recomputed from the joined stream rather than
#: dropped, so a port that emits one non-output frame too many is still a line.
#:
#: WHAT IT DOES COST. The RAW `text` of an output frame is no longer compared
#: byte for byte: it is re-rendered from the joined payload on BOTH sides, so a
#: difference in JSON spacing or escaping INSIDE AN OUTPUT FRAME ONLY would not
#: show. Every other event frame still carries its recorded `text`, and that is
#: where such a difference would appear.
#:
#: THE DECISION. Normalise in the HARNESS. The alternatives were a kernel rule
#: serialising the PTY reader against in-flight dispatches (inventing latency to
#: imitate python's accident) and a coalescing timer (a new timing contract in
#: three runtimes). Neither is behaviour anyone wants; the harness is where the
#: difference actually lives.
def join_output_runs(entries: list) -> list:
    def run_key(e):
        if not isinstance(e, dict):
            return None
        j = e.get("json")
        if not isinstance(j, dict) or j.get("type") != "event":
            return None
        pl = j.get("payload")
        if not isinstance(pl, dict) or pl.get("type") != "output":
            return None
        if not isinstance(pl.get("data"), str):
            return None  # an `output` event with no `data` is not a chunk
        rest = tuple(sorted((k, json.dumps(v, sort_keys=True, default=str))
                            for k, v in pl.items() if k != "data"))
        return (e.get("sock"), e.get("dir"), e.get("kind"), rest)

    out, run = [], []

    def flush_run():
        if not run:
            return
        first = run[0]
        joined = {**first["json"]["payload"],
                  "data": "".join(e["json"]["payload"]["data"] for e in run)}
        frame = {k: v for k, v in first.items() if k != "text"}
        frame["json"] = {**first["json"], "payload": joined}
        # Re-rendered on BOTH sides from the joined payload, so the two agree
        # exactly when the payloads do (see WHAT IT DOES COST above).
        frame["text"] = json.dumps(frame["json"], ensure_ascii=False)
        out.append(frame)
        run.clear()

    key = None
    for e in entries:
        k = run_key(e)
        if k is None:
            flush_run()
            key = None
            out.append(e)
            continue
        if key is not None and k != key:
            flush_run()
        key = k
        run.append(e)
    flush_run()
    return out


def _frame_kind(e) -> str | None:
    """`reply` (a call's answer: `reply`/`error`), `event`, or None (anything else)."""
    if not isinstance(e, dict) or e.get("dir") != "in":
        return None
    j = e.get("json")
    t = j.get("type") if isinstance(j, dict) else None
    if t in ("reply", "error"):
        return "reply"
    if t == "event":
        pl = j.get("payload")
        # a terminal OUTPUT chunk is not one of these: its order against a reply
        # stays a contract (N17's "a reply between two chunks ends the run")
        if isinstance(pl, dict) and pl.get("type") == "output":
            return None
        return "event"
    return None


def replies_after_events(entries: list) -> list:
    """N20 — on ONE socket, the order of a call's REPLY relative to the EVENTS a
    watch pushes is not a contract: both are written by different tasks onto one
    socket, and which reaches it first is scheduling (measured 2026-10-01: the
    `ctrl` socket's `wirecap-N` reply one or two frames earlier or later among
    `reflect`/`status` events, rust 8 places, swift 1, in a 367-frame stream).
    Inside a RUN of consecutive incoming reply/event frames on one socket, the
    events keep their order, the replies keep theirs, and the replies are put
    after the events. Anything else — an outgoing frame, a mark, another socket,
    a frame of another type — ends the run, so a reply never crosses the call
    that caused it, and a missing, extra or reordered frame of one kind is still
    a line."""
    out, run = [], []

    def flush():
        out.extend([e for e in run if _frame_kind(e) == "event"])
        out.extend([e for e in run if _frame_kind(e) == "reply"])
        run.clear()

    for e in entries:
        k = _frame_kind(e)
        if k is None or (run and run[-1].get("sock") != e.get("sock")):
            flush()
        if k is None:
            out.append(e)
        else:
            run.append(e)
    flush()
    return out


def frames_view(entries: list) -> list:
    """The frame stream as every comparison sees it: N11 (`by_mark_and_sock`),
    N20 (`replies_after_events`), then N17 (`join_output_runs`)."""
    return join_output_runs(replies_after_events(by_mark_and_sock(entries)))


#: MEASURED, not assumed: two reads of the SAME python run disagree on these two
#: files by hundreds of lines. `ledger.json` is rewritten continuously (C47 — each
#: flush is itself an execution whose settle re-kicks the flusher) and
#: `messages.xml` grows while the snapshot walks it, so neither their bytes nor
#: their rows are a comparable document. What IS comparable, and what C42/C46/C47
#: are about, is their SHAPE: which row kinds appear and which keys each carries.
SHAPE_ONLY = ("ledger.json", "messages.xml")


def shape_of(x, depth: int = 0):
    """A document reduced to its key sets and value TYPES — no values."""
    if isinstance(x, dict):
        return {k: shape_of(v, depth + 1) for k, v in sorted(x.items())}
    if isinstance(x, list):
        # A SET KEYED BY THE SHAPE, not a list. The document this reduction exists
        # for is `ledger.json`, whose rows are already declared unordered
        # (N15/N12): two reads of one run see the same row KINDS in a different
        # order. v4 sorted the list, which fixed arrival order but NOT insertion:
        # `flatten` indexes a list POSITIONALLY, so one shape present on one side
        # only shifted every index after it and the file hit the 200-line cap.
        # Measured 2026-09-21, `store/store_files_final/disk_raw/ledger.json`:
        # 131 shapes (python) against 132 (rust) and 133 (swift), with only 2+3
        # and 1+3 shapes actually one-sided — 201 diff lines for 5 real ones.
        # Keying the dict BY the shape makes each one-sided shape exactly one
        # line, naming the shape, and hides nothing: a shape present on one side
        # only is still `<absent>` on the other. (Owner, 2026-09-21: this is N12's
        # `distinct_shapes: sorted` doing what it was written to do.)
        seen = {json.dumps(shape_of(e, depth + 1), sort_keys=True, default=str) for e in x}
        return {"__distinct_shapes__": {s: True for s in sorted(seen)}}
    return type(x).__name__


# ── N18 — THE LOADER'S OWN PERSISTENCE TRAFFIC. Owner ruling 2026-09-21. ─────
#
# WHAT IT NORMALISES. Before an ON-DISK `ledger.json` or `messages.xml` snapshot
# is compared, the loader's OWN persistence traffic is dropped from BOTH sides:
#
#   * in `ledger.json` — every row that satisfies the kernels' shared
#     `_is_own_persistence` predicate (python `kernel_state/tools.py`, rust
#     `persistence.rs::is_own_persistence`): a `call` row with NO origin (nobody
#     sent it; the loader is writing as itself) addressed to the DISCOVERED store
#     agent. The predicate is applied HERE to the recorded JSON — no kernel code
#     is imported, and the store is discovered FROM THE DOCUMENT (the target of
#     the no-origin rows whose result names `ledger.json` / `messages.xml`), not
#     hardcoded, so a runtime that wired a differently named store is not
#     silently matched. The discovered id is written into the compared document,
#     so two sides that discovered DIFFERENT stores are a diff line.
#   * in `messages.xml` — the `ledger_flush` / `message_pump_flush` /
#     `persist_writer` task rows, and any `settle` row whose parent is one of
#     them. (Own-persistence CALLS never reach the message log at all: that is
#     the first half of the same predicate, inside the kernel.)
#
# WHY IT IS NOT HIDING A DIFFERENCE. The write COUNT is a debounce artefact, not
# behaviour. Python runs one event loop, so a batch of rows is spread over more
# 150 ms debounce windows and each window writes; the ports burst and one write
# covers the batch. The rows the kernel actually executed are the same.
#
# MEASURED, 2026-09-21 acceptance captures, `store/store_files_final/disk_raw`:
#
#   |                              | python | rust | swift |
#   |------------------------------|--------|------|-------|
#   | `ledger.json` write rows     |   379  |  132 |  102  |
#   | `messages.xml` append rows   |    56  |   29 |   20  |
#   | own-persistence rows, total  |   535  |  261 |  222  |
#   | NON-persistence ledger rows  |   482  |  483 |  536  |
#   | persisted ledger rows        |  1017  |  744 |  758  |
#
# So on RUST the hypothesis holds exactly: 274 fewer rows on disk, ALL of them
# the loader's own, and the non-persistence total lands within the ±1 already
# known (the +1 decomposes as -1 `kernel_state`←`rest`, +1 `rest`←`web`, +1
# `ws`←`web`). On SWIFT it does NOT: swift carries 55 extra `kernel_state`←`ai`
# rows and 3 fewer `kernel_state`←`rest`. Those rows have an ORIGIN, so this rule
# never touches them — they are a real divergence and they stay in the diff.
#
# THE TAIL. `ledger.json` is rewritten WHILE the snapshot reads it (C47), so the
# last rows the kernel executed may not be on disk yet. After the drop, if one
# side's row list is a STRICT PREFIX of the other's, the surplus at the END of
# the longer one is that un-flushed tail and is cut from both. It is a prefix or
# the rule does not fire: a surplus in the MIDDLE is not a tail, it is a missing
# or an extra row, and it stays. (Measured on these three captures: the clause is
# INERT — the common prefix is 2 rows, the runtimes diverge immediately at the
# `rest` boot row, so nothing was cut.)
#
# WHAT IT MUST STILL FLAG, and how — the drop removes rows, so the assertion the
# rows carried is re-added explicitly as `__row_census__`, a DICT of row
# signature → count compared exactly:
#   * a MISSING or EXTRA non-persistence row — its signature's count differs, or
#     the signature is `<absent>` on one side. One line per signature, not one
#     per row.
#   * a WRONG PARENT — a `messages.xml` signature carries the PARENT ROW's own
#     (kind, verb, agent), resolved through the log, so a row re-parented onto a
#     different kind of row lands in a different bucket.
#   * a DIFFERING OUTCOME — the outcome status is part of every signature.
# A census is a projection and says nothing about a row's BODY; the bodies of
# these two files were already compared as SHAPE ONLY before this rule (N12 —
# they are rewritten while they are read), and that is unchanged.
#
# THE DECISION (owner, 2026-09-21): normalise in the HARNESS. The alternative was
# to make the ports' debounce imitate python's event-loop turn boundaries — a new
# timing contract in three runtimes, to make a write count match that measures
# nothing but which thread read the rows.

#: The loader's own task rows in `messages.xml`. `persist_writer` installs the
#: write-through, `ledger_flush` is the record flusher's debounce kick and
#: `message_pump_flush` is the pump's. None is work anyone asked for.
PERSISTENCE_TASK_VERBS = ("ledger_flush", "message_pump_flush", "persist_writer")


def discover_store(rows: list) -> str | None:
    """The store agent, DISCOVERED from the recorded rows — never hardcoded.

    It is the target of the operator-context (no origin) rows whose outcome names
    `ledger.json` or `messages.xml`: the only agent the loader persists THROUGH.
    No such row => no store was wired in this capture and N18 does not fire,
    which is the same answer both kernels give (`find_store` => None)."""
    for r in rows:
        # `origin` must be RECORDED. The live `ledger` verb's projection
        # (`*/final/kernel_ledger.json`) drops the field entirely, and reading a
        # missing key as "no origin" would drop every store row there, including
        # the ones an operator really sent. A row that does not carry the field
        # is not decidable, so N18 leaves it — and with no decidable row, no
        # store is discovered and the rule is inert on that file. Measured
        # 2026-09-21: it is inert on all six `final/kernel_ledger.json` captures
        # and fires on the four `store_files_*/…/ledger.json` snapshots.
        if not isinstance(r, dict) or "origin" not in r or r.get("origin") is not None:
            continue
        if r.get("kind") not in (None, "call"):
            continue
        res = (r.get("outcome") or {}).get("result")
        if not isinstance(res, dict):
            continue
        path = res.get("path")
        if isinstance(path, str) and path.split("/")[-1].split(".tmp.")[0] in (
                "ledger.json", "messages.xml"):
            who = r.get("target") or r.get("agent")
            if isinstance(who, str):
                return who
    return None


def drop_own_persistence(rows: list) -> tuple:
    """`_is_own_persistence`, applied to the RECORDED JSON. Returns (kept, store).

    A recorded ledger row has no `kind` field — every ledger row IS an execution,
    which is what the predicate's `kind == "call"` selects — so `None` is accepted
    there and `messages.xml`'s explicit `kind` is honoured where it is present."""
    store = discover_store(rows)
    if store is None:
        return rows, None
    kept = [r for r in rows
            if not (isinstance(r, dict)
                    and r.get("kind") in (None, "call")
                    and "origin" in r and r.get("origin") is None
                    and (r.get("target") or r.get("agent")) == store)]
    return kept, store


#: `<row …/>` or `<row …>…</row>`. The log is written by three runtimes and read
#: here as DATA; a full XML parse would fail the whole file on one stray byte,
#: where the census only needs the attributes and the outcome status.
#: The SELF-CLOSING form FIRST, and as its own alternative. A single pattern with
#: an optional `(.*?)</row>` tail reads `<row …/>` as an OPEN tag and swallows
#: every row up to the next `</row>` — measured 2026-09-21: it lost most of the
#: log, so parents resolved to `<parent not in log>` and the census printed 199
#: lines of nothing.
_ROW_RE = re.compile(r"<row\b([^>]*?)/>|<row\b([^>]*?)>(.*?)</row>", re.S)
_ATTR_RE = re.compile(r'(\w+)="([^"]*)"')


def parse_message_rows(text: str) -> list:
    rows = []
    for m in _ROW_RE.finditer(text):
        attrs = dict(_ATTR_RE.findall(m.group(1) if m.group(1) is not None else m.group(2)))
        st = re.search(r'<outcome\s+status="([^"]*)"', m.group(3) or "")
        attrs["__status__"] = st.group(1) if st else None
        rows.append(attrs)
    return rows


def drop_persistence_tasks(text: str) -> tuple:
    """N18 on `messages.xml`: the loader's own task rows, and the settles that
    belong to them. Returns (kept_rows, all_rows, text_without_those_elements).

    `all_rows` is returned because a parent must still resolve to the row it
    NAMES even when that row was dropped — that is how a row re-parented onto a
    flush task stays visible in the census. The filtered TEXT is returned because
    the tag-shape comparison runs over the document, and a `<row … verb="…">`
    shape that exists only because of a dropped row would otherwise come back as
    a tag-shape difference the drop was supposed to remove."""
    rows = parse_message_rows(text)
    dropped = {r.get("id") for r in rows
               if r.get("kind") == "task" and r.get("verb") in PERSISTENCE_TASK_VERBS}

    def is_dropped(r) -> bool:
        return (r.get("id") in dropped
                or (r.get("kind") == "settle" and r.get("parent") in dropped))

    kept = [r for r in rows if not is_dropped(r)]
    out, at = [], 0
    for m, r in zip(_ROW_RE.finditer(text), rows):
        if is_dropped(r):
            out.append(text[at:m.start()])
            at = m.end()
    out.append(text[at:])
    return kept, rows, "".join(out)


def message_census(kept: list, all_rows: list) -> dict:
    """Row signature → count for `messages.xml` (see N18).

    The signature carries the PARENT ROW's own (kind, verb, agent) — resolved
    through the whole log, not the kept subset — so a row re-parented onto a
    different kind of row moves to a different bucket and is a diff line."""
    by_id = {r.get("id"): r for r in all_rows}
    rows: dict = {}
    outcomes: dict = {}
    for r in kept:
        p = r.get("parent")
        if p is None:
            par = "-"
        elif p in by_id:
            q = by_id[p]
            par = f"{q.get('kind')}/{q.get('verb')}/{q.get('agent')}"
        else:
            par = "<parent not in log>"
        sig = (f"{r.get('kind')} · {r.get('verb')} · {r.get('agent')}"
               f" <- {r.get('origin')} · ^{par}")
        rows[sig] = rows.get(sig, 0) + 1
        st = r.get("__status__")
        if st is not None:
            outcomes.setdefault(sig, set()).add(st)
    # THE STATUS IS A SET, NOT A COUNT, and the signature above does not carry it.
    # `messages.xml` is appended to WHILE the snapshot reads it (C47), so a row
    # can be in the file with its `<outcome>` not yet written: measured
    # 2026-09-21, `call · read · totals ← ai` stood at 7 unsettled + 2 settled on
    # python against 0 + 9 on rust — the SAME NINE ROWS, read at two different
    # moments. Counting by status therefore measured the read, and putting the
    # status in the signature split every bucket in two. As a SET it still flags
    # what the owner asked for: a row that ERRORED on one runtime and COMPLETED
    # on the other is `["errored"]` against `["completed"]`, one line.
    return {"rows": rows, "outcomes": {k: sorted(v) for k, v in outcomes.items()}}


def ledger_census(rows: list) -> dict:
    """Row signature → count, for a `ledger.json` row array (see N18)."""
    out: dict = {}
    for r in rows:
        if not isinstance(r, dict):
            out["<non-object row>"] = out.get("<non-object row>", 0) + 1
            continue
        sig = (f"{r.get('target')} <- {r.get('origin')} · named={r.get('named')}"
               f" · name={r.get('name')} · {(r.get('outcome') or {}).get('status')}")
        out[sig] = out.get(sig, 0) + 1
    return out


def drop_unflushed_tail(a: list, b: list) -> tuple:
    """N18's TAIL clause. Cut the surplus at the END of the longer list IF the
    shorter one is a strict PREFIX of it — that surplus is what the other side had
    not flushed when the snapshot was taken. A surplus anywhere else is a missing
    or an extra row and is left alone."""
    sa = [json.dumps(x, sort_keys=True, default=str) for x in a]
    sb = [json.dumps(x, sort_keys=True, default=str) for x in b]
    n = min(len(sa), len(sb))
    if len(sa) != len(sb) and sa[:n] == sb[:n]:
        return a[:n], b[:n], True
    return a, b, False


def unordered_keys(rel: str):
    """`None` = the whole document is a bag; a tuple = only those array keys are."""
    for glob, keys in UNORDERED:
        if fnmatch.fnmatch(rel, glob):
            return keys if keys is not None else ("*",)
    return ()


def bagify(x, keys, top=False):
    """Replace a declared array with a SORTED MULTISET of its entries, so a
    different interleaving is not a different document. The LENGTH is kept as its
    own field: a runtime that produced fewer rows is still a diff line."""
    if isinstance(x, dict):
        out = {}
        for k, v in x.items():
            if isinstance(v, list) and (keys == ("*",) or k in keys):
                out[k] = {"__len__": len(v),
                          "__bag__": sorted((json.dumps(bagify(e, keys), sort_keys=True,
                                                        default=str) for e in v))}
            else:
                out[k] = bagify(v, keys)
        return out
    if isinstance(x, list):
        if top and keys == ("*",):
            return {"__len__": len(x),
                    "__bag__": sorted(json.dumps(bagify(e, keys), sort_keys=True, default=str)
                                      for e in x)}
        return [bagify(e, keys) for e in x]
    return x


_PH = re.compile(r"<ID#(\d+):(int|str)>")


#: N19 — ROW-ID ORDER WITHIN A TURN. Owner ruling 2026-09-21.
#:
#: WHAT IT NORMALISES. N4 replaces a minted row id with `<ID#n>` numbered BY
#: FIRST APPEARANCE, i.e. by MINT ORDER. This pass throws that numbering away and
#: re-derives it from the ROWS THEMSELVES: every dict whose `id` is an N4
#: placeholder is a row, each row gets a key of (agent, kind, verb, PARENT'S
#: canonical ordinal, hash of its own content with every id placeholder masked),
#: the rows are ranked by that key (parents before children, in rounds), and the
#: ordinal each row's id gets is its rank. A placeholder that no row defines —
#: an `expand` list, a `<pid>` in prompt text — is numbered afterwards, by first
#: appearance in the ALREADY-CANONICALISED document. The result is `<CID#k:type>`,
#: and the TYPE is still carried, so an int id and a digit-string id stay a
#: difference (C17, A3.10, A4.20, B5.9 — the same items N4 names).
#:
#: WHY. Python's single event loop cannot run a spawned task body until the
#: handler that spawned it SUSPENDS; rust's runs concurrently on another thread.
#: So within one turn 2–3 ids are handed out in a different order on the two
#: runtimes, and the sequence RE-CONVERGES on the next row — the same rows, the
#: same parents, the same content, a permuted id assignment.
#:
#: MEASURED (2026-09-21 acceptance captures, `ai_paths/**`): the SAME 408 ids on
#: both sides, 20 + 9 occurrences of the permutation, 1515 diff lines, and the
#: row bodies byte-identical apart from the ordinal (`"id": "<ID#36:int>"` on
#: python against `"<ID#34:int>"` on rust). The amplification is the placeholder
#: numbering, not the rows: one offset early in a file shifts every ordinal after
#: it, and each shifted row is its own line.
#:
#: THE DECISION (owner, 2026-09-21): normalise in the HARNESS. THREE kernel-side
#: mirrors were built and measured, and ALL THREE made the diff worse — the code
#: was reverted. Making a port imitate python's suspension point is inventing a
#: scheduling contract nobody wants, to make a number match that means nothing.
#:
#: WHAT IT MUST NOT HIDE, and why it does not:
#:   * A MISSING or EXTRA ROW. The rank is over the rows that are there, so a
#:     side with fewer rows has fewer ordinals, its array length differs, and the
#:     content of every row that cites the missing one differs too.
#:   * A WRONG PARENT. `parent` is a placeholder like any other, and it is
#:     rewritten to the parent's CANONICAL ordinal — which is derived from the
#:     parent's own content. A row re-parented onto a different row therefore
#:     renders a different `parent` and is a diff line. (Re-parenting onto a row
#:     with byte-identical content is indistinguishable, and so is not a
#:     difference anyone could observe.)
#:   * A DIFFERENT kind / verb / agent / content. All four are IN the key, so two
#:     rows that differ in any of them cannot be given each other's ordinal.
#:
#: WHAT IT SUPERSEDES. The old `renumber()`, which re-numbered placeholders in
#: the order they appeared in the BAGGED document. That was a fix for one
#: symptom of the same defect (a bag's arrival order) and it is circular: the bag
#: is sorted by strings that still carry the OLD numbering, so a permutation
#: changed the sort, which changed the new numbering. Deriving the ordinal from
#: content breaks the cycle, which is why this pass runs BEFORE `bagify` — the
#: bag then sorts strings that are already canonical.
#: HOW IT IS BOUNDED, and the measurement that forced the bound. The first cut
#: of this rule numbered ids from content on EACH SIDE INDEPENDENTLY. That is
#: unbounded, and it made the diff WORSE — 5435 → 7815 lines on the same rust
#: capture (measured 2026-09-21): where a row's body genuinely differs between
#: the runtimes, a content-derived ordinal scrambles for every row after it, so
#: the rule amplified exactly what it was written to damp.
#:
#: So the pass is TWO-SIDED and bounded by AGREEMENT. Rows are paired across the
#: two documents by their MASKED CONTENT — the row with every id placeholder
#: replaced by `<ID>` — and only rows whose masked content occurs the SAME NUMBER
#: OF TIMES on both sides are paired at all. A paired row's id gets the same
#: canonical ordinal on both sides; everything else falls back to N4's mint
#: order. That is the "window bounded by the rows whose ids differ only by that
#: permutation": inside a run of agreeing rows the ordinals are made to agree,
#: and the moment the rows stop agreeing the rule stops.
#:
#: `parent` is masked for the PAIRING and rendered canonically afterwards, which
#: is what keeps a re-parent visible: two rows can pair on their bodies and still
#: differ on the parent they name.
def pair_row_ids(doc_a, doc_b):
    """N19, both sides at once. Returns (a, b) with `<ID#n>` → `<CID#k>`."""
    rows_a, all_a = _row_ids(doc_a)
    rows_b, all_b = _row_ids(doc_b)
    buckets_a: dict = {}
    buckets_b: dict = {}
    for h, ph in rows_a:
        buckets_a.setdefault(h, []).append(ph)
    for h, ph in rows_b:
        buckets_b.setdefault(h, []).append(ph)
    pair: dict = {}          # B placeholder → A placeholder
    taken: set = set()
    for h, phs_b in buckets_b.items():
        phs_a = buckets_a.get(h)
        # SAME COUNT on both sides or no pairing: a masked body that occurs 3
        # times here and 4 times there is not a permutation, it is a missing or
        # an extra row, and pairing 3 of the 4 would hide which.
        if not phs_a or len(phs_a) != len(phs_b):
            continue
        for pb, pa in zip(phs_b, phs_a):
            if pb not in pair and pa not in taken:
                pair[pb] = pa
                taken.add(pa)
    # Canonical labels: side A in ITS document order, side B through the pairing.
    label_a: dict = {}
    for ph in all_a:
        if ph not in label_a:
            label_a[ph] = f"<CID#{len(label_a) + 1}:{_PH.fullmatch(ph).group(2)}>"
    # Whatever did not pair falls back to MINT ORDER against the A placeholders
    # that nothing claimed — exactly what N4 alone did, so this rule can never be
    # worse than not having it.
    spare = [ph for ph in all_a if ph not in taken]
    label_b: dict = {}
    k = 0
    for ph in all_b:
        if ph in label_b:
            continue
        typ = _PH.fullmatch(ph).group(2)
        if ph in pair:
            label_b[ph] = f"<CID#{label_a[pair[ph]][5:].split(':')[0]}:{typ}>"
        elif k < len(spare):
            label_b[ph] = f"<CID#{label_a[spare[k]][5:].split(':')[0]}:{typ}>"
            k += 1
        else:
            label_b[ph] = f"<CID#x{k - len(spare) + 1}:{typ}>"
            k += 1
    return _apply_map(doc_a, label_a), _apply_map(doc_b, label_b)


def _row_ids(doc):
    """(masked-body hash, placeholder) for every row, plus EVERY placeholder in
    the document in first-appearance order."""
    rows: list = []
    seen: dict = {}

    def collect(x):
        if isinstance(x, dict):
            rid = x.get("id")
            if isinstance(rid, str) and _PH.fullmatch(rid):
                rows.append((hashlib.sha1(
                    json.dumps(_mask_deep(x), sort_keys=True, default=str).encode()
                ).hexdigest()[:12], rid))
            for k, v in x.items():
                if isinstance(k, str):
                    _note(k, seen)
                collect(v)
        elif isinstance(x, list):
            for e in x:
                collect(e)
        elif isinstance(x, str):
            _note(x, seen)

    collect(doc)
    return rows, list(seen)


def _note(s: str, seen: dict) -> None:
    for m in _PH.finditer(s):
        seen.setdefault(m.group(0), True)


def _mask_deep(x):
    if isinstance(x, dict):
        return {(_PH.sub("<ID>", k) if isinstance(k, str) else k): _mask_deep(v)
                for k, v in x.items()}
    if isinstance(x, list):
        return [_mask_deep(e) for e in x]
    return _PH.sub("<ID>", x) if isinstance(x, str) else x


def _sub_ph(s: str, mapping: dict) -> str:
    return _PH.sub(lambda m: mapping.get(m.group(0), m.group(0)), s)


def _apply_map(x, mapping: dict):
    if isinstance(x, dict):
        return {(_sub_ph(k, mapping) if isinstance(k, str) else k): _apply_map(v, mapping)
                for k, v in x.items()}
    if isinstance(x, list):
        return [_apply_map(e, mapping) for e in x]
    return _sub_ph(x, mapping) if isinstance(x, str) else x


#: N10 — the raw length of a store row's `head`, stashed by `Norm.walk` for the
#: pairwise trim in `trim_heads` and removed before anything is compared.
HEAD_RAW_LEN = "__head_raw_len__"


def trim_heads(a, b, fired: set):
    """N10, both sides at once: a row `head` cut at the SAME raw width on both
    sides, where one normalised head is a prefix of the other, is compared on the
    prefix both show. Different raw widths, or a shared prefix that differs, stay
    a difference. Every `HEAD_RAW_LEN` mark is removed on the way out."""
    if isinstance(a, dict) and isinstance(b, dict):
        la, lb = a.pop(HEAD_RAW_LEN, None), b.pop(HEAD_RAW_LEN, None)
        ha, hb = a.get("head"), b.get("head")
        if la is not None and la == lb and isinstance(ha, str) and isinstance(hb, str) \
                and ha != hb and (ha.startswith(hb) or hb.startswith(ha)):
            fired.add("N10")
            a["head"] = b["head"] = min(ha, hb, key=len)
        for k in set(a) & set(b):
            trim_heads(a[k], b[k], fired)
    elif isinstance(a, list) and isinstance(b, list):
        for x, y in zip(a, b):
            trim_heads(x, y, fired)
    drop_head_marks(a)
    drop_head_marks(b)


def drop_head_marks(x):
    if isinstance(x, dict):
        x.pop(HEAD_RAW_LEN, None)
        for v in x.values():
            drop_head_marks(v)
    elif isinstance(x, list):
        for v in x:
            drop_head_marks(v)
    return x


#: N10 — a token BUDGET is `window − reserve − the system block's own tokens`,
#: and the system block names the runtime (`runtime=<RUNTIME>`, 4–6 characters,
#: ≤1 token at 4 chars a token). So the same budget is off by at most one token
#: between runtimes. Compared PAIRWISE with that tolerance — never blanked — so a
#: port computing it differently (off by more than one) is still a line.
#: Measured 2026-10-01: `<budget>3158</budget>` vs 3159 in a recall reply's text,
#: `"budget": 22301` vs 22302 in a view row, `$.budget` 3158 vs 3159.
_BUDGET_TEXT = re.compile(r'(<budget>|\\*"budget\\*"\s*:\s*)(\d+)')
BUDGET_TOLERANCE = 1


def tolerate_budgets(sa: str, sb: str, fired: set) -> tuple:
    """Pairwise: when both texts carry the same NUMBER of budgets and each pair is
    within `BUDGET_TOLERANCE`, the second text takes the first's numbers."""
    xa, xb = _BUDGET_TEXT.findall(sa), _BUDGET_TEXT.findall(sb)
    if not xa or len(xa) != len(xb):
        return sa, sb
    if any(p[0] != q[0] or abs(int(p[1]) - int(q[1])) > BUDGET_TOLERANCE
           for p, q in zip(xa, xb)):
        return sa, sb
    if all(p[1] == q[1] for p, q in zip(xa, xb)):
        return sa, sb
    it = iter(xa)
    fired.add("N10")
    return sa, _BUDGET_TEXT.sub(lambda m: m.group(1) + next(it)[1], sb)


def _budget_close(key: str, va, vb) -> bool:
    """A FIELD named `budget` (a recall reply's, a view row's), ±1 token."""
    last = key.split("\x00")[0].rsplit(".", 1)[-1]
    return (last == "budget" and isinstance(va, (list, tuple)) and isinstance(vb, (list, tuple))
            and len(va) == 2 and len(vb) == 2 and va[0] == vb[0] == "int"
            and abs(va[1] - vb[1]) <= BUDGET_TOLERANCE)


def _name_length_delta(key: str, va, vb, na: "Norm", nb: "Norm") -> bool:
    """N2 — a `stdout_bytes` whose stdout was recorded only as a HEAD (so N10
    cannot see the text it counts) and that differs by EXACTLY the difference in
    length of the two runtime names: the reflect it counts prints its runtime once
    (measured 2026-10-01: `setup.reflect` 3120 python vs 3118 rust, 3119 swift).
    Any other difference is still a line."""
    last = key.split("\x00")[0].rsplit(".", 1)[-1]
    if last != "stdout_bytes" or not (isinstance(va, (list, tuple))
                                      and isinstance(vb, (list, tuple))):
        return False
    if len(va) != 2 or len(vb) != 2 or va[0] != "int" or vb[0] != "int":
        return False
    if na.runtime not in ("python", "rust", "swift") or nb.runtime not in ("python", "rust", "swift"):
        return False
    return va[1] - vb[1] == len(na.runtime) - len(nb.runtime) != 0


def compare_json(path: str, a, b, na: Norm, nb: Norm) -> list:
    keys = unordered_keys(path)
    wa, wb = pair_row_ids(na.walk(a), nb.walk(b))
    trim_heads(wa, wb, na.fired)
    if keys:
        wa, wb = bagify(wa, keys, top=True), bagify(wb, keys, top=True)
    fa, fb = flatten(wa), flatten(wb)
    lines = []
    for key in sorted(set(fa) | set(fb)):
        va, vb = fa.get(key, "<absent>"), fb.get(key, "<absent>")
        if va == vb:
            continue
        if _budget_close(key, va, vb):
            na.fired.add("N10")
            continue
        if _name_length_delta(key, va, vb, na, nb):
            na.fired.add("N2")
            continue
        if (isinstance(va, (list, tuple)) and isinstance(vb, (list, tuple))
                and len(va) == len(vb) == 2 and va[0] == vb[0] == "str"):
            ta, tb = tolerate_budgets(va[1], vb[1], na.fired)
            if ta == tb:
                continue
        lines.append({"file": path, "where": key.replace("\x00", " · "),
                      "base": _short(va), "other": _short(vb),
                      "rules_fired": sorted(na.fired | nb.fired)})
        if len(lines) >= 200:
            lines.append({"file": path, "where": "$", "base": "…", "other": "…",
                          "note": "capped at 200 lines for this file"})
            break
    return lines


def _short(v, n: int = 400):
    s = v if isinstance(v, str) else json.dumps(v, ensure_ascii=False, default=str)
    return s if len(s) <= n else s[:n] + f"…(+{len(s) - n})"


def load(p: Path):
    raw = p.read_bytes()
    if p.suffix == ".json":
        try:
            return "json", json.loads(raw.decode("utf-8", "surrogatepass"))
        except ValueError:
            return "text", raw.decode("utf-8", "replace")
    if p.suffix == ".yaml":
        # A PERSISTED RECORD (`agent.yaml`, `lock.yaml`, `schedules.yaml`) is a
        # document: parsed, so the key-based rules (N3–N6) see its guid, its
        # clock and its pid, exactly as they did when the store wrote json.
        try:
            return "json", yaml.safe_load(raw.decode("utf-8"))
        except (ValueError, yaml.YAMLError):
            return "text", raw.decode("utf-8", "replace")
    if p.suffix == ".jsonl":
        out = []
        for line in raw.decode("utf-8", "surrogatepass").splitlines():
            if not line.strip():
                continue
            try:
                out.append(json.loads(line))
            except ValueError:
                out.append({"__unparsable_line__": line})
        return "jsonl", out
    try:
        return "text", raw.decode("utf-8")
    except UnicodeDecodeError:
        return "bin", raw


#: `meta.json` carries both what the harness was TOLD (the argv, the runtime name,
#: the workdirs) and what it MEASURED (the root id, which branch seeded it, the
#: context constants). Only the second half is a measurement; the first differs by
#: definition and would put a diff line in every family.
META_MEASURED = ("root_id", "seed_branch", "ctx_window", "ctx_reserve", "ctx_window_tiny",
                 "ai_window", "ai_reserve", "ai_fill_chars",
                 "ws_client_ids_discovered", "chains")


def joined_frame_total(p: Path):
    """The frame COUNT after N17, read from the `frames.jsonl` beside this file.

    `frames_index.json` records `{"total": n}` over the RAW stream, so the same
    PTY chunking N17 forgives inside the stream would come back as a one-line
    count difference here. RECOMPUTED, not dropped: a port that emits an extra
    frame of any OTHER kind still moves this number."""
    fj = p.parent / "frames.jsonl"
    if not fj.exists():
        return None
    kind, entries = load(fj)
    return len(frames_view(entries)) if kind == "jsonl" else None


#: N21 — the files whose AI event lists carry the two-delivery race below.
SELF_REFLECT_FILES = ("ai_paths/events.json", "ai_paths/cases/cycle.json")


def canon_self_reflect(events: list) -> list:
    """N21 — an agent busy with one turn is sent a PEER message (a `send` event
    with no `client_id`) and, a moment later, the reply its own earlier call was
    waiting for. The peer turn's self-`reflect` (the agent reflecting itself as
    its prompt is assembled) and the reply's DELIVERY race: python records the
    reflect right after the peer `send`; swift records the reply's `send` and its
    `queued` status first (measured 2026-10-01, `ai_paths/events.json` 386-388 and
    `cases/cycle.json`). Owner, 2026-10-01: that order is not a contract.

    Canonical form: the first `reflect` after a peer `send` is moved to right
    after it, but ONLY across `send` events and `queued` statuses. A `thinking`
    status (a turn STARTED) or anything else in between ends the search — so the
    case where the two turns themselves ran in the other order (rust: the reply's
    turn thinks first) is NOT reordered and stays a diff line."""
    out = list(events)
    i = 0
    while i < len(out):
        e = out[i]
        if isinstance(e, dict) and e.get("type") == "send" and e.get("client_id") is None:
            j = i + 1
            while j < len(out):
                f = out[j]
                if not isinstance(f, dict):
                    j = None
                    break
                if f.get("type") == "reflect":
                    break
                if f.get("type") == "send" or (f.get("type") == "status"
                                               and f.get("phase") == "queued"):
                    j += 1
                    continue
                j = None
                break
            else:
                j = None
            if j is not None and j > i + 1:
                out.insert(i + 1, out.pop(j))
        i += 1
    return out


def _canon_ai_events(rel: str, doc):
    if rel not in SELF_REFLECT_FILES or not isinstance(doc, dict):
        return doc
    return {k: (canon_self_reflect(v) if isinstance(v, list)
                and (rel.endswith("events.json") or k == "events") else v)
            for k, v in doc.items()}


def exclude_flush_rows(doc):
    """N24 — the memory store's reflect `rows` WITHOUT the kernel's `flush`
    bookkeeping rows. Returns `(doc, fired)`.

    Each flush appends ONE `flush` row beside the batch it writes, and how many
    flushes a run made is the flusher's debounce against the runtime's speed
    (measured: python 334, rust/swift 333 on the same scenario). `store.py`
    brackets the reflect with two `{rows, flush}` censuses of the whole store
    (one `dump` each) under `harness.flush_rows`. The census whose `rows` equals
    the reflect's is the store the reflect saw; its `flush` is subtracted and
    the count becomes `<ROWS_EXCLUDING_FLUSH:n>`, compared exactly. NOT FIRED —
    the raw count stays and is a line — when no census was taken, when none
    matches (two flushes inside the bracket), or when both match with different
    flush counts."""
    if not isinstance(doc, dict):
        return doc, False
    h, rows = doc.get("harness"), doc.get("rows")
    cens = h.get("flush_rows") if isinstance(h, dict) else None
    if not isinstance(cens, list) or not isinstance(rows, int) or isinstance(rows, bool):
        return doc, False
    flush = {c["flush"] for c in cens
             if isinstance(c, dict) and c.get("rows") == rows
             and isinstance(c.get("flush"), int) and not isinstance(c["flush"], bool)}
    if len(flush) != 1:
        return doc, False
    return {**doc, "rows": f"<ROWS_EXCLUDING_FLUSH:{rows - flush.pop()}>"}, True


#: N25 — python ships no `foundation_models_backend` (owner, 2026-10-02: py has
#: no FM); rust and swift do. Dropped from the PORT side of a catalog only.
FM_BUNDLE = "foundation_models_backend"
_FM_COMMA = re.compile(r"(?<![\w])bundles=([\w,]+)")
#: A `"bundles": [...]` of NAMES inside text (a cli stdout, a REST body recorded
#: as a string), quoted once or more. No bracket or brace inside: a
#: `bundles=all` list of objects is not a list of names.
_FM_ARRAY = re.compile(r'(\\*"bundles\\*"\s*:\s*\[)([^\[\]{}]*)(\])')


def _bundle_name(e):
    if isinstance(e, str):
        return e
    if isinstance(e, dict) and isinstance(e.get("name"), str):
        return e["name"]
    return None


def drop_fm_entries(lst: list) -> list:
    """N25 — a catalog list without the bundle python does not ship."""
    return [e for e in lst if _bundle_name(e) != FM_BUNDLE]


def drop_fm_text(s: str) -> str:
    """N25 — the same, in TEXT: the comma-joined `bundles=a,b,c` a system prompt
    carries, and a `"bundles": [...]` id list in compact or indented json. The
    entry goes with ONE separator, so the list around it is what python's is."""
    def comma(m):
        toks = m.group(1).split(",")
        kept = [t for t in toks if t != FM_BUNDLE]
        return m.group(0) if len(kept) == len(toks) else "bundles=" + ",".join(kept)

    def array(m):
        inner = m.group(2)
        name = re.escape(FM_BUNDLE)
        new = re.sub(rf'\s*(\\*"){name}\1\s*,', "", inner)              # first / middle
        new = re.sub(rf'\s*,\s*(\\*"){name}\1(?=\s*$)', "", new)         # last
        new = re.sub(rf'^\s*(\\*"){name}\1\s*$', "", new)                # the only one
        return m.group(1) + new + m.group(3)

    return _FM_ARRAY.sub(array, _FM_COMMA.sub(comma, s))


def repage_without_fm(doc):
    """N25 — a PAGED catalog walk (`families.page_walk` over a `bundles` reflect)
    on the port side, re-paged as if the bundle python does not ship were absent.
    Returns `(doc, fired)`.

    Dropping the entry from its page is not enough: every page boundary after
    it shifts by one (python: 16 names, 8 pages of 2; a port with the bundle: 17
    names, 9 pages, `[foundation_models_backend, kernel_state]` on page 3), so
    the cursor of every later page and the page count differ for that one name.

    The walk is re-paged ONLY when the port's own walk is a well-formed limit
    paging in python's convention — every page but the last holds exactly
    `limit` entries and names its last entry as `bundles_next`, each next page
    asks `after` that cursor, the last page has no cursor — so a port paging bug
    is never re-paged away: the walk is left as recorded and is a line. The pages
    are then rebuilt from the port's catalog minus the bundle, in chunks of
    `limit`: page j keeps every other field of the port's own page j (the last
    page those of the port's own last page, so its last-page convention is the
    port's), and the cursors and `after`s are derived the way the port's own
    walk derived them."""
    if not (isinstance(doc, dict) and isinstance(doc.get("pages"), list)
            and isinstance(doc.get("base_payload"), dict)):
        return doc, False
    base, pages = doc["base_payload"], doc["pages"]
    limit = base.get("limit")
    if ("bundles" not in base or not isinstance(limit, int) or isinstance(limit, bool)
            or limit < 1 or not pages or doc.get("stop") != "no_cursor"
            or doc.get("page_count") != len(pages)):
        return doc, False
    catalog = []
    for i, p in enumerate(pages):
        r = p.get("reply") if isinstance(p, dict) else None
        if not isinstance(r, dict) or not isinstance(r.get("bundles"), list):
            return doc, False
        names = [_bundle_name(e) for e in r["bundles"]]
        if None in names or p.get("payload") != (
                base if i == 0 else {**base, "after": pages[i - 1].get("cursor")}):
            return doc, False
        if i < len(pages) - 1:
            if not (len(names) == limit and p.get("cursor_key") == "bundles_next"
                    and p.get("cursor") == names[-1] == r.get("bundles_next")):
                return doc, False
        elif not (1 <= len(names) <= limit and p.get("cursor") is None
                  and r.get("bundles_next") is None):
            return doc, False
        catalog += r["bundles"]
    kept = drop_fm_entries(catalog)
    if len(kept) == len(catalog) or not kept:
        return doc, False
    chunks = [kept[i:i + limit] for i in range(0, len(kept), limit)]
    new_pages: list = []
    for j, chunk in enumerate(chunks):
        last = j == len(chunks) - 1
        tmpl = pages[-1] if last else pages[j]
        cur = None if last else _bundle_name(chunk[-1])
        reply = {k: (chunk if k == "bundles" else cur if k == "bundles_next" and not last else v)
                 for k, v in tmpl["reply"].items()}
        fields = {"payload": base if j == 0 else {**base, "after": new_pages[-1]["cursor"]},
                  "reply": reply, "cursor_key": None if last else "bundles_next",
                  "cursor": cur}
        new_pages.append({k: fields.get(k, v) for k, v in tmpl.items()})
    return {**doc, "page_count": len(new_pages), "pages": new_pages}, True


def file_lines(rel: str, pa: Path, pb: Path, ports_a, ports_b, root_alias, rt_b) -> list:
    ka, a = load(pa)
    kb, b = load(pb)
    if ka == "json" and kb == "json" and rel in SELF_REFLECT_FILES:
        a, b = _canon_ai_events(rel, a), _canon_ai_events(rel, b)
    if rel.endswith("/meta.json"):
        a = {k: v for k, v in (a or {}).items() if k in META_MEASURED}
        b = {k: v for k, v in (b or {}).items() if k in META_MEASURED}
    na = Norm(ports_a, None, "python")
    nb = Norm(ports_b, root_alias, rt_b)
    if ka == "json" and kb == "json":
        # N24 — both sides: the memory store's row count without its flush rows.
        a, fired_a = exclude_flush_rows(a)
        b, fired_b = exclude_flush_rows(b)
        if fired_a or fired_b:
            na.fired.add("N24")
        # N25 — the port side: a paged catalog walk re-paged without the bundle
        # python does not ship.
        if rt_b != "python":
            b, fired = repage_without_fm(b)
            if fired:
                nb.fired.add("N25")
    if ka == "text" and kb == "text" and rt_b != "python":
        # N25 — the port side, BEFORE the text is cut into lines: an indented
        # `"bundles": [` list puts the entry on a line of its own (a cli stdout).
        new = drop_fm_text(b)
        if new != b:
            nb.fired.add("N25")
            b = new
    if ka != kb:
        return [{"file": rel, "where": "$", "base": f"<{ka}>", "other": f"<{kb}>",
                 "note": "different file KIND"}]
    if ka == "bin":
        if hashlib.sha256(a).hexdigest() != hashlib.sha256(b).hexdigest():
            return [{"file": rel, "where": "$bytes",
                     "base": f"sha256={hashlib.sha256(a).hexdigest()} len={len(a)}",
                     "other": f"sha256={hashlib.sha256(b).hexdigest()} len={len(b)}"}]
        return []
    if ka == "text" and rel.endswith(SHAPE_ONLY):
        # N12 / SHAPE_ONLY on a text document (messages.xml): the comparable thing
        # is the set of distinct TAG SHAPES, not the entries — plus, since N18,
        # the row CENSUS of what is left once the loader's own persistence task
        # rows are dropped.
        def tags(doc):
            # N12 — A SET KEYED BY THE SHAPE, not a sorted list, for exactly the
            # reason `shape_of` keys `__distinct_shapes__` that way: `flatten`
            # indexes a list POSITIONALLY, so ONE tag shape present on one side
            # only shifted every index after it and turned a single one-sided
            # row shape into a ~110-line cascade (measured 2026-09-21b, swift,
            # `store/store_files_final/disk_raw/messages.xml`: 28 `__tag_shapes__`
            # lines for 2 one-sided shapes). Keyed, a one-sided shape is exactly
            # one line and it NAMES the shape; it hides nothing, because a shape
            # on one side only is still `<absent>` on the other.
            import re as _re
            return {t: True for t in sorted({_re.sub(r'="[^"]*"', '="…"', t)
                                             for t in _re.findall(r"<[^/][^>]*>", doc)})}
        def msg_doc(raw):
            kept, all_rows, filtered = drop_persistence_tasks(raw)
            return {"__tag_shapes__": tags(filtered),
                    "__row_census__": message_census(kept, all_rows)}
        da, db = msg_doc(a), msg_doc(b)
        # A signature that is settled on one side and NOT YET on the other is not
        # comparable — the file is appended to while it is read (C47), so which
        # rows already carry an `<outcome>` measures the moment of the read.
        # `rows` (row identity) is compared WHOLE; `outcomes` is compared on the
        # signatures BOTH sides have settled, where a row that errored against a
        # row that completed is still exactly one diff line.
        both = set(da["__row_census__"]["outcomes"]) & set(db["__row_census__"]["outcomes"])
        for d in (da, db):
            d["__row_census__"]["outcomes"] = {
                k: v for k, v in d["__row_census__"]["outcomes"].items() if k in both}
        return compare_json(rel, da, db, Norm({}, None, "python"), Norm({}, None, rt_b))
    if ka == "json" and rel.endswith("frames_index.json") and isinstance(a, dict) \
            and isinstance(b, dict) and "total" in a and "total" in b:
        ta, tb = joined_frame_total(pa), joined_frame_total(pb)
        if ta is not None and tb is not None:
            a, b = {**a, "total": ta}, {**b, "total": tb}
    if ka == "json" and rel.endswith(SHAPE_ONLY):
        # N18 — drop the loader's own persistence rows from BOTH sides, cut the
        # un-flushed tail if one side is a strict prefix of the other, and put
        # the row CENSUS back as an explicit assertion. Then N12's shape
        # reduction, as before, on what is left.
        rows_key = next((k for k in ("rows", "executions")
                         if isinstance(a, dict) and isinstance(a.get(k), list)
                         and isinstance(b, dict) and isinstance(b.get(k), list)), None)
        extra_a: dict = {}
        extra_b: dict = {}
        if rows_key:
            ra, store_a = drop_own_persistence(a[rows_key])
            rb, store_b = drop_own_persistence(b[rows_key])
            # The census is taken BEFORE the tail cut, ON PURPOSE. Cutting a tail
            # that happens to be a genuinely MISSING trailing row would hide it —
            # the rulecheck case "a MISSING non-persistence row" proves exactly
            # that, and failed until the census moved above this line. So the
            # tail is timing FOR THE SHAPE REDUCTION, where a tail row of a new
            # shape cascades, and never for the census, which still counts every
            # row either side flushed.
            census_a, census_b = ledger_census(ra), ledger_census(rb)
            ra, rb, cut = drop_unflushed_tail(ra, rb)
            a, b = {**a, rows_key: ra}, {**b, rows_key: rb}
            # The census and the discovered store id live OUTSIDE the shape
            # reduction: `shape_of` replaces every value with its TYPE, which
            # would turn a count of 55 into `"int"` and delete the one assertion
            # N18 owes. They are compared as VALUES.
            extra_a = {"__store__": store_a, "__row_census__": census_a}
            extra_b = {"__store__": store_b, "__row_census__": census_b}
            if cut:
                # The same string on BOTH sides: not a difference, a note to
                # whoever reads the normalised document.
                extra_a["__tail__"] = extra_b["__tail__"] = \
                    "<N18: un-flushed tail cut, strict prefix>"
        sa = {**extra_a, "__shape__": shape_of(drop_head_marks(na.walk(a)))}
        sb = {**extra_b, "__shape__": shape_of(drop_head_marks(nb.walk(b)))}
        if sa != sb:
            return compare_json(rel, sa, sb, Norm({}, None, "python"), Norm({}, None, rt_b))
        return []
    if ka == "text":
        # N11 — a log is compared as ORDERED LINES, so one extra line does not
        # shift every line after it. A text file named in UNORDERED (messages.xml,
        # rewritten while it is read) is compared as a bag of lines instead.
        la = [na.string(x) for x in a.splitlines()]
        lb = [nb.string(x) for x in b.splitlines()]
        if len(la) == len(lb):
            # N10 — the budget tolerance, line for line (a stub request body is
            # one line carrying every recall reply the conversation holds)
            lb = [tolerate_budgets(x, y, na.fired)[1] for x, y in zip(la, lb)]
        if unordered_keys(rel):
            la, lb = sorted(la), sorted(lb)
        lines = []
        import difflib

        for d in difflib.unified_diff(la, lb, lineterm="", n=0):
            if d.startswith(("---", "+++", "@@")):
                continue
            lines.append({"file": rel, "where": "line", "base": d[1:] if d[0] == "-" else "",
                          "other": d[1:] if d[0] == "+" else "",
                          "rules_fired": sorted(na.fired | nb.fired)})
            if len(lines) >= 200:
                break
        return lines
    if ka == "jsonl":
        if rel.endswith("frames.jsonl"):
            a, b = frames_view(a), frames_view(b)
        return compare_json(rel, a, b, na, nb)
    return compare_json(rel, a, b, na, nb)


# ── residuals and proofs ────────────────────────────────────────────────────
def load_yaml(p):
    import yaml

    return yaml.safe_load(Path(p).read_text()) if p else {}


def match_residual(line: dict, residuals: list, runtime: str) -> dict | None:
    blob = json.dumps(line, ensure_ascii=False, default=str)
    for r in residuals:
        if runtime not in (r.get("runtimes") or []):
            continue
        if not fnmatch.fnmatch(line["file"], r.get("file_glob", "**/*")):
            continue
        anchor = r.get("anchor") or ""
        if anchor and not re.search(anchor, blob):
            continue
        return r
    return None


def classify(lines: list, residuals: list, landed: list, proofs: dict, runtime: str):
    """Every line gets one of four verdicts, and the counts ARE the report."""
    out = []
    used = set()
    # item → selectors, so a line's file can name the items it belongs to.
    sel = (proofs or {}).get("by_selector") or {}
    for line in lines:
        items = sorted({i for glob, ids in sel.items()
                        if fnmatch.fnmatch(line["file"], glob) for i in ids})
        r = match_residual(line, residuals, runtime)
        verdict = "unmapped"
        if r is not None:
            used.add(r["id"])
            r_items = r.get("items") or []
            # ACCEPTED wins over landed.yaml: the owner decided this difference IS
            # the runtime's landed behaviour (residuals.yaml v6, 2026-10-01). A
            # PENDING residual whose items have all landed is a regression — the
            # undecided difference has outlived the claim.
            if r.get("state") == "accepted":
                verdict = "residual-accepted"
            elif r_items and all(i in landed for i in r_items):
                verdict = "regression"
            else:
                verdict = "residual-pending-owner"
        elif items:
            verdict = "regression" if all(i in landed for i in items) else "open-item"
        out.append({**line, "items": items, "verdict": verdict,
                    "residual": r["id"] if r else None})
    stale = [r["id"] for r in residuals
             if runtime in (r.get("runtimes") or []) and r["id"] not in used]
    return out, sorted(used), stale


# ── commands ────────────────────────────────────────────────────────────────
def walk_capture(root: Path) -> dict:
    """family/relative path → file, read from each family's manifest so a file a
    family forgot to list is caught here rather than compared silently."""
    out = {}
    for fam in sorted(p for p in root.iterdir() if p.is_dir()):
        man = fam / "manifest.json"
        if not man.exists():
            continue
        for e in json.loads(man.read_text()):
            out[f"{fam.name}/{e['path']}"] = fam / e["path"]
    return out


def ports_of(root: Path, family: str) -> dict:
    meta = root / family / "meta.json"
    if not meta.exists():
        return {}
    try:
        return json.loads(meta.read_text()).get("ports") or {}
    except ValueError:
        return {}


def harness_errors(root: Path) -> list:
    """Every `__harness_error__` a capture wrote, wherever it landed.

    `families.call` records a transport failure AS DATA in the file where the
    reply belonged — which is right (a capture never invents a reply) but means
    the marker can sit quietly inside a 400-line document. The CLEAN definition
    says zero of them, so they are counted here, across the whole capture, and
    reported before any diff line: a run with one is not a measurement of the
    port, it is a measurement of a socket that did not answer.
    """
    out = []
    for fam in sorted(p for p in root.iterdir() if p.is_dir()):
        for f in fam.rglob("*"):
            if not f.is_file() or f.suffix not in (".json", ".jsonl", ".txt"):
                continue
            try:
                raw = f.read_text(errors="replace")
            except OSError:
                continue
            n = raw.count("__harness_error__")
            if n:
                out.append({"file": f"{fam.name}/{f.relative_to(fam)}", "count": n})
    return out


#: The harness's own whole-family transport log. Every exchange that is a
#: MEASUREMENT is recorded inside its own case file (`bundle_verbs/*/…json`
#: carries its `http` block); this file is the dump beside them, its order is
#: whichever concurrent step answered first, and its bodies embed the ledger,
#: which is rewritten while it is read. Diffing it measures the recorder.
SKIP = ("http.json",)


#: N22 — the stub request bodies whose NUMBERING interleaves across agents.
STUB_REQ = re.compile(r"^ai_paths/stub/req_\d+\.bin$")
_STUB_AGENT = re.compile(r"\bid=(\w+)\s+model=")


def pair_stub_requests(fa: dict, fb: dict) -> dict:
    """N22 — `ai_paths` runs TWO AI agents, and the stub numbers their requests in
    ARRIVAL order across both: when `ai2`'s turn reaches the stub before `ai`'s,
    every number after it shifts (measured 2026-10-01: swift `req_068` is `ai2`'s
    request, python's is `req_069`, so both files read as wholly different). Each
    AGENT's own request sequence is a contract and stays compared, in order: the
    k-th request of agent X is paired with the other side's k-th request of X
    (the agent is the `id=<agent>  model=` line of the system block). A request
    one side made and the other did not is still a missing/extra file."""
    def by_agent(files: dict) -> dict:
        out: dict = {}
        for rel in sorted(r for r in files if STUB_REQ.match(r)):
            try:
                m = _STUB_AGENT.search(Path(files[rel]).read_text(errors="replace"))
            except OSError:
                m = None
            out.setdefault(m.group(1) if m else None, []).append(rel)
        return out
    ga, gb = by_agent(fa), by_agent(fb)
    pairs = {}
    for agent, rels in ga.items():
        for ra, rb in zip(rels, gb.get(agent) or []):
            pairs[ra] = rb
    return pairs


def run_diff(base: Path, other: Path, root_alias, rt_b: str) -> list:
    fa, fb = walk_capture(base), walk_capture(other)
    fa = {k: v for k, v in fa.items() if not k.endswith(SKIP)}
    fb = {k: v for k, v in fb.items() if not k.endswith(SKIP)}
    lines = []
    # N22: re-point each python stub request at the port's request of the SAME
    # agent and rank; the port's files so consumed are not compared twice.
    stub_pairs = pair_stub_requests(fa, fb)
    if stub_pairs and any(a != b for a, b in stub_pairs.items()):
        port = {k: v for k, v in fb.items() if STUB_REQ.match(k)}
        fb = {k: v for k, v in fb.items() if not STUB_REQ.match(k)}
        fb.update({ra: port[rb] for ra, rb in stub_pairs.items()})
        # a port request with no python partner keeps its own name — renamed if
        # that name is now taken by a pair, so it is never compared against a
        # request of another agent
        for rb in sorted(set(port) - set(stub_pairs.values())):
            fb[rb if rb not in fb else f"{rb}#unpaired"] = port[rb]
    for rel in sorted(set(fa) | set(fb)):
        fam = rel.split("/", 1)[0]
        if rel not in fa:
            lines.append({"file": rel, "where": "$file", "base": "<absent>",
                          "other": "<present>", "note": "present only in the port capture"})
            continue
        if rel not in fb:
            lines.append({"file": rel, "where": "$file", "base": "<present>",
                          "other": "<absent>", "note": "MISSING in the port capture"})
            continue
        lines += file_lines(rel, fa[rel], fb[rel], ports_of(base, fam), ports_of(other, fam),
                            root_alias, rt_b)
    return lines


def diff_dir(capture: Path) -> Path:
    """Where a diff over `capture` is written — ALWAYS inside the run tree.

    The rule is one line: the diff sits beside the captures it compares, which
    is `capture.parent/diff` for the `<run>/<runtime>` layout every acceptance
    run uses. It is NOT `capture.parent/diff` when `capture` IS a run directory
    (`--other e2e/runs/parity/20260922_sw`, a capture with the families directly
    under it): `parent` is then the runs ROOT, and the diff landed in
    `e2e/runs/parity/diff/` — outside every capture, overwritten by the next
    run of any shape, and easy to read as belonging to a run it has nothing to
    do with (measured 2026-09-21b: `e2e/runs/parity/diff/swift.jsonl`). The rule
    is path-only and reads nothing from disk, so it cannot change its mind
    because a directory was tidied away."""
    return (capture if capture.parent == RUNS else capture.parent) / "diff"


def cmd_self(a: Path, b: Path) -> int:
    lines = run_diff(a, b, None, "python")
    outdir = diff_dir(b)
    outdir.mkdir(parents=True, exist_ok=True)
    with (outdir / "python-self.jsonl").open("w") as fh:
        for ln in lines:
            fh.write(json.dumps(ln, ensure_ascii=False, default=str) + "\n")
    print(f"python self-diff: {len(lines)} lines "
          f"(every line is a HARNESS defect — a missing rule, or a nondeterministic step)")
    for ln in lines[:40]:
        print(f"  {ln['file']} {ln['where']}: {_short(ln.get('base'), 120)} "
              f"!= {_short(ln.get('other'), 120)}")
    return 1 if lines else 0


def cmd_port(base: Path, other: Path, rules, residuals, proofs, landed, root_alias) -> int:
    rt = json.loads((other / "summary.json").read_text())["runtime"] \
        if (other / "summary.json").exists() else other.name
    R = load_yaml(residuals) or {}
    P = load_yaml(proofs) or {}
    L = (load_yaml(landed) or {}).get(rt) or []
    he_base, he_other = harness_errors(base), harness_errors(other)
    if he_base or he_other:
        print(f"  HARNESS ERRORS — base: {sum(e['count'] for e in he_base)} in "
              f"{len(he_base)} files; {rt}: {sum(e['count'] for e in he_other)} in "
              f"{len(he_other)} files. A run with any is NOT clean.")
        for e in (he_base + he_other)[:10]:
            print(f"    {e['file']} ×{e['count']}")
    lines = run_diff(base, other, root_alias, rt)
    classified, used, stale = classify(lines, R.get("residuals") or [], L, P, rt)
    outdir = diff_dir(other)
    outdir.mkdir(parents=True, exist_ok=True)
    with (outdir / f"{rt}.jsonl").open("w") as fh:
        for ln in classified:
            fh.write(json.dumps(ln, ensure_ascii=False, default=str) + "\n")
    counts: dict = {}
    for ln in classified:
        counts[ln["verdict"]] = counts.get(ln["verdict"], 0) + 1
    summary = {"runtime": rt, "lines": len(classified), "verdicts": counts,
               "residuals_fired": used, "residuals_stale": stale,
               "rules": str(rules)}
    summary["harness_errors"] = {"base": he_base, rt: he_other}
    (outdir / f"{rt}.summary.json").write_text(json.dumps(summary, indent=1))
    print(f"{rt}: {len(classified)} diff lines  {counts}")
    print(f"  residuals fired: {used or 'none'}")
    print(f"  residuals STALE (matched nothing on {rt}): {stale or 'none'}")
    return 0 if not classified else 1


def cmd_check_proofs(proofs, plan: Path) -> int:
    P = load_yaml(proofs) or {}
    covered = set()
    for ids in (P.get("by_selector") or {}).values():
        covered |= set(ids)
    covered |= set((P.get("unit") or {}))
    covered |= set((P.get("capture_and_unit") or {}))
    covered |= set((P.get("no_proof") or {}))
    items = plan_items(plan)
    missing = sorted(items - covered)
    extra = sorted(covered - items) if items else []
    print(f"plan items: {len(items)}; covered by proofs.yaml: {len(covered & items)}; "
          f"UNPROVEN: {len(missing)}")
    if missing:
        print("  unproven: " + ", ".join(missing[:80]) + ("…" if len(missing) > 80 else ""))
    if extra:
        print(f"  in proofs.yaml but not in the plan ({len(extra)}): "
              + ", ".join(extra[:40]) + ("…" if len(extra) > 40 else ""))
    return 1 if missing else 0


def cmd_status(diffdir: Path, proofs, landed, plan: Path) -> int:
    P = load_yaml(proofs) or {}
    L = load_yaml(landed) or {}
    items = plan_items(plan)
    rows = []
    for f in sorted(diffdir.glob("*.summary.json")):
        s = json.loads(f.read_text())
        rt = s["runtime"]
        touched = set()
        for line in (diffdir / f"{rt}.jsonl").read_text().splitlines():
            if not line.strip():
                continue
            ln = json.loads(line)
            # an ACCEPTED difference does not stop its items passing — that is
            # what accepting it means (residuals.yaml v6)
            if ln.get("verdict") == "residual-accepted":
                continue
            touched |= set(ln.get("items") or [])
        landed_rt = set(L.get(rt) or [])
        passing = sorted(landed_rt - touched)
        rows.append({"runtime": rt, "total": len(items), "landed": len(landed_rt),
                     "pass": len(passing), "lines": s["lines"],
                     "verdicts": s["verdicts"], "stale": s["residuals_stale"],
                     "fired": s["residuals_fired"], "touched_items": sorted(touched)})
    md = ["# parity status", ""]
    line = " · ".join(f"{r['runtime']} {r['pass']}/{r['total']}" for r in rows)
    md += [f"**{line}**", "",
           "`pass` counts items that are in `landed.yaml` AND produce no diff line.",
           "`total` is every item the plan defines. An item that has not landed cannot",
           "pass, so the gap is the work, not a failure of the measurement.", ""]
    for r in rows:
        md += [f"## {r['runtime']}", "",
               f"- diff lines: {r['lines']} — {r['verdicts']}",
               f"- landed items: {r['landed']}, of which clean: {r['pass']}",
               f"- residuals fired: {', '.join(r['fired']) or 'none'}",
               f"- residuals STALE: {', '.join(r['stale']) or 'none'}",
               f"- items touched by a diff line: {len(r['touched_items'])}", ""]
    (diffdir / "status.md").write_text("\n".join(md))
    print("\n".join(md[:8]))
    print(f"\nwrote {diffdir / 'status.md'}")
    return 0


def cmd_rulecheck() -> int:
    """N17's OWN TEST: different chunking with the same bytes must be 0 lines,
    and different bytes must still be a line.

    Without the second case a "rule" that dropped every output frame would pass
    the first. The rest pin what the rule must NOT swallow: a run is per leg,
    per direction and per payload-field-set, a frame of another kind between two
    chunks ends the run, and an `output` payload with no `data` is not a chunk.
    Run it with `diff.py rulecheck` whenever N17 or `by_mark_and_sock` changes."""
    def ev(sock, data, **extra):
        pl = {"type": "output", "data": data, **extra}
        return {"sock": sock, "dir": "in", "kind": "text",
                "text": json.dumps({"type": "event", "payload": pl}),
                "json": {"type": "event", "payload": pl}}

    def rep(sock):
        return {"sock": sock, "dir": "in", "kind": "text",
                "json": {"type": "reply", "pid": 1}}

    cases = [
        ("same bytes, different chunking",
         [ev("ctrl", "echo hi\r\nhi\r\n$ ")],
         [ev("ctrl", "echo hi\r\n"), ev("ctrl", "hi\r\n"), ev("ctrl", "$ ")], False),
        ("different bytes",
         [ev("ctrl", "echo hi\r\nhi\r\n$ ")],
         [ev("ctrl", "echo hi\r\n"), ev("ctrl", "HI\r\n"), ev("ctrl", "$ ")], True),
        ("a reply between two chunks ends the run",
         [ev("ctrl", "a"), rep("ctrl"), ev("ctrl", "b")],
         [ev("ctrl", "ab"), rep("ctrl")], True),
        ("two legs are never joined together",
         [ev("ctrl", "a"), ev("proto", "b")],
         [ev("ctrl", "ab")], True),
        ("a different source agent starts a new run",
         [ev("ctrl", "a", source="t1"), ev("ctrl", "b", source="t2")],
         [ev("ctrl", "ab", source="t1")], True),
        ("an output payload with no `data` is not a chunk",
         [ev("ctrl", "a"), {"sock": "ctrl", "dir": "in", "kind": "text",
                            "json": {"type": "event", "payload": {"type": "output"}}}],
         [ev("ctrl", "a")], True),
    ]
    bad = 0
    for name, left, right, want_lines in cases:
        lines = compare_json("t/frames.jsonl", frames_view(left), frames_view(right),
                             Norm({}, None, "python"), Norm({}, None, "rust"))
        ok = bool(lines) == want_lines
        bad += 0 if ok else 1
        print(f"  {'ok  ' if ok else 'FAIL'} N17 — {name}: {len(lines)} line(s), "
              f"expected {'>0' if want_lines else '0'}")
    total = len(cases)

    # ── N18 — the loader's own persistence rows. ────────────────────────────
    # Both ways, every time: the write COUNT must be forgiven and the ROWS must
    # not be. Without the second half a "rule" that dropped the whole file would
    # pass the first.
    import tempfile

    def persist(n, path="ledger.json"):
        """n own-persistence rows: no origin, addressed to the store."""
        return [{"id": 100 + i, "target": "store", "origin": None, "named": False,
                 "name": None, "retries": 0, "attempts": 1,
                 "outcome": {"pid": 100 + i, "status": "completed", "attempts": 1,
                             "result": {"path": path, "written": 10, "offset": 0,
                                        "size": 10}}}
                for i in range(n)]

    def work(i, origin="rest", target="ai", status="completed"):
        return {"id": i, "target": target, "origin": origin, "named": False,
                "name": None, "retries": 0, "attempts": 1,
                "outcome": {"pid": i, "status": status, "attempts": 1, "result": {"ok": True}}}

    W = [work(1), work(2), work(3, target="runner")]

    def ledger_lines(rows_a, rows_b, rel="t/store_files_final/disk_raw/ledger.json"):
        with tempfile.TemporaryDirectory() as d:
            pa, pb = Path(d) / "a.json", Path(d) / "b.json"
            pa.write_text(json.dumps({"v": 2, "rows": rows_a}))
            pb.write_text(json.dumps({"v": 2, "rows": rows_b}))
            return file_lines(rel, pa, pb, {}, {}, None, "rust")

    n18 = [
        # The measurement itself: python flushed 379 times where rust flushed 132,
        # over the same work.
        ("a different number of own-persistence rows is not a difference",
         W + persist(40), persist(3) + W, False),
        ("a MISSING non-persistence row is still a difference",
         W + persist(40), persist(3) + W[:2], True),
        ("an EXTRA non-persistence row is still a difference",
         W + persist(40), persist(3) + W + [work(4)], True),
        ("a DIFFERING OUTCOME is still a difference",
         W + persist(40), persist(3) + W[:2] + [work(3, target="runner", status="errored")],
         True),
        ("a row addressed to the store WITH an origin is never dropped",
         W + persist(40), persist(3) + W + [work(5, target="store")], True),
        # The TAIL clause, both ways. It forgives the tail IN THE SHAPE
        # REDUCTION — where one trailing row of a shape the other side has not
        # flushed yet cascades — and NOT in the census, because an on-disk file
        # cannot tell an un-flushed tail from a row that never happened. So the
        # tail costs exactly one census line instead of a shape cascade.
        ("an un-flushed TAIL costs a census line and no shape cascade",
         W, W + [work(4)], "census-only"),
        ("a surplus row in the MIDDLE is not a tail",
         [work(1), work(3, target="runner")], W, True),
    ]
    for name, la, lb, want in n18:
        lines = ledger_lines(la, lb)
        if want == "census-only":
            ok = bool(lines) and all(ln["where"].startswith("$.__row_census__")
                                     for ln in lines)
        else:
            ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} N18 — {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # …and on `messages.xml`, where the loader's traffic is TASK rows.
    def msg(rows):
        out = ['<messages v="1">']
        for r in rows:
            at = " ".join(f'{k}="{v}"' for k, v in r.items() if k != "st")
            st = r.get("st", "completed")
            out.append(f"<row {at} />" if st is None
                       else f"<row {at}><outcome status=\"{st}\" /></row>")
        return "\n".join(out) + "\n</messages>\n"

    MW = [{"id": "7", "kind": "call", "verb": "boot", "agent": "web"},
          {"id": "8", "kind": "call", "verb": "get_routes", "parent": "7",
           "origin": "web", "agent": "rest"}]

    def flush(n, first=20):
        return [{"id": str(first + i), "kind": "task", "verb": "ledger_flush",
                 "agent": "kernel_state"} for i in range(n)]

    def msg_lines(ra, rb):
        with tempfile.TemporaryDirectory() as d:
            pa, pb = Path(d) / "messages.xml", Path(d) / "b_messages.xml"
            pa.write_text(msg(ra))
            pb.write_text(msg(rb))
            return file_lines("t/disk_raw/messages.xml", pa, pb, {}, {}, None, "rust")

    n18x = [
        ("messages.xml: 559 flush task rows against 26 is not a difference",
         MW + flush(40), flush(3) + MW, False),
        ("messages.xml: a missing work row is still a difference",
         MW + flush(40), flush(3) + MW[:1], True),
        ("messages.xml: a RE-PARENTED row is still a difference",
         MW + flush(40),
         flush(3) + [MW[0], {**MW[1], "parent": "20"}], True),
        ("messages.xml: a settle belonging to a dropped flush goes with it",
         MW + flush(40) + [{"id": "99", "kind": "settle", "parent": "20",
                            "agent": "kernel_state"}],
         flush(3) + MW, False),
        # The status is a SET, so an unsettled row is not a second bucket…
        # A row with no `<outcome>` yet is a different TAG SHAPE, which is a
        # real difference in this synthetic fixture and stays a line; what must
        # NOT happen is the census splitting the row's bucket in two.
        ("messages.xml: a row not settled YET is not a differing outcome",
         MW + flush(40), flush(3) + [MW[0], {**MW[1], "st": None}], "no-census"),
        # …and a row that really ERRORED still is.
        ("messages.xml: a row that ERRORED is still a difference",
         MW + flush(40), flush(3) + [MW[0], {**MW[1], "st": "errored"}], True),
    ]
    for name, ra, rb, want in n18x:
        lines = msg_lines(ra, rb)
        if want == "no-census":
            ok = not any(ln["where"].startswith("$.__row_census__") for ln in lines)
        else:
            ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} N18 — {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # ── N19 — row-id order within a turn. ──────────────────────────────────
    # A PERMUTATION of the ids among rows minted in the same window must be 0
    # lines; a dropped row, a re-parented row and a changed body must not be.
    def R(i, parent=None, verb="send", agent="ai", kind="call", extra=None):
        r = {"id": i, "kind": kind, "verb": verb, "agent": agent,
             "outcome": {"status": "completed"}}
        if parent is not None:
            r["parent"] = parent
        if extra:
            r.update(extra)
        return r

    # The measured shape: a handler row (41) spawns two rows in one turn; python
    # mints the task before the call, rust the other way round, and everything
    # after re-converges.
    base = {"rows": [R(41, verb="boot"), R(42, 41, verb="think"),
                     R(43, 41, verb="reply", kind="task"), R(44, 41, verb="settle")]}
    perm = {"rows": [R(41, verb="boot"), R(43, 41, verb="think"),
                     R(42, 41, verb="reply", kind="task"), R(44, 41, verb="settle")]}
    dropped_row = {"rows": [R(41, verb="boot"), R(42, 41, verb="think"),
                            R(44, 41, verb="settle")]}
    reparented = {"rows": [R(41, verb="boot"), R(42, 41, verb="think"),
                           R(43, 42, verb="reply", kind="task"), R(44, 41, verb="settle")]}
    changed = {"rows": [R(41, verb="boot"), R(42, 41, verb="THINK"),
                        R(43, 41, verb="reply", kind="task"), R(44, 41, verb="settle")]}
    n19 = [
        ("a permutation of ids minted in one turn is not a difference", base, perm, False),
        ("a DROPPED row is still a difference", base, dropped_row, True),
        ("a RE-PARENTED row is still a difference", base, reparented, True),
        ("a changed verb is still a difference", base, changed, True),
    ]
    for name, la, lb, want in n19:
        lines = compare_json("t/rows.json", la, lb,
                             Norm({}, None, "python"), Norm({}, None, "rust"))
        ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} N19 — {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # ── N12 — `__tag_shapes__` is a SET KEYED BY THE SHAPE. ────────────────
    # The same defect `__distinct_shapes__` had and the same fix: `flatten`
    # indexes a LIST positionally, so one tag shape present on one side only
    # shifted every index after it. One one-sided shape must be ONE line, and it
    # must NAME the shape; equal shape sets must be none.
    TS = [{"id": "7", "kind": "call", "verb": "boot", "agent": "web"}]
    #: A row with an attribute the other side never writes: a NEW tag shape, and
    #: one that sorts BEFORE the plain one, so a positional compare cascades.
    TSX = [{"id": "8", "agent": "ws", "kind": "call", "verb": "boot"}]
    n12 = [
        ("tag shapes: the same shapes in a different order are not a difference",
         TS + flush(4), flush(2) + TS, 0),
        ("tag shapes: ONE one-sided shape is ONE line, and it names the shape",
         TS + flush(4), flush(2) + TS + TSX, 1),
    ]
    for name, ra, rb, want_ts in n12:
        lines = msg_lines(ra, rb)
        # The NAMED lines: `$.__tag_shapes__.<the shape itself>`. The key-set
        # line beside them (`$.__tag_shapes__ · keys`) is the set difference
        # said once, and is expected whenever there is one.
        ts = [ln for ln in lines if ln["where"].startswith("$.__tag_shapes__.")]
        keyline = [ln for ln in lines if ln["where"] == "$.__tag_shapes__ · keys"]
        ok = len(ts) == want_ts and len(keyline) == (1 if want_ts else 0) \
            and all("<row" in ln["where"] for ln in ts)
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} N12 — {name}: {len(ts)} named "
              f"tag-shape line(s) + {len(keyline)} key line(s), expected {want_ts} + "
              f"{1 if want_ts else 0}")

    # ── N10 — the request CENSUS, and the recall header's two estimates. ────
    # The counts go ONLY where the text they count is compared whole beside
    # them; everything else in the same entry stays exact, and a ZERO stays a
    # zero so "no system block at all" cannot hide in "a shorter one".
    def census(bytes_, sysc, messages=3, keys=("model", "messages")):
        return [{"n": 1, "agent": "a", "kind": "turn", "phase": "live", "name": "t",
                 "bytes": bytes_, "system_chars": sysc, "messages": messages,
                 "has_context_notice": False, "keys": list(keys)}]

    def cj(a, b, rel="t/ctx/requests_index.json"):
        return compare_json(rel, a, b, Norm({}, None, "python"), Norm({}, None, "rust"))

    RECALL = ("You are rebuilding your own working context.\n"
              "DIGEST resolution; your budget for the body is {b} tokens and the "
              "digests use\n{d}. Replies ALREADY IN THIS HISTORY:\n"
              "1118 · user · \"reread d\" · 3654B · ↑1097\n")
    n10 = [
        ("census: `bytes`/`system_chars` over a body recorded beside them",
         census(23815, 23214), census(23813, 23212), False),
        ("census: a different MESSAGE COUNT is still a difference",
         census(23815, 23214), census(23813, 23212, messages=4), True),
        ("census: NO system block at all is still a difference",
         census(23815, 23214), census(23813, 0), True),
        ("census: a different `keys` set is still a difference",
         census(23815, 23214), census(23813, 23212, keys=("model",)), True),
        ("recall header: the digest estimate and the budget",
         {"content": RECALL.format(b=3869, d=2141)},
         {"content": RECALL.format(b=3870, d=2137)}, False),
        ("recall header: a CHANGED DIGEST LINE is still a difference",
         {"content": RECALL.format(b=3869, d=2141)},
         {"content": RECALL.format(b=3870, d=2137).replace("reread d", "reread z")}, True),
    ]
    for name, la, lb, want in n10:
        lines = cj(la, lb) if isinstance(la, list) else cj(la, lb, "t/ctx/req/recall.json")
        ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} N10 — {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # ── The 2026-09-30 python self-diff: counter ids a run consumed. ────────
    # The loader's debounced persistence takes a different number of ids on
    # every run, so every id after it shifts and some cross a digit width. Each
    # rule both ways: the shift is forgiven, a changed RELATIONSHIP or a changed
    # word beside it is not.
    def outcome(bytes_, status="completed"):
        return {"executions": [{"id": 342, "target": "kernel_state", "status": status,
                                "outcome": {"pid": 342, "status": status,
                                            "bytes": bytes_, "attempts": 1}}]}

    def row(rid, parent, reply, tail, bytes_, word="write"):
        return {"rows": [{"id": rid, "parent": parent, "kind": "user", "role": "user",
                          "head": f"[reply to {reply} · totals {word} · ↑{parent}] "
                                  f"<version>73475cb40a568e8d{tail}",
                          "bytes": bytes_}]}

    SAME = "[identical to the reply in row {r} — the same {n} bytes, not repeated]"
    ids = [
        ("N4 — an event's `msg` shifted by the run",
         [{"msg": 1177}, {"msg": 1243}], [{"msg": 1180}, {"msg": 1246}], False,
         "t/ai_paths/events.json"),
        ("N4 — a `msg` naming a DIFFERENT message is still a difference",
         [{"msg": 10}, {"msg": 10}], [{"msg": 10}, {"msg": 11}], True,
         "t/ai_paths/events.json"),
        ("N4 — `reply in row <n>` shifted by the run",
         {"content": SAME.format(r=718, n=6205)}, {"content": SAME.format(r=721, n=6205)},
         False, "t/ai_paths/cases/x.json"),
        ("N4 — the byte count beside it is still compared",
         {"content": SAME.format(r=718, n=6205)}, {"content": SAME.format(r=721, n=6206)},
         True, "t/ai_paths/cases/x.json"),
        ("N4 — a `msg` quoted in an origin line",
         {"content": 'origin: readings reflect {\\"to\\": [\\"readings\\"], \\"msg\\": 1414}'},
         {"content": 'origin: readings reflect {\\"to\\": [\\"readings\\"], \\"msg\\": 1408}'},
         False, "t/store/x.json"),
        ("N4 — the recipients beside it are still compared",
         {"content": 'origin: readings reflect {\\"to\\": [\\"readings\\"], \\"msg\\": 1414}'},
         {"content": 'origin: readings reflect {\\"to\\": [\\"totals\\"], \\"msg\\": 1408}'},
         True, "t/store/x.json"),
        ("N4 — the reservation mark `next_id=`",
         {"content": "guid=x  next_id=2001  kernel_verbs"},
         {"content": "guid=x  next_id=1001  kernel_verbs"}, False, "t/store/x.json"),
        ("N4 — a prompt WITHOUT the mark is still a difference",
         {"content": "guid=x  next_id=2001  kernel_verbs"},
         {"content": "guid=x  kernel_verbs"}, True, "t/store/x.json"),
        ("N10 — a ledger outcome's reply size",
         outcome(450), outcome(452), False, "t/tree/kernel_ledger_after.json"),
        ("N10 — its status is still compared",
         outcome(450), outcome(452, status="failed"), True, "t/tree/kernel_ledger_after.json"),
        ("N10 — an outcome with NO body is still a difference",
         outcome(450), outcome(0), True, "t/tree/kernel_ledger_after.json"),
        ("N10 — a row head cut at the same raw width",
         row(1010, 997, 1001, "</v", 127), row(1004, 991, 995, "</ve", 126), False,
         "t/store/s4/history_final.json"),
        ("N10 — a head whose shared prefix differs is still a difference",
         row(1010, 997, 1001, "</v", 127), row(1004, 991, 995, "</ve", 126, word="read"),
         True, "t/store/s4/history_final.json"),
        ("N10 — heads of different raw widths are still a difference",
         row(1010, 997, 1001, "</v", 127), row(1004, 991, 995, "</ver", 126), True,
         "t/store/s4/history_final.json"),
        ("N10 — a row whose head names no id keeps its count",
         {"rows": [{"id": "a", "head": "plain", "bytes": 10}]},
         {"rows": [{"id": "a", "head": "plain", "bytes": 11}]}, True,
         "t/store/s4/history_final.json"),
    ]
    for name, la, lb, want, rel in ids:
        lines = cj(la, lb, rel)
        ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # ── N3's qualified-caller form: the minted guid goes, the agent stays. ───
    g1, g2 = "25e352990c374626905a70c52164dc46", "229a2a97b2bb4ee1a9726ff8cbfbbcff"
    for name, la, lb, want in (
            ("a caller's minted guid is not compared",
             [{"caller": f"{g1}|rest"}], [{"caller": f"{g2}|rest"}], False),
            ("a caller's AGENT half is still compared",
             [{"caller": f"{g1}|rest"}], [{"caller": f"{g2}|ws"}], True),
    ):
        lines = cj(la, lb, "t/bridges/ws_peer_partner_events.json")
        ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} N3 — {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # ── 2026-10-01 — the parity-leftover rules (owner: tmp/todo.md (a)/(c)). ──
    # Each both ways: the measured difference is forgiven, the real one beside it
    # is still a line.
    def cjr(a, b, rel, rt="rust"):
        return compare_json(rel, a, b, Norm({}, None, "python"), Norm({}, None, rt))

    sched = "agents/sched/schedules.yaml"

    def srec(n, verb="read_stream"):
        return {"path": sched, "size": n, "chunk_replies": [{"reply": {
            "origin": f"store {sched} bytes 0-{n} of {n} — {verb} {{path: {sched}, "
                      f"offset: 0, length: {n}}}"}}]}

    def ev(t, **kw):
        return {"type": t, **kw}

    PEER = ev("send", text="CASE=cycle_end#1")
    REPLY = ev("send", text="[reply to 9 · ai2 send · ↑5] ok", client_id="probe")
    QUEUED = ev("status", phase="queued", client_id="probe")
    THINK = ev("status", phase="thinking", client_id="probe")
    SELF = ev("reflect", tree="none")

    def fr(sock, kind, ftype, **extra):
        j = {"type": ftype, **extra}
        return {"sock": sock, "dir": "in" if kind == "in" else kind,
                "kind": "text", "json": j}

    R1, E1, E2 = (fr("ctrl", "in", "reply", id="w-1", data={}),
                  fr("ctrl", "in", "event", src="ai", payload={"type": "reflect"}),
                  fr("ctrl", "in", "event", src="ai", payload={"type": "status"}))
    OUT = fr("ctrl", "out", "call", id="w-2")
    new_cases = [
        ("N2 — the runtime field as JSON inside a stdout",
         lambda: cjr({"stdout": '{\n  "runtime": "python",\n  "env": "host"\n}'},
                     {"stdout": '{\n  "runtime": "rust",\n  "env": "host"\n}'}, "t/cli/x.json"), False),
        ("N2 — a different field beside it is still a line",
         lambda: cjr({"stdout": '{\n  "runtime": "python",\n  "env": "host"\n}'},
                     {"stdout": '{\n  "runtime": "rust",\n  "env": "container"\n}'}, "t/cli/x.json"), True),
        ("N2 — a head-only stdout's count off by EXACTLY the name length",
         lambda: cjr({"stdout": "{ head", "stdout_bytes": 3120},
                     {"stdout": "{ head", "stdout_bytes": 3118}, "t/cli/y.json"), False),
        ("N2 — off by anything else is still a line",
         lambda: cjr({"stdout": "{ head", "stdout_bytes": 3120},
                     {"stdout": "{ head", "stdout_bytes": 3117}, "t/cli/y.json"), True),
        ("N4 — the escaped ↑ arrow in a frame's text",
         lambda: cjr({"text": "[reply to 7 \\u00b7 files reflect \\u00b7 \\u2191118]"},
                     {"text": "[reply to 7 \\u00b7 files reflect \\u00b7 \\u2191115]"}, "t/wire/f.json"), False),
        ("N4 — a different verb beside it is still a line",
         lambda: cjr({"text": "[reply to 7 \\u00b7 files reflect \\u00b7 \\u2191118]"},
                     {"text": "[reply to 7 \\u00b7 files read \\u00b7 \\u2191115]"}, "t/wire/f.json"), True),
        ("N4 — an id whose digits also sit in the arrow's own code (\\u2191 then 91)",
         lambda: cjr({"text": "[reply to 7 \\u00b7 files reflect \\u00b7 \\u219192]"},
                     {"text": "[reply to 7 \\u00b7 files reflect \\u00b7 \\u219191]"}, "t/wire/f.json"), False),
        ("N7 — a stream id whose agent carries a digit",
         lambda: cjr({"stream_id": "ai2:3d113b94"}, {"stream_id": "ai2:ca4a928a"},
                     "t/ai_paths/e.json"), False),
        ("N7 — a bridge forward id: the boot nonce differs, the count does not",
         lambda: cjr({"frame": '{"id": "wsb-3c9d11402c46-3"}'}, {"frame": '{"id": "wsb-0a1b2c3d4e5f-4"}'},
                     "t/bridges/p.json"), True),
        ("N4 — a part_start's `re` anchor is a minted id",
         lambda: cjr({"type": "part_start", "re": 187}, {"type": "part_start", "re": 186},
                     "t/wire/f.json"), False),
        ("N10 — a body count over text naming the runtime",
         lambda: cjr({"body": {"text": '{"runtime":"python","a":1}', "len": 26}},
                     {"body": {"text": '{"runtime":"rust","a":1}', "len": 24}}, "t/wire/r.json"), False),
        ("N10 — a body that differs beside it is still a line",
         lambda: cjr({"body": {"text": '{"runtime":"python","a":1}', "len": 26}},
                     {"body": {"text": '{"runtime":"rust","a":2}', "len": 24}}, "t/wire/r.json"), True),
        ("N10 — a budget FIELD within one token",
         lambda: cjr({"budget": 3158, "why": "w"}, {"budget": 3159, "why": "w"}, "t/ctx/b.json"), False),
        ("N10 — a budget FIELD two tokens off is still a line",
         lambda: cjr({"budget": 3158, "why": "w"}, {"budget": 3160, "why": "w"}, "t/ctx/b.json"), True),
        ("N10 — a <budget> in text within one token",
         lambda: cjr({"c": "<dropped>29</dropped>\n<budget>3158</budget>"},
                     {"c": "<dropped>29</dropped>\n<budget>3159</budget>"}, "t/ctx/c.json"), False),
        ("N10 — a <budget> in text with another count changed is still a line",
         lambda: cjr({"c": "<dropped>29</dropped>\n<budget>3158</budget>"},
                     {"c": "<dropped>28</dropped>\n<budget>3159</budget>"}, "t/ctx/c.json"), True),
        ("N12 — a rewritten file's read origin repeats its moving size",
         lambda: cjr(srec(193), srec(195), "t/store/s.json"), False),
        ("N12 — a different verb in the origin is still a line",
         lambda: cjr(srec(193), srec(195, verb="read"), "t/store/s.json"), True),
        ("N20 — a reply and an event swapped on one socket",
         lambda: compare_json("t/frames.jsonl", frames_view([E1, R1, E2]),
                              frames_view([R1, E1, E2]), Norm({}, None, "python"),
                              Norm({}, None, "rust")), False),
        ("N20 — a MISSING reply is still a line",
         lambda: compare_json("t/frames.jsonl", frames_view([E1, R1, E2]),
                              frames_view([E1, E2]), Norm({}, None, "python"),
                              Norm({}, None, "rust")), True),
        ("N20 — events among themselves are still ordered",
         lambda: compare_json("t/frames.jsonl", frames_view([E1, E2, R1]),
                              frames_view([E2, E1, R1]), Norm({}, None, "python"),
                              Norm({}, None, "rust")), True),
        ("N20 — a reply never crosses an outgoing call",
         lambda: compare_json("t/frames.jsonl", frames_view([R1, OUT, E1]),
                              frames_view([E1, OUT, R1]), Norm({}, None, "python"),
                              Norm({}, None, "rust")), True),
        ("N21 — the peer turn's self-reflect after the reply's delivery",
         lambda: cjr({"ai": canon_self_reflect([PEER, SELF, REPLY, QUEUED, THINK])},
                     {"ai": canon_self_reflect([PEER, REPLY, QUEUED, SELF, THINK])},
                     "ai_paths/events.json", "swift"), False),
        ("N21 — the two TURNS in the other order is still a line",
         lambda: cjr({"ai": canon_self_reflect([PEER, SELF, REPLY, QUEUED, THINK])},
                     {"ai": canon_self_reflect([PEER, REPLY, THINK, SELF])},
                     "ai_paths/events.json"), True),
        ("N4 — a documentation example's number is a literal, not an id",
         lambda: cjr({"text": "[from solver_1 · to solver_2, solver_3 · msg 412]\n"
                              "[reply to 5 · docs read · ↑412]"},
                     {"text": "[from solver_1 · to solver_2, solver_3 · msg 412]\n"
                              "[reply to 5 · docs read · ↑409]"},
                     "t/ctx/req/x.json"), False),
        ("N4 — a real header's msg id is still renamed like any id",
         lambda: cjr({"text": "[from ai · to ai2 · msg 412]"},
                     {"text": "[from ai · to ai2 · msg 409]"},
                     "t/ctx/req/x.json"), False),
        ("N4 — the documentation example with another number is still a line",
         lambda: cjr({"text": "[from solver_1 · to solver_2, solver_3 · msg 412]"},
                     {"text": "[from solver_1 · to solver_2, solver_3 · msg 413]"},
                     "t/ctx/req/x.json"), True),
        ("N4 — a flush's `covers` inside embedded json",
         lambda: cjr({"summary": '{"id": 723, "kind": "flush", "covers": [717, 718]}'},
                     {"summary": '{"id": 726, "kind": "flush", "covers": [720, 721]}'},
                     "t/wire/frames.jsonl"), False),
        ("N4 — a `covers` of another LENGTH is still a line",
         lambda: cjr({"summary": '{"id": 723, "kind": "flush", "covers": [717, 718]}'},
                     {"summary": '{"id": 726, "kind": "flush", "covers": [720, 721, 722]}'},
                     "t/wire/frames.jsonl"), True),
        ("N26 — the ports' foundation_models_backend catalog entry in a prompt",
         lambda: cjr({"content": "x  bundles=cli,file_bridge,kernel_state  y"},
                     {"content": "x  bundles=cli,file_bridge,foundation_models_backend,kernel_state  y"},
                     "t/ctx/r.json"), False),
        ("N26 — any other catalog difference beside it is still a line",
         lambda: cjr({"content": "x  bundles=cli,file_bridge,kernel_state  y"},
                     {"content": "x  bundles=cli,foundation_models_backend,kernel_state,web  y"},
                     "t/ctx/r.json"), True),
        ("N23 — a KV conversation id in a reflect",
         lambda: cjr({"kv": {"live": 1, "conversations": ["022609017e2a0f28"]}},
                     {"kv": {"live": 1, "conversations": ["715ef89e7813c656"]}},
                     "t/tree/reflect_ai.json"), False),
        ("N23 — a different NUMBER of conversations is still a line",
         lambda: cjr({"kv": {"live": 1, "conversations": ["022609017e2a0f28"]}},
                     {"kv": {"live": 2, "conversations": ["715ef89e7813c656",
                                                          "0123456789abcdef"]}},
                     "t/tree/reflect_ai.json"), True),
        ("N23 — a conversations entry that is not a 16-hex id is still compared",
         lambda: cjr({"kv": {"conversations": ["abc"]}}, {"kv": {"conversations": ["abd"]}},
                     "t/tree/reflect_ai.json"), True),
        ("N23 — the ids rendered into a prompt",
         lambda: cjr({"text": "<live>1</live> <conversations> <item>022609017e2a0f28</item> "
                              "</conversations> <evicted>0</evicted>"},
                     {"text": "<live>1</live> <conversations> <item>715ef89e7813c656</item> "
                              "</conversations> <evicted>0</evicted>"},
                     "t/ai_paths/x.json"), False),
        ("N23 — the table's other fields beside them are still compared",
         lambda: cjr({"text": "<conversations> <item>022609017e2a0f28</item> "
                              "</conversations> <evicted>0</evicted>"},
                     {"text": "<conversations> <item>715ef89e7813c656</item> "
                              "</conversations> <evicted>1</evicted>"},
                     "t/ai_paths/x.json"), True),
    ]
    for name, fn, want in new_cases:
        lines = fn()
        ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # N22 — the stub's requests re-paired per agent, on real files.
    with tempfile.TemporaryDirectory() as td:
        tdp = Path(td)

        def req(side, n, agent):
            f = tdp / side / f"req_{n:03d}.bin"
            f.parent.mkdir(parents=True, exist_ok=True)
            f.write_text(f'{{"system": "x  id={agent}  model=m"}}')
            return f"ai_paths/stub/req_{n:03d}.bin", str(f)

        fa = dict([req("a", 1, "ai"), req("a", 2, "ai2"), req("a", 3, "ai")])
        fb = dict([req("b", 1, "ai"), req("b", 2, "ai"), req("b", 3, "ai2")])
        pairs = pair_stub_requests(fa, fb)
        for name, ok in (
                ("N22 — each agent's k-th request pairs with the other side's k-th",
                 pairs == {"ai_paths/stub/req_001.bin": "ai_paths/stub/req_001.bin",
                           "ai_paths/stub/req_003.bin": "ai_paths/stub/req_002.bin",
                           "ai_paths/stub/req_002.bin": "ai_paths/stub/req_003.bin"}),
                ("N22 — a request with no partner of its agent stays unpaired",
                 "ai_paths/stub/req_003.bin" not in pair_stub_requests(
                     fa, dict([req("c", 1, "ai"), req("c", 2, "ai2")]))),
        ):
            bad += 0 if ok else 1
            total += 1
            print(f"  {'ok  ' if ok else 'FAIL'} {name}")

    # ── N24 / N25 (2026-10-02) — each both ways, through `file_lines`, where
    # the two passes run. ─────────────────────────────────────────────────────
    def flines(a, b, rel, rt="rust"):
        with tempfile.TemporaryDirectory() as d:
            suffix = Path(rel).suffix or ".txt"
            pa, pb = Path(d) / f"a{suffix}", Path(d) / f"b{suffix}"
            for p, doc in ((pa, a), (pb, b)):
                p.write_text(doc if isinstance(doc, str) else json.dumps(doc))
            return file_lines(rel, pa, pb, {}, {}, None, rt)

    def mem(rows, *cens):
        doc = {"id": "memory", "owners": 1, "rows": rows, "grants": 0}
        return {**doc, "harness": {"flush_rows": list(cens)}} if cens else doc

    def cen(rows, flush):
        return {"rows": rows, "flush": flush}

    MEM = "store/s1/readme_true/memory.json"
    PY_MEM = mem(334, cen(334, 34), cen(334, 34))

    def pwalk(names, limit, objs=False, size=None):
        """A `families.page_walk` record over a `bundles` reflect, built the way
        the family builds it; `size` != `limit` is a runtime paging bug."""
        base = {"type": "reflect", "tree": "none", "bundles": "all" if objs else "ids",
                "limit": limit}
        ents = [{"name": n, "handler_module": f"{n}.tools"} if objs else n for n in names]
        size = size or limit
        pages, payload = [], dict(base)
        for i in range(0, len(ents), size):
            chunk = ents[i:i + size]
            reply = {"id": "kernel_state", "sentence": "s", "bundles": chunk}
            cur = None
            if i + size < len(ents):
                cur = reply["bundles_next"] = _bundle_name(chunk[-1])
            pages.append({"payload": dict(payload), "reply": reply,
                          "cursor_key": "bundles_next" if cur else None, "cursor": cur})
            payload = {**base, "after": cur}
        return {"base_payload": base, "stop": "no_cursor", "page_count": len(pages),
                "pages": pages}

    CAT = ["cli", "exec_bridge", "file_bridge", "kernel_state", "ollama_backend", "web"]
    CAT_FM = CAT[:3] + [FM_BUNDLE] + CAT[3:]
    WALK = "store/s2/root/bundles_ids_limit2_pages.json"

    def stdout(names):
        return "{\n  \"runtime\": \"x\",\n  \"bundles\": [\n" + ",\n".join(
            f"    \"{n}\"" for n in names) + "\n  ]\n}\n"

    def edit_page(walk, i, **kw):
        walk["pages"][i]["reply"].update(kw)
        return walk

    n2425 = [
        ("N24 — flush rows the runtime's speed made are not a difference",
         lambda: flines(PY_MEM, mem(333, cen(333, 33), cen(333, 33)), MEM), False),
        ("N24 — a different count of NON-flush rows is still a line",
         lambda: flines(PY_MEM, mem(334, cen(334, 33), cen(334, 33)), MEM), True),
        ("N24 — a flush between the first census and the reflect: the second one counts",
         lambda: flines(PY_MEM, mem(339, cen(333, 33), cen(339, 39)), MEM), False),
        ("N24 — a reflect that matches NO census is a line, not a guess",
         lambda: flines(PY_MEM, mem(336, cen(333, 33), cen(339, 34)), MEM), True),
        ("N24 — two matching censuses that disagree on flushes is a line",
         lambda: flines(PY_MEM, mem(333, cen(333, 33), cen(333, 32)), MEM), True),
        ("N24 — a port that took no census is a line",
         lambda: flines(PY_MEM, mem(333), MEM), True),
        ("N24 — another field of the reflect beside it is still compared",
         lambda: flines(PY_MEM, {**mem(333, cen(333, 33)), "grants": 1}, MEM), True),
        ("N25 — the port's catalog list without the bundle python does not ship",
         lambda: cjr({"bundles": CAT}, {"bundles": CAT_FM}, "t/store/x.json"), False),
        ("N25 — the same as `bundles=all` objects",
         lambda: cjr(pwalk(CAT, 99, objs=True)["pages"][0]["reply"],
                     pwalk(CAT_FM, 99, objs=True)["pages"][0]["reply"], "t/store/x.json"), False),
        ("N25 — any OTHER extra bundle on the port is still a line",
         lambda: cjr({"bundles": CAT}, {"bundles": CAT + ["extra_backend"]},
                     "t/store/x.json"), True),
        ("N25 — PYTHON carrying it is still a line (the port side only)",
         lambda: cjr({"bundles": CAT_FM}, {"bundles": CAT_FM}, "t/store/x.json"), True),
        ("N25 — the comma-joined catalog in a system prompt",
         lambda: cjr({"text": "not to find one.  bundles=" + ",".join(CAT) + "\\n\\nYou are"},
                     {"text": "not to find one.  bundles=" + ",".join(CAT_FM) + "\\n\\nYou are"},
                     "t/ctx/req/x.json"), False),
        ("N25 — a prompt catalog missing a bundle is still a line",
         lambda: cjr({"text": "bundles=" + ",".join(CAT)},
                     {"text": "bundles=" + ",".join(CAT_FM[:-1])}, "t/ctx/req/x.json"), True),
        ("N25 — an id list in a REST body, compact, first / middle / last",
         lambda: cjr({"t": [json.dumps({"bundles": n}, separators=(",", ":"))
                            for n in (CAT, CAT, CAT)]},
                     {"t": [json.dumps({"bundles": n}, separators=(",", ":"))
                            for n in ([FM_BUNDLE] + CAT, CAT_FM, CAT + [FM_BUNDLE])]},
                     "t/wire/rest.json"), False),
        ("N25 — an indented id list in a cli stdout, inside a json record",
         lambda: cjr({"stdout": stdout(CAT)}, {"stdout": stdout(CAT_FM)}, "t/cli/x.json"),
         False),
        ("N25 — the same stdout as a raw text file (the line goes, not the text of it)",
         lambda: flines(stdout(CAT), stdout(CAT_FM), "cli/cli/x.stdout"), False),
        ("N25 — a stdout list missing ANOTHER entry is still a line",
         lambda: flines(stdout(CAT), stdout(CAT_FM[:-1]), "cli/cli/x.stdout"), True),
        ("N25 — a paged walk re-paged without it (ids)",
         lambda: flines(pwalk(CAT, 2), pwalk(CAT_FM, 2), WALK), False),
        ("N25 — a paged walk re-paged without it (objects)",
         lambda: flines(pwalk(CAT, 2, objs=True), pwalk(CAT_FM, 2, objs=True), WALK), False),
        ("N25 — re-paged when it sits FIRST, and alone on the port's last page",
         lambda: flines(pwalk(CAT, 2), pwalk([FM_BUNDLE] + CAT, 2), WALK)
         + flines(pwalk(CAT, 2), pwalk(CAT + [FM_BUNDLE], 2), WALK), False),
        ("N25 — a port that pages by 3 under limit=2 is left as recorded",
         lambda: flines(pwalk(CAT, 2), pwalk(CAT_FM, 2, size=3), WALK), True),
        ("N25 — another field on a re-paged page is still a line",
         lambda: flines(pwalk(CAT, 2), edit_page(pwalk(CAT_FM, 2), 1, sentence="t"), WALK),
         True),
        ("N25 — a port walk missing another bundle is still a line",
         lambda: flines(pwalk(CAT, 2), pwalk(CAT_FM[:-1], 2), WALK), True),
        ("N25 — python's own walk is never re-paged",
         lambda: flines(pwalk(CAT_FM, 2), pwalk(CAT, 2), WALK, rt="python"), True),
    ]
    for name, fn, want in n2425:
        lines = fn()
        ok = bool(lines) == want
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} {name}: {len(lines)} line(s), "
              f"expected {'>0' if want else '0'}")

    # ── The diff is written INSIDE the run tree, never beside it. ───────────
    for name, given, want in (
            ("a <run>/<runtime> capture writes to <run>/diff",
             RUNS / "20260922/rust", RUNS / "20260922/diff"),
            ("a capture that IS a run directory writes INSIDE itself",
             RUNS / "20260922_sw", RUNS / "20260922_sw/diff"),
    ):
        got = diff_dir(given)
        ok = got == want and got != RUNS / "diff" and RUNS in got.parents
        bad += 0 if ok else 1
        total += 1
        print(f"  {'ok  ' if ok else 'FAIL'} H — {name}: {got}")

    print(f"rulecheck: {total - bad}/{total}")
    return 1 if bad else 0


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    s = sub.add_parser("self")
    s.add_argument("--a", required=True)
    s.add_argument("--b", required=True)
    p = sub.add_parser("port")
    p.add_argument("--base", required=True)
    p.add_argument("--other", required=True)
    p.add_argument("--rules", default=str(HERE / "rules.yaml"))
    p.add_argument("--residuals", default=str(HERE / "residuals.yaml"))
    p.add_argument("--proofs", default=str(HERE / "proofs.yaml"))
    p.add_argument("--landed", default=str(HERE / "landed.yaml"))
    p.add_argument("--root-alias", default=None)
    c = sub.add_parser("check-proofs")
    c.add_argument("--proofs", default=str(HERE / "proofs.yaml"))
    c.add_argument("--plan", default=str(PLAN))
    sub.add_parser("rulecheck")
    t = sub.add_parser("status")
    t.add_argument("--diff", required=True)
    t.add_argument("--proofs", default=str(HERE / "proofs.yaml"))
    t.add_argument("--landed", default=str(HERE / "landed.yaml"))
    t.add_argument("--plan", default=str(PLAN))
    a = ap.parse_args()
    if a.cmd == "self":
        return cmd_self(Path(a.a), Path(a.b))
    if a.cmd == "port":
        return cmd_port(Path(a.base), Path(a.other), a.rules, a.residuals, a.proofs,
                        a.landed, a.root_alias)
    if a.cmd == "rulecheck":
        return cmd_rulecheck()
    if a.cmd == "check-proofs":
        return cmd_check_proofs(a.proofs, Path(a.plan))
    return cmd_status(Path(a.diff), a.proofs, a.landed, Path(a.plan))


if __name__ == "__main__":
    sys.exit(main())
