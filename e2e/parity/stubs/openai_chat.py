"""The OpenAI chat surface — `POST <endpoint>/chat/completions`, SSE — scripted.

`vllm_backend` speaks it, and every AI agent the families drive is a
`vllm_backend` agent pointed here. Its record names this stub's `/v1` URL as
`endpoint`; no `credentials` unless a case is about the key.

Three ways a family drives it, all deterministic on every runtime:

  * a CHAIN — `set_chain(label, [turn, turn, …])`, answered in order;
  * a CASE — the family plants `CASE=<name>#<n>` in the text it sends, the stub
    finds it in the newest message and answers that case's script from the
    top. This is how one boot can run twenty independent scripts without the
    turn counter of one leaking into the next (lifted from
    `SCR/parity_capture_store.py:142-178`);
  * a RESPONDER — `responder(req, raw) -> str | dict | None` takes the wheel
    (ctx classifies each request by what the kernel put in it); `None` falls
    through to the case/chain script.

A script entry that is a dict instead of a string asks for something other than
a clean stream:

  * `status` (+ `body`, `content_type`, `retry_after`) — an HTTP error, no stream;
  * `stall` — the response held open that many seconds before its first byte;
  * `stall_mid` — the first LINE of the answer, a stall, then the rest: a token
    event must reach the caller before the whole reply is released (B2.11);
  * `cut` — the stream ends with NO `finish_reason` and no `[DONE]`, which
    vllm_backend reads as a server that died mid-answer;
  * `finish_reason` — the final chunk's reason (default `stop`; `length` is a cut);
  * `reasoning` — `reasoning_content` deltas streamed BEFORE the answer, the
    channel vLLM's reasoning parser uses;
  * `chunk` — the answer split across deltas of that many characters (the wire
    family measures one token event per delta); otherwise the answer is ONE
    delta, so a prompt diff is not drowned in framing.

The stalls are the only timing the harness owns, and each is a CONSTANT per
case, so `FANTASTIC_AI_SEND_TIMEOUT` and the tag `<timeout>` items measure the
kernel's clock rather than the machine's load.

NO `usage` is streamed. ai_core calibrates its token estimate by a reported
prompt size, and a stub's number would be invented: with none, the calibration
stays 1.0 and every compaction trigger the families plan (`spec.AI_WINDOW`,
`spec.CTX_WINDOW`) lands where the estimate puts it.
"""

from __future__ import annotations

import json
import re
import time

from . import JSONHandler, StubBase

CASE_RE = re.compile(r"CASE=([a-z0-9_]+)#(\d+)")
DONE = '<call name="done"></call>'


def content_text(m) -> str:
    """One message's text: a plain string, or the `text` of each content part."""
    c = m.get("content") if isinstance(m, dict) else None
    if isinstance(c, str):
        return c
    if isinstance(c, list):
        return "".join(b.get("text", "") for b in c
                       if isinstance(b, dict) and isinstance(b.get("text"), str))
    return ""


def last_user_text(req: dict) -> str:
    """The NEWEST message's text — where a family plants its case marker."""
    msgs = req.get("messages") if isinstance(req, dict) else None
    if not isinstance(msgs, list) or not msgs:
        return ""
    return content_text(msgs[-1])


def all_text(req: dict) -> str:
    """Every message's text, the system message included (it is message 0)."""
    msgs = req.get("messages") if isinstance(req, dict) else None
    return "\n".join(content_text(m) for m in (msgs if isinstance(msgs, list) else []))


class OpenAIChatStub(StubBase):
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
        self.responder = None

    def base_url(self) -> str:
        """What a `vllm_backend` record names as `endpoint` — the `/v1` base."""
        return f"http://127.0.0.1:{self.port}/v1"

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


def chunk_line(delta: dict, finish_reason: str | None = None) -> bytes:
    return ("data: " + json.dumps({
        "id": "stub", "object": "chat.completion.chunk", "model": "stub",
        "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
    }) + "\n\n").encode()


class Handler(JSONHandler):
    outer: OpenAIChatStub

    def do_POST(self):  # noqa: N802 - stdlib signature
        raw = self.read_body()
        try:
            req = json.loads(raw)
        except ValueError:
            req = {}
        answer, how = self.outer.pick(req, raw)
        plan = answer if isinstance(answer, dict) else {"text": answer}
        self.outer.record(self.path, self.headers, raw,
                          {**how, **{k: v for k, v in plan.items() if k != "text"}})

        if plan.get("stall"):
            time.sleep(float(plan["stall"]))
        if plan.get("status"):
            body = plan.get("body", "")
            data = body.encode() if isinstance(body, str) else json.dumps(body).encode()
            extra = {}
            if plan.get("retry_after") is not None:
                extra["Retry-After"] = str(plan["retry_after"])
            self.answer(int(plan["status"]), plan.get("content_type", "application/json"),
                        data, extra)
            return

        text = plan.get("text", DONE)
        # CHUNKED, so a mid-stream stall is possible: `content-length` would force
        # the whole body out at once and the token-before-release item (B2.11)
        # could not be measured.
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("transfer-encoding", "chunked")
        self.send_header("connection", "close")
        self.end_headers()
        self.close_connection = True

        def write(b: bytes) -> None:
            self.wfile.write(f"{len(b):X}\r\n".encode() + b + b"\r\n")
            self.wfile.flush()

        if plan.get("reasoning"):
            write(chunk_line({"role": "assistant", "content": None,
                              "reasoning_content": plan["reasoning"]}))
        if plan.get("stall_mid"):
            head, nl, tail = text.partition("\n")
            parts = [head + nl, tail]
        elif plan.get("chunk"):
            n = int(plan["chunk"])
            parts = [text[i:i + n] for i in range(0, len(text), n)]
        else:
            parts = [text]
        for i, part in enumerate(parts):
            if i == 1 and plan.get("stall_mid"):
                time.sleep(float(plan["stall_mid"]))
            if part:
                write(chunk_line({"content": part}))
        if not plan.get("cut"):
            write(chunk_line({}, plan.get("finish_reason", "stop")))
            write(b"data: [DONE]\n\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()
