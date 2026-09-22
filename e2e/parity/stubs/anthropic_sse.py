"""The anthropic backend's endpoint, scripted.

Three ways a family drives it, all deterministic on every runtime:

  * a CHAIN — `set_chain(label, [turn, turn, …])`, answered in order;
  * a CASE — the family plants `CASE=<name>#<n>` in the text it sends, the stub
    finds it in the newest user message and answers that case's script from the
    top. This is how one boot can run twenty independent scripts without the
    turn counter of one leaking into the next (lifted from
    `SCR/parity_capture_store.py:142-178`);
  * a FAULT — a script entry that is a dict instead of a string asks for
    something other than a clean 200: an HTTP status with a body, an SSE
    `event: error`, a stream cut before `message_stop`, a `stop_reason` with no
    block, or a stall that holds the response open.

The stall is the only timing the harness owns, and it is a CONSTANT per case, so
`FANTASTIC_AI_SEND_TIMEOUT` and the tag `<timeout>` items measure the kernel's
clock rather than the machine's load.
"""

from __future__ import annotations

import json
import re
import time

from . import JSONHandler, StubBase, anthropic_events, sse

CASE_RE = re.compile(r"CASE=([a-z0-9_]+)#(\d+)")
DONE = '<call name="done"></call>'


def last_user_text(req: dict) -> str:
    msgs = req.get("messages") if isinstance(req, dict) else None
    if not isinstance(msgs, list) or not msgs:
        return ""
    c = msgs[-1].get("content") if isinstance(msgs[-1], dict) else None
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(b.get("text", "") for b in c
                       if isinstance(b, dict) and isinstance(b.get("text"), str))
    return ""


def all_text(req: dict) -> str:
    out = []
    sysv = req.get("system") if isinstance(req, dict) else None
    if isinstance(sysv, str):
        out.append(sysv)
    elif isinstance(sysv, list):
        out += [b.get("text", "") for b in sysv if isinstance(b, dict)]
    for m in (req.get("messages") or []) if isinstance(req, dict) else []:
        c = m.get("content") if isinstance(m, dict) else None
        if isinstance(c, str):
            out.append(c)
        elif isinstance(c, list):
            out += [b.get("text", "") for b in c if isinstance(b, dict) and isinstance(b.get("text"), str)]
    return "\n".join(out)


class AnthropicStub(StubBase):
    def __init__(self, rec, rel: str, port: int) -> None:
        super().__init__(rec, rel, port)
        self.cases: dict[str, list] = {}
        self.chain: list = []
        self.chain_label = "none"
        self.chain_n = 0
        self.state: dict = {"nonce": None, "case": None, "step": 0}
        #: HOW MANY TURNS ONE CASE MAY EVER GET, across every re-entry.
        #:
        #: A case whose script sends an AI back into another AI re-plants its
        #: marker each hop, so the case restarts from step 0 forever: the first
        #: run of `ai_paths` took 6722 model turns and wrote 16669 files before it
        #: was stopped. The cap ends the CHAIN, not the measurement — the item is
        #: whether the kernel's own cycle/depth guard fired first, and the answer
        #: is in the rows and in `cap_hits` either way.
        self.case_cap = 30
        self.case_turns: dict[str, int] = {}
        self.cap_hits: dict[str, int] = {}
        #: A family may take the wheel entirely (ctx classifies each request by
        #: what the kernel put in it). `responder(req, raw) -> str | dict | None`;
        #: `None` falls through to the case/chain script.
        self.responder = None

    def url(self) -> str:
        return f"http://127.0.0.1:{self.port}/v1/messages"

    def set_cases(self, cases: dict) -> None:
        self.cases = dict(cases)

    def set_chain(self, label: str, script: list) -> None:
        with self.lock:
            self.chain_label, self.chain, self.chain_n = label, list(script), 0

    # ── scripting ────────────────────────────────────────────────────────────
    def pick(self, req: dict, raw: bytes):
        if self.responder is not None:
            answer = self.responder(req, raw)
            if answer is not None:
                return answer, {"by": "responder"}
        text = last_user_text(req)
        m = CASE_RE.search(text) or CASE_RE.search(all_text(req))
        with self.lock:
            if m and m.group(0) != self.state["nonce"]:
                self.state.update(nonce=m.group(0), case=m.group(1), step=0)
            case = self.state["case"]
            if case and case in self.cases:
                step = self.state["step"]
                self.state["step"] += 1
                seen = self.case_turns.get(case, 0) + 1
                self.case_turns[case] = seen
                if seen > self.case_cap:
                    self.cap_hits[case] = self.cap_hits.get(case, 0) + 1
                    return DONE, {"by": "case_cap", "case": case, "turns": seen}
                script = self.cases[case]
                answer = script[step] if step < len(script) else DONE
                return answer, {"by": "case", "case": case, "step": step}
            step = self.chain_n
            self.chain_n += 1
            answer = self.chain[step] if step < len(self.chain) else DONE
            return answer, {"by": "chain", "label": self.chain_label, "step": step}


class Handler(JSONHandler):
    outer: AnthropicStub

    def do_POST(self):  # noqa: N802 - stdlib signature
        raw = self.read_body()
        try:
            req = json.loads(raw)
        except ValueError:
            req = {}
        answer, how = self.outer.pick(req, raw)
        plan = answer if isinstance(answer, dict) else {"text": answer}
        self.outer.record(self.path, self.headers, raw, {**how, **{k: v for k, v in plan.items() if k != "text"}})

        if plan.get("stall"):
            time.sleep(float(plan["stall"]))
        if plan.get("status"):
            body = plan.get("body", "")
            data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
            self.answer(int(plan["status"]), plan.get("content_type", "application/json"),
                        data, plan.get("extra_headers"))
            return

        text = plan.get("text", DONE)
        events = anthropic_events(text, stop_reason=plan.get("stop_reason", "end_turn"),
                                  chunk=plan.get("chunk"))
        if plan.get("event_error"):
            events = events[:3] + [("error", {"type": "error", "error": {
                "type": "overloaded_error", "message": "stub overloaded"}})]
        if plan.get("cut"):
            events = [e for e in events if e[0] != "message_stop"]
        data = sse(events)
        self.answer(200, "text/event-stream", data)
