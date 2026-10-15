"""THE CANVAS TEST FIXTURE — one rig every CLI canvas test shares (E40 is its
base).

The fixture is the PRODUCT: the `fantastic-tui` binary, `fantastic
canvas` in a fresh directory, a real headless Chrome over CDP. A live test takes
its model from the kernel suite's plug (`BACKEND=…`, `e2e/backends`); a
scripted test runs no model.

ARTIFACTS stay NEAR, and are safe to wipe: every run writes under
`src/tui/e2e/runs/<stamp>/<test id>/` (gitignored) — the host's log (the kernel's
message stream), the project dir, the page's final DOM and a summary — and
`src/tui/e2e/wipe.sh` clears them. Nothing goes to the system temp dir.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import time
import urllib.request
from contextlib import asynccontextmanager
from pathlib import Path
from typing import Any

import yaml

LIB = Path(__file__).resolve().parent
REPO = next(d for d in LIB.parents if (d / "e2e" / ".e2e-root").exists())
sys.path.insert(0, str(REPO / "e2e"))
sys.path.insert(0, str(REPO / "e2e" / "runners"))

import websockets  # noqa: E402

from canvas import MAC_CHROME, Page, attach, free_port  # noqa: E402
from e2e_build import build_lock  # noqa: E402
from helpers.verdict import verdict  # noqa: E402

SRC = REPO / "src"
# e2e builds go to their OWN target dir: the dev binary (`src/target/debug/fantastic`,
# often linked as `~/.local/bin/fantastic`) keeps the features its owner built it with
E2E_TARGET = SRC / "target" / "e2e"
BIN = E2E_TARGET / "debug" / "fantastic"
RUNS = SRC / "tui" / "e2e" / "runs"
#: The backends the CLI links, and the cargo feature that links each.
FEATURES = {
    "responses_backend.tools": "responses",
    "vllm_backend.tools": "vllm",
    "ollama_backend.tools": "ollama",
    "anthropic_backend.tools": "anthropic",
    "foundation_models_backend.tools": "foundation-models",
}
#: Where a run's key comes from, per provider (the plug's own environment).
KEY_VARS = {"vllm": "VLLM_API_KEY", "responses": "OPENAI_API_KEY"}


#: A cell's geometry — kept in its view's layout per theme (`layout.ts`), never on the cell.
GEOMETRY = ("x", "y", "width", "height")

def host_of(endpoint: str) -> str:
    """The `host[:port]` of a URL, lowercased — as the product's `kern/keys`."""
    rest = endpoint.split("://", 1)[-1]
    return rest.split("/")[0].split("?")[0].split("#")[0].rsplit("@", 1)[-1].lower()


def seed_key(home: Path, record: dict) -> dict:
    """The PRODUCT's key path, seeded the way a user's set-up leaves it: the
    plain key mode, the ref remembered, its value in the 0600 credentials file.
    Returns the saved set-up without the plug's own `credentials` (the e2e `creds`
    store does not exist here): the product names its own ref at spawn."""
    rec = {k: v for k, v in record.items() if k != "credentials"}
    # the window the model has (owner: 262144 for window AND output — vLLM gives
    # a turn what the prompt leaves of it when max_tokens reaches the window)
    if (ctx := os.environ.get("CTX")) and "context_window" not in rec:
        rec["context_window"] = int(ctx)
    # the thought: the RUN's, when it names one (REASONING_EFFORT,
    # THINKING_TOKEN_BUDGET); else vllm_backend's own (medium, its adapted budget)
    if rec.get("handler_module") == "vllm_backend.tools":
        if effort := os.environ.get("REASONING_EFFORT"):
            rec["reasoning_effort"] = effort
        if budget := os.environ.get("THINKING_TOKEN_BUDGET"):
            rec["thinking_token_budget"] = int(budget)
    provider = str(rec.get("handler_module", "")).removesuffix("_backend.tools")
    key = os.environ.get(KEY_VARS.get(provider, ""), "")
    if not key or not rec.get("endpoint"):
        return rec
    ref, host = f"{provider}/{host_of(rec['endpoint'])}", host_of(rec["endpoint"])
    private_write(keys_dir(home) / "mode", "plain")
    private_write(keys_dir(home) / "refs.json", json.dumps({ref: host}))
    # at rest the value carries its HOST (`{host, fields}`): a stored value with
    # no host is refused at read
    private_write(keys_dir(home) / "credentials.yaml", yaml.safe_dump({"refs": {ref: {"host": host, "fields": {"api_key": key}}}}))
    return rec


def saved_dir(home: Path) -> Path:
    """The home's AI saved set-ups: `agents/ai_agent/saved`."""
    return home / "agents" / "ai_agent" / "saved"


def keys_dir(home: Path) -> Path:
    """The home's keys (mode, refs, the plain file): `agents/ai_agent/keys`."""
    return home / "agents" / "ai_agent" / "keys"


def private_dirs(d: Path) -> None:
    """`d` and every missing parent made 0700 — as the home store wants every
    level (`os.makedirs`'s mode reaches only the leaf since Python 3.7)."""
    missing = []
    while not d.exists():
        missing.append(d)
        d = d.parent
    for m in reversed(missing):
        m.mkdir(mode=0o700)
        os.chmod(m, 0o700)


def private_write(p: Path, text: str) -> None:
    private_dirs(p.parent)
    fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.write(fd, text.encode())
    os.close(fd)


# the root view's backend folder on the host: a cell's backend lives here
FOLDER = "agents/user/agents/web/agents/canvas_page/agents/canvas_root"


#: The runner's hard stop per test (`e2e/run.py` HARD_STOP_S), less a margin
#: for teardown and the verdict line.
HARD_STOP_S = 15 * 60 - 30
ENDED_HOLD_S = 3  # a `done` (not awaiting) that holds this long ends the wait


class Rig:
    """One test's world: its artifacts dir, its binary, its home, its host."""

    def __init__(self, test_file: str, live: bool = True):
        # the runner's HARD STOP (e2e/run.py, owner 2026-10-06: 15 min a test)
        # counts from here — build and boot included
        self.born = time.time()
        self.dir = Path(test_file).resolve().parent
        self.spec = yaml.safe_load((self.dir / "test.yaml").read_text()) or {}
        self.id = self.spec["id"]
        self.limits = self.spec.get("limits") or {}
        stamp = os.environ.get("CANVAS_E2E_STAMP") or time.strftime("%Y%m%d-%H%M%S")
        self.art = RUNS / stamp / self.id
        if self.art.exists():
            shutil.rmtree(self.art)
        self.art.mkdir(parents=True)
        self.proj = self.art / "proj"
        self.proj.mkdir()
        self.home = self.art / "home"
        self.log = self.art / "host.log"
        self.live = live
        self.plug = None
        self.host: subprocess.Popen | None = None
        self.url: str | None = None
        self.port = free_port()
        self.summary: dict[str, Any] = {"id": self.id}

    # ── the model plug (live tests) ─────────────────────────────────────────
    def plug_or_refusal(self) -> str | None:
        """The plug's blockers, or None when it can run on this CLI."""
        from backends import backend

        self.plug = backend()
        if self.plug.blockers():
            return "the plug cannot run: " + "; ".join(self.plug.blockers())
        if self.plug.handler_module not in FEATURES:
            return f"the CLI links no {self.plug.handler_module} — it links {', '.join(FEATURES)}"
        return None

    def build(self, *features: str) -> None:
        feats = sorted({*features, *([FEATURES[self.plug.handler_module]] if self.plug else [])})
        args = ["cargo", "build", "-p", "fantastic-tui", *(["--features", ",".join(feats)] if feats else [])]
        # the build AND the copy under one lock: another test's build (other
        # features) cannot land in between
        with build_lock(E2E_TARGET):
            src = Path(os.environ["FANTASTIC_BIN"]).expanduser().resolve() if os.environ.get("FANTASTIC_BIN") else BIN
            if not os.environ.get("FANTASTIC_BIN"):
                r = subprocess.run(args, cwd=SRC, capture_output=True, text=True, env={**os.environ, "CARGO_TARGET_DIR": str(E2E_TARGET)})
                if r.returncode != 0:
                    raise SystemExit(f"{' '.join(args)} failed:\n{r.stderr[-2000:]}")
            self.binary = self.art / "fantastic"
            shutil.copy2(src, self.binary)  # a private copy: another build cannot swap it mid-run

    def make_home(self, record: dict | None = None, name: str = "plug") -> None:
        """A throwaway FANTASTIC_HOME — with `record` saved as the default, or none."""
        private_dirs(saved_dir(self.home))
        os.chmod(self.home, 0o700)
        if record is not None:
            record = seed_key(self.home, record)
            private_write(saved_dir(self.home) / f"{name}.yaml", yaml.safe_dump(record, sort_keys=False))
            private_write(saved_dir(self.home) / ".preferred", name)

    # ── the host ──────────────────────────────────────────────────────────
    def env(self) -> dict:
        return {**os.environ, "FANTASTIC_HOME": str(self.home)}

    def start(self) -> None:
        first = not (self.proj / ".fantastic").exists()
        args = [str(self.binary), "canvas", *(["--port", str(self.port)] if first else [])]
        with open(self.log, "a") as f:
            f.write("\n=== start\n")
        self.host = subprocess.Popen(args, cwd=self.proj, env=self.env(), stdout=open(self.log, "a"), stderr=subprocess.STDOUT)
        for _ in range(600):
            if self.host.poll() is not None:
                raise SystemExit(f"the host exited {self.host.returncode}: {self.log.read_text()[-800:]}")
            run = self.log.read_text().split("=== start")[-1]
            m = re.search(r"^fantastic: the canvas: (\S+)", run, re.M)
            if m:
                self.url = m.group(1)
                return
            time.sleep(0.2)
        raise SystemExit("the host never announced the canvas")

    def stop(self) -> None:
        if self.host is None:
            return
        self.host.send_signal(signal.SIGTERM)
        try:
            self.host.wait(30)
        except subprocess.TimeoutExpired:
            self.host.kill()
        self.host = None

    def init(self, cells: list[dict] | None = None, host_agents: list[tuple[str, dict]] | None = None) -> None:
        """A fresh project, then SEEDED the way the product stores it: first boot
        (inits the dir), one page load (the canvas root is made), stop, write the
        seed records, boot again."""
        self.start()
        asyncio.run(self._touch())
        self.stop()
        if cells:
            self.seed_cells(cells)
        for parent, rec in host_agents or []:
            self.seed_host_agent(parent, rec)
        self.start()

    async def _touch(self) -> None:
        # until the page has STORED its root view: seeds written before that land
        # under a view the next boot makes afresh, and never load
        root = self.cells_dir.parent / "agent.yaml"
        async with self.page() as pg:
            for _ in range(120):
                if root.is_file():
                    break
                await asyncio.sleep(0.25)
            else:
                (self.art / "touch_page_errors.json").write_text(json.dumps(pg.logs, indent=1))
                (self.art / "touch_page.html").write_text(await pg.js("document.documentElement.outerHTML") or "")
                raise SystemExit(f"the page never stored its root view: {root}")
            await asyncio.sleep(1.0)

    # ── the disk: seeding and reading the two trees ─────────────────────────
    @property
    def cells_dir(self) -> Path:
        return self.proj / ".fantastic" / "web" / "agents" / "canvas" / "agents" / "view_root" / "agents"

    def seed_cells(self, cells: list[dict]) -> None:
        """Write page cells into the root view as the page loader stores them (host stopped):
        each cell's x/y/width/height into the view's canvas-theme layout, never on the cell."""
        rects = self.rects()
        for c in cells:
            rec = {"parent_id": "view_root", **{k: v for k, v in c.items() if k not in GEOMETRY}}
            g = {k: c[k] for k in GEOMETRY if k in c}
            if g:
                rects[c["id"]] = {"x": 0, "y": 0, "width": 480, "height": 320, **rects.get(c["id"], {}), **g}
            (self.cells_dir / c["id"]).mkdir(parents=True, exist_ok=True)
            (self.cells_dir / c["id"] / "agent.yaml").write_text(yaml.safe_dump(rec, sort_keys=False, allow_unicode=True))
        if rects:
            d = self.cells_dir / "layout_canvas"
            d.mkdir(parents=True, exist_ok=True)
            (d / "agent.yaml").write_text(yaml.safe_dump({"id": "layout_canvas", "parent_id": "view_root", "handler_module": "layout.ts",
                                                          "theme": "canvas", "rects": rects}, sort_keys=False))

    def pinned(self, theme: str = "mobile") -> str | None:
        """The cell pinned in `theme` (its layout agent's `pinned`)."""
        f = self.cells_dir / f"layout_{theme}" / "agent.yaml"
        return (yaml.safe_load(f.read_text()) or {}).get("pinned") if f.exists() else None

    def rects(self, theme: str = "canvas") -> dict:
        """Where the root view's cells sit in `theme` (its layout agent's rects)."""
        f = self.cells_dir / f"layout_{theme}" / "agent.yaml"
        return dict((yaml.safe_load(f.read_text()) or {}).get("rects") or {}) if f.exists() else {}

    def seed_host_agent(self, parent_path: str, rec: dict) -> None:
        """Write a HOST agent record under `parent_path` (e.g. `FOLDER`, the
        root view's backend folder) — loaded at the next boot."""
        d = self.proj / ".fantastic" / parent_path / "agents" / rec["id"]
        d.mkdir(parents=True, exist_ok=True)
        (d / "agent.yaml").write_text(yaml.safe_dump(rec, sort_keys=False))

    def patch_host_agent(self, agent_id: str, fields: dict) -> None:
        """Merge `fields` into a HOST agent's record on disk (host stopped)."""
        for f in (self.proj / ".fantastic").rglob(f"{agent_id}/agent.yaml"):
            rec = yaml.safe_load(f.read_text()) or {}
            rec.update(fields)
            f.write_text(yaml.safe_dump(rec, sort_keys=False))
            return
        raise SystemExit(f"no host record {agent_id} to patch")

    def cells(self) -> list[dict]:
        out = []
        for f in sorted(self.cells_dir.glob("*/agent.yaml")):
            r = yaml.safe_load(f.read_text()) or {}
            if r.get("handler_module") not in ("background.ts", "layout.ts"):
                out.append(r)
        return out

    def page_guid(self) -> str:
        f = self.proj / ".fantastic/web/agents/canvas/agent.yaml"
        return (yaml.safe_load(f.read_text()) or {}).get("guid", "") if f.exists() else ""

    def host_record(self, agent_id: str) -> dict | None:
        for f in (self.proj / ".fantastic").rglob(f"{agent_id}/agent.yaml"):
            return yaml.safe_load(f.read_text()) or {}
        return None

    # ── the evidence: the host's own stream ─────────────────────────────────
    def page_calls(self) -> list[str]:
        """Every call to a PAGE agent, `<page guid>|<agent> call <verb>` (only a
        cell's AI holds peer grants)."""
        g = self.page_guid()
        if not g:
            return []
        return [line.split(" ", 1)[1] for line in self.log.read_text().splitlines()
                if re.match(rf"^\[\d+\] {re.escape(g)}\|\S+ call \S+", line)]

    def looks_since_spawn(self) -> list[str]:
        """How the AI LOOKED at the canvas once its cell existed: every call to a
        page agent, and every host-side read of the canvas's records (`web_loader
        load_tree` — its control-plane reach allows it) after its view folder's `spawn`."""
        g = self.page_guid()
        out, armed = [], False
        for line in self.log.read_text().splitlines():
            if re.match(r"^\[\d+\] canvas_root call spawn", line):
                armed = True
            elif armed and (re.match(r"^\[\d+\] web_loader call load_tree", line)
                            or (g and re.match(rf"^\[\d+\] {re.escape(g)}\|\S+ call \S+", line))):
                out.append(line.split(" ", 1)[1])
        return out

    # ── the browser ───────────────────────────────────────────────────────
    @asynccontextmanager
    async def page(self, width: int = 1500, height: int = 950, extra: list[str] | None = None):
        cdp = free_port()
        # CANVAS_HEADFUL=1: a visible Chrome on the real GPU (a load test's frame
        # rate is the GPU's, not the software rasterizer's)
        # headless: software WebGL (SwiftShader) — the terminals draw with WebGL
        mode = [] if os.environ.get("CANVAS_HEADFUL") == "1" else ["--headless=new", "--use-angle=swiftshader", "--enable-unsafe-swiftshader"]
        chrome = subprocess.Popen([MAC_CHROME, *mode, f"--window-size={width},{height}", f"--remote-debugging-port={cdp}",
                                   f"--user-data-dir={self.art / 'chrome'}", "--no-first-run",
                                   "--autoplay-policy=no-user-gesture-required", *(extra or []), "about:blank"],
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            async with await attach(cdp) as ws:
                pg = Page(ws)
                await pg.cdp("Runtime.enable")
                await pg.cdp("Page.enable")
                if os.environ.get("CANVAS_WS_LOG") == "1":
                    pg.ws_log = []
                    await pg.cdp("Network.enable")
                if os.environ.get("CANVAS_COUNT_RO") == "1":
                    # count ResizeObserver callbacks (a load probe's suspect)
                    await pg.cdp("Page.addScriptToEvaluateOnNewDocument", source="""(()=>{const R=window.ResizeObserver; window.__ro=0;
                      window.ResizeObserver=class extends R{constructor(cb){super((...a)=>{window.__ro++; return cb(...a)})}}})()""")
                await pg.open(self.url)
                await pg.until("!!document.getElementById('canvas')", 30)
                await asyncio.sleep(1.5)
                yield pg
                html = await pg.js("document.documentElement.outerHTML")
                (self.art / "page.html").write_text(html or "")
                (self.art / "page_errors.json").write_text(json.dumps(pg.logs, indent=1))
                if hasattr(pg, "ws_log"):
                    (self.art / "ws_frames.log").write_text("\n".join(pg.ws_log))
        finally:
            chrome.terminate()

    def left(self, reserve: float = 0.0) -> float:
        """Seconds still usable under the runner's 15-minute hard stop, keeping
        `reserve` for what must run after (the scripted checks)."""
        return max(0.0, HARD_STOP_S - (time.time() - self.born) - reserve)

    def budget(self, limit: float, reserve: float = 0.0) -> float:
        """The test's own `limit`, cut to what the hard stop leaves."""
        return min(float(limit), self.left(reserve))

    def finish(self, ok: bool, note: str) -> tuple[bool, str]:
        """Record the summary; the probe prints the machine verdict itself
        (`return verdict(*rig.finish(ok, note))`)."""
        self.summary.update({"ok": ok, "note": note, "page_calls": self.page_calls()})
        (self.art / "summary.json").write_text(json.dumps(self.summary, indent=1, ensure_ascii=False))
        print(f"artifacts: {self.art}")
        return ok, note


# ── page helpers ─────────────────────────────────────────────────────────────
async def spot(pg: Page, x: int, y: int, text: str) -> None:
    """Double-click empty canvas at (x, y) and type into the action popup."""
    for c in (1, 2):
        await pg.mouse("mousePressed", x, y, c)
        await pg.mouse("mouseReleased", x, y, c)
    await pg.until("!!document.querySelector('.action-popup input')", 10)
    await pg.cdp("Input.insertText", text=text)
    await asyncio.sleep(0.3)
    await pg.cdp("Input.dispatchKeyEvent", type="keyDown", key="Enter", code="Enter", windowsVirtualKeyCode=13)


CHAT_IDS = "[...document.querySelectorAll('.chat-view')].map(v=>v.closest('.agent-frame').dataset.id)"


async def other_setups(pg: Page, cell: str) -> None:
    """AI cell `cell`'s ⚙ settings → `‹ other set-ups`: page 1 of the wizard FOR
    that cell (a saved set-up or a new provider rebinds it). The old chat view is
    marked, so `rebound` can tell the new one."""
    await pg.js(f"document.querySelector('.agent-frame[data-id=\"{cell}\"] .chat-view')?.setAttribute('data-old', '1')")
    gear = f"[...document.querySelectorAll('.agent-frame[data-id=\"{cell}\"] > .frame-head .ext')].find(c=>c.textContent==='⚙')"
    # the ⚙ chip comes with the mounted chat view, a moment after the cell
    await pg.until(f"!!{gear}", 15)
    await pg.js(f"{gear}.click()")
    await pg.until("(document.querySelector('.wiz h1')?.textContent||'').startsWith('Settings')", 15)
    await pg.js("[...document.querySelectorAll('.wiz button')].find(b=>b.textContent.trim()==='‹ other set-ups')?.click()")
    await pg.until("document.querySelectorAll('.wiz .tiles button').length === 4", 10)


async def rebound(pg: Page, cell: str, timeout: float = 8) -> bool:
    """True once `cell` shows a NEW chat view (not the one `other_setups` marked) and the wizard is gone."""
    return bool(await pg.until(f"!!document.querySelector('.agent-frame[data-id=\"{cell}\"] .chat-view:not([data-old])') && !document.querySelector('.wiz')", timeout))


async def new_ai_wizard(pg: Page, x: int, y: int) -> str | None:
    """The wizard for a new AI from the double-click's ONE `AI agent` tile: with
    no default set-up it opens the wizard (None); with one it makes an AI cell on
    it, and that cell's ⚙ → `‹ other set-ups` is the wizard for it (its id)."""
    before = set(await pg.js(CHAT_IDS))
    await spot(pg, x, y, "AI agent")
    await pg.until(f"!!document.querySelector('.wiz .tiles button') || {CHAT_IDS}.length > {len(before)}", 20)
    new = set(await pg.js(CHAT_IDS)) - before
    if not new:
        await pg.until("document.querySelectorAll('.wiz .tiles button').length === 4", 10)
        return None
    cell = new.pop()
    await other_setups(pg, cell)
    return cell


async def open_sidebar(pg: Page, x: int, y: int) -> bool:
    """Place a TREE VIEW cell (the agent tree; a `sidebar_view.ts` cell) at empty
    canvas (x, y) — nothing floats: it is a cell from the double-click."""
    before = await pg.js("document.querySelectorAll('.agent-frame .side .agents-tree').length")
    await spot(pg, x, y, "tree view")
    if not await pg.until(f"document.querySelectorAll('.agent-frame .side .agents-tree').length > {before}", 10):
        return False
    # its host tree is read from the host (async): ready once a host row shows
    return bool(await pg.until("document.querySelectorAll('.side .agents-tree li.agent.host').length > 0", 10))


async def say(pg: Page, text: str, cell: int = 0) -> None:
    """Send a follow-up turn in chat cell #cell — ENTER in its input (it queues
    even while the cell works; the button turns into STOP then, an interrupt)."""
    await pg.js(f"(()=>{{const v=document.querySelectorAll('.chat-view')[{cell}]; const i=v.querySelector('.c-input');"
                f" i.value={json.dumps(text)}; i.dispatchEvent(new KeyboardEvent('keydown',{{key:'Enter',bubbles:true}}));}})()")


async def wait_quiet(pg: Page, total: float, quiet: float, cell: int = 0) -> bool:
    """Until chat cell #cell's chain is OVER — its last `done` names no work
    still awaited — or, when that event never arrives, until it has answered
    in words and gone QUIET `quiet` s."""
    deadline = time.time() + total
    last_n, since = -1, time.time()
    seen_live = False
    ended = 0.0
    sel = f"document.querySelectorAll('.chat-view')[{cell}]"
    while time.time() < deadline:
        n = await pg.js(f"({sel}?.querySelectorAll('.msg').length) ?? 0") or 0
        said = await pg.js(f"[...({sel}?.querySelectorAll('.msg.assistant:not(.thinking):not(.c-think)') ?? [])].some(m=>m.textContent.trim())")
        # still WORKING — the flashing placeholder, or the cell's own phase pill
        # (thinking / streaming / tool_calling / queued; a turn that reasons
        # between two calls shows no placeholder) — is never quiet
        thinking = await pg.js(f"!!{sel}?.querySelector('.msg.thinking') || "
                               f"['thinking','streaming','tool_calling','queued'].includes({sel}?.querySelector('.phase-pill')?.dataset.phase)")
        if thinking:
            seen_live = True
        # THE TRIGGER: the turn's `done` arrived and its reason is not
        # `awaiting` — the chain is over, nothing to wait for. Only after this
        # wait SAW the cell work (a `done` left from an earlier turn is no end).
        # `ok` closes EVERY generation, before its calls dispatch: the end must
        # HOLD `ENDED_HOLD_S` with no new phase (a live chain goes tool_calling).
        reason = await pg.js(f"{sel}?.querySelector('.phase-pill')?.dataset.reason || ''")
        if seen_live and said and not thinking and reason not in ("", "awaiting"):
            ended = ended or time.time()
            if time.time() - ended >= ENDED_HOLD_S:
                return True
        else:
            ended = 0.0
        if n != last_n or thinking:
            last_n, since = n, time.time()
        elif said and time.time() - since >= quiet:
            # the `done` event did not reach this cell (it is not always
            # delivered): the old quiet rule ends the wait
            return True
        await asyncio.sleep(0.5)
    return False


async def answer(pg: Page, cell: int = 0) -> str:
    """The AI's last WORDS: its last assistant bubble with text, skipping one that
    is only call markup — a malformed call (the server's dropped `<`, e.g.
    `call name="done"></call>`) shows as text but is no answer."""
    return await pg.js(f"(()=>{{const m=[...(document.querySelectorAll('.chat-view')[{cell}]?.querySelectorAll('.msg.assistant:not(.thinking):not(.c-think)') ?? [])]"
                       ".filter(x=>{const t=x.textContent.trim(); return t && !/^(<?\\/?call\\b[^>]*>\\s*)+$/.test(t);});"
                       " return m.length ? m[m.length-1].textContent : '';})()") or ""


async def in_frame(pg: Page, cell_id: str, expr: str) -> Any:
    """Evaluate `expr` inside cell `cell_id`'s panel iframe (a trusted panel is
    same-origin; `doc` is its document)."""
    return await pg.js(
        f"(()=>{{const f=document.querySelector('.agent-frame[data-id='+JSON.stringify({json.dumps(cell_id)})+']')?.querySelector('iframe');"
        f" const doc=f?.contentDocument; if(!doc) return null; return ({expr});}})()"
    )


def http_get(url: str, timeout: float = 5) -> tuple[int, str]:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as r:
            return r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, ""
    except Exception:
        return 0, ""


async def ask(pg: Page, x: int, y: int, text: str, total: float, quiet: float) -> tuple[bool, str]:
    """Open an AI cell at (x, y) with `text` as its first turn; wait it out."""
    n = await pg.js("document.querySelectorAll('.chat-view').length") or 0
    await spot(pg, x, y, text)
    await pg.until(f"document.querySelectorAll('.chat-view').length > {n}", 30)
    done = await wait_quiet(pg, total, quiet, cell=n)
    return done, await answer(pg, cell=n)


def html_cell(cid: str, name: str, x: int, y: int, html: str, w: int = 340, h: int = 240, **extra: Any) -> dict:
    """A trusted html panel record, as a ported project holds one."""
    return {"id": cid, "handler_module": "html_agent.ts", "display_name": name, "trusted": True,
            "html": html, "x": x, "y": y, "width": w, "height": h, **extra}


def probe_main(fn) -> None:
    """Run a probe's async main and exit with its verdict."""
    ok = fn()
    sys.exit(0 if ok else 1)
