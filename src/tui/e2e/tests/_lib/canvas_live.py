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
import atexit
import json
import os
import re
import shlex
import shutil
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

from canvas import DESKTOP_POINTER, MAC_CHROME, Page, attach, free_port  # noqa: E402
from e2e_build import build_lock  # noqa: E402
from helpers import oslayer  # noqa: E402
from helpers.chrome import Chrome  # noqa: E402
from helpers.verdict import verdict  # noqa: E402

SRC = REPO / "src"
# e2e builds go to their OWN target dir: the dev binary (`src/target/debug/fantastic`,
# often linked as `~/.local/bin/fantastic`) keeps the features its owner built it with
E2E_TARGET = SRC / "target" / "e2e"
BIN = oslayer.exe(E2E_TARGET / "debug" / "fantastic")
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


#: A cell's geometry — kept in its view's layout (`layout.ts`), never on the cell.
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
    """The home's AI saved set-ups: `user/setups/ai_agent`."""
    return home / "user" / "setups" / "ai_agent"


def keys_dir(home: Path) -> Path:
    """The home's keys (mode, refs, the plain file): `user/keys`."""
    return home / "user" / "keys"


def private_dirs(d: Path) -> None:
    """`d` and every missing parent made 0700 — as the home store wants every
    level (`os.makedirs`'s mode reaches only the leaf since Python 3.7)."""
    missing = []
    while not d.exists():
        missing.append(d)
        d = d.parent
    for m in reversed(missing):
        m.mkdir(mode=0o700)
        oslayer.make_private(m)


def private_write(p: Path, text: str) -> None:
    private_dirs(p.parent)
    fd = os.open(p, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    os.write(fd, text.encode())
    os.close(fd)
    oslayer.make_private(p)


# the root view's backend folder on the host: a cell's backend lives here
FOLDER = "agents/user/agents/web/agents/canvas_page/agents/canvas_root"

#: The interpreter a test PROGRAM runs under: this one, by absolute path, so no
#: program needs a `python3` on some PATH (Windows has none).
PY = sys.executable


def py_job(*args: str) -> tuple[list[str], str]:
    """`(argv, its exact allowed line)` for a job that runs `PY` on `args`."""
    argv = [PY, *args]
    return argv, "^" + re.escape(shlex.join(argv)) + "$"


def py_typed(*args: str) -> str:
    """A command line running `PY` on `args`, as typed into ANY shell (sh, bash,
    zsh, cmd.exe): the interpreter's path double-quoted, nothing else."""
    return " ".join([f'"{PY}"', *args])


#: The runner's hard stop per test (`e2e/run.py` HARD_STOP_S), less a margin
#: for teardown and the verdict line.
HARD_STOP_S = 15 * 60 - 30
#: The page size `page()` opens with by default, and the one `init()`'s Chrome is launched for.
DEFAULT_SIZE = (1500, 950)
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
        self._warm: Chrome | None = None  # the Chrome init() leaves up for the first page()
        self.url: str | None = None
        self.port = free_port()
        self.summary: dict[str, Any] = {"id": self.id}
        self.misses: list[str] = []

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

    def build(self, *features: str, example: str | None = None) -> None:
        """Build the CLI, and copy it to `self.binary`. `example`: also copy that cargo example of
        the CLI crate (an embedder) to `self.example` — the run's prebuilt `FANTASTIC_EMBEDDER`, else built here."""
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
            self.binary = oslayer.exe(self.art / "fantastic")
            shutil.copy2(src, self.binary)  # a private copy: another build cannot swap it mid-run
            if example:
                # the run's prebuilt embedder (e2e/build.py), else built here
                if built := os.environ.get("FANTASTIC_EMBEDDER"):
                    ex_src = Path(built).expanduser().resolve()
                else:
                    r = subprocess.run([*args, "--example", example], cwd=SRC, capture_output=True, text=True,
                                       env={**os.environ, "CARGO_TARGET_DIR": str(E2E_TARGET)})
                    if r.returncode != 0:
                        raise SystemExit(f"{' '.join(args)} --example {example} failed:\n{r.stderr[-2000:]}")
                    ex_src = oslayer.exe(E2E_TARGET / "debug" / "examples" / example)
                self.example = oslayer.exe(self.art / example)
                shutil.copy2(ex_src, self.example)

    def make_home(self, record: dict | None = None, name: str = "plug") -> None:
        """A throwaway FANTASTIC_HOME — with `record` saved as the default, or none."""
        private_dirs(saved_dir(self.home))
        oslayer.make_private(self.home)
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
        self.host = subprocess.Popen(args, cwd=self.proj, env=self.env(), stdout=open(self.log, "a"), stderr=subprocess.STDOUT,
                                     **oslayer.group_kwargs())
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
        oslayer.stop(self.host)
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
        # under a view the next boot makes afresh, and never load. The Chrome stays
        # up (self._warm): `page()` takes it over, so a scenario launches ONE Chrome.
        root = self.cells_dir.parent / "agent.yaml"
        self._warm = self._launch_chrome(*DEFAULT_SIZE, None)
        atexit.register(self._drop_warm)  # a probe that dies before its page() leaves no Chrome
        async with self._session(self._warm, *DEFAULT_SIZE) as pg:
            for _ in range(120):
                if root.is_file():
                    break
                await asyncio.sleep(0.25)
            else:
                (self.art / "touch_page_errors.json").write_text(json.dumps(pg.logs, indent=1))
                (self.art / "touch_page.html").write_text(await pg.js("document.documentElement.outerHTML") or "")
                raise SystemExit(f"the page never stored its root view: {root}")
            await asyncio.sleep(1.0)
            # the host stops next: this tab must not sit on a dead canvas, retrying
            await pg.cdp("Page.navigate", url="about:blank")

    # ── the disk: seeding and reading the two trees ─────────────────────────
    @property
    def cells_dir(self) -> Path:
        return self.proj / ".fantastic" / "web" / "agents" / "canvas" / "agents" / "view_root" / "agents"

    def seed_cells(self, cells: list[dict]) -> None:
        """Write page cells into the root view as the page loader stores them (host stopped):
        each cell's x/y/width/height into the view's free-canvas layout, never on the cell."""
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
                                                          "rects": rects}, sort_keys=False))

    def pinned(self, layout: str = "mobile") -> str | None:
        """The cell pinned in the layout agent `layout_<layout>` (its `pinned`): `mobile` is the column's."""
        f = self.cells_dir / f"layout_{layout}" / "agent.yaml"
        return (yaml.safe_load(oslayer.read_text(f)) or {}).get("pinned") if f.exists() else None

    def rects(self, layout: str = "canvas") -> dict:
        """Where the root view's cells sit in the layout agent `layout_<layout>` (its rects): `canvas` is the free canvas, `mobile` the column."""
        f = self.cells_dir / f"layout_{layout}" / "agent.yaml"
        return dict((yaml.safe_load(oslayer.read_text(f)) or {}).get("rects") or {}) if f.exists() else {}

    def seed_host_agent(self, parent_path: str, rec: dict) -> None:
        """Write a HOST agent record under `parent_path` (e.g. `FOLDER`, the
        root view's backend folder) — loaded at the next boot."""
        d = self.proj / ".fantastic" / parent_path / "agents" / rec["id"]
        d.mkdir(parents=True, exist_ok=True)
        (d / "agent.yaml").write_text(yaml.safe_dump(rec, sort_keys=False))

    def patch_host_agent(self, agent_id: str, fields: dict) -> None:
        """Merge `fields` into a HOST agent's record on disk (host stopped)."""
        for f in (self.proj / ".fantastic").rglob(f"{agent_id}/agent.yaml"):
            rec = yaml.safe_load(oslayer.read_text(f)) or {}
            rec.update(fields)
            f.write_text(yaml.safe_dump(rec, sort_keys=False))
            return
        raise SystemExit(f"no host record {agent_id} to patch")

    def cells(self) -> list[dict]:
        out = []
        for f in sorted(self.cells_dir.glob("*/agent.yaml")):
            # a cell deleted between the listing and the read is no longer a cell
            try:
                r = yaml.safe_load(oslayer.read_text(f)) or {}
            except FileNotFoundError:
                continue
            if r.get("handler_module") not in ("background.ts", "layout.ts"):
                out.append(r)
        return out

    def page_guid(self) -> str:
        f = self.proj / ".fantastic/web/agents/canvas/agent.yaml"
        return (yaml.safe_load(oslayer.read_text(f)) or {}).get("guid", "") if f.exists() else ""

    def host_record(self, agent_id: str) -> dict | None:
        for f in (self.proj / ".fantastic").rglob(f"{agent_id}/agent.yaml"):
            try:
                return yaml.safe_load(oslayer.read_text(f)) or {}
            except FileNotFoundError:
                continue  # deleted between the listing and the read
        return None

    def log_at(self) -> str:
        """Where the host's log stands: its newest `[n]` line."""
        try:
            lines = self.log.read_text(errors="replace").splitlines()
        except OSError:
            return ""
        return next((ln[:80] for ln in reversed(lines) if ln.startswith("[")), "")

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
    def _launch_chrome(self, width: int, height: int, extra: list[str] | None) -> Chrome:
        cdp = free_port()
        # CANVAS_HEADFUL=1: a visible Chrome on the real GPU (a load test's frame
        # rate is the GPU's, not the software rasterizer's)
        # headless: software WebGL (SwiftShader) — the terminals draw with WebGL
        mode = [] if os.environ.get("CANVAS_HEADFUL") == "1" else ["--headless=new", "--use-angle=swiftshader", "--enable-unsafe-swiftshader"]
        return Chrome([os.environ.get("CHROME") or MAC_CHROME, *mode, DESKTOP_POINTER, f"--window-size={width},{height}", f"--remote-debugging-port={cdp}",
                       f"--user-data-dir={self.art / 'chrome'}", "--no-first-run", "--password-store=basic",
                       "--autoplay-policy=no-user-gesture-required", *(extra or []), "about:blank"], self.art / f"chrome-{cdp}.log", cdp)

    def _drop_warm(self) -> None:
        """Close the Chrome `init()` left up, if nothing took it over."""
        w, self._warm = self._warm, None
        if w is not None:
            w.proc.terminate()

    @asynccontextmanager
    async def page(self, width: int = 1500, height: int = 950, extra: list[str] | None = None):
        # The Chrome `init()` left up serves the FIRST page opened without extra flags, its
        # window put at this page's size (what `--window-size` does at launch); a page with
        # extra flags closes it first (one Chrome per user-data-dir) and launches its own.
        warm, self._warm = self._warm, None
        if warm is not None and extra:
            warm.proc.terminate()
            warm.proc.wait()
            warm = None
        chrome = warm or self._launch_chrome(width, height, extra)
        try:
            async with self._session(chrome, width, height, resize=warm is not None) as pg:
                yield pg
        finally:
            chrome.proc.terminate()

    @asynccontextmanager
    async def _session(self, chrome: Chrome, width: int, height: int, resize: bool = False):
        """One CDP session on `chrome`'s tab: the canvas open and drawn. Writes the page's final DOM and errors."""
        async with await attach(chrome) as ws:
            pg = Page(ws)
            pg.rig = self
            pg.mark = self.log_at
            self.misses = pg.misses
            await pg.cdp("Runtime.enable")
            await pg.cdp("Page.enable")
            if resize:
                win = await pg.cdp("Browser.getWindowForTarget")
                await pg.cdp("Browser.setWindowBounds", windowId=win["windowId"], bounds={"width": width, "height": height})
                seen = None
                for _ in range(30):  # until the window has settled at its size
                    now = await pg.js("[innerWidth, innerHeight]")
                    if now == seen:
                        break
                    seen = now
                    await asyncio.sleep(0.1)
            await pg.viewport(width, height)
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
        self._drop_warm()
        self.summary.update({"ok": ok, "note": note, "page_calls": self.page_calls()})
        (self.art / "summary.json").write_text(json.dumps(self.summary, indent=1, ensure_ascii=False))
        if not ok and self.misses:
            # every page wait that ran out, when, and where the host's log stood
            print("waits that ran out:\n  " + "\n  ".join(self.misses[-20:]))
        if not ok and self.log.exists():
            # the run's own log: a CI run keeps only what is printed
            print("host.log (tail):\n" + "\n".join(self.log.read_text(errors="replace").splitlines()[-40:]))
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


async def open_tree(pg: Page, x: int, y: int) -> str | None:
    """Place a FILE TREE cell (a `files_view.ts` cell) at empty canvas (x, y) —
    nothing floats: it is a cell from the double-click. Returns the cell's id once
    the project's folder is listed (`.fantastic` shows), else None."""
    frames = "[...document.querySelectorAll('.agent-frame')].filter(f=>f.querySelector('.files-view'))"
    before = await pg.js(f"{frames}.length")
    await spot(pg, x, y, "file tree")
    if not await pg.until(f"{frames}.length > {before}", 10):
        return None
    # its root is listed from the host (async): ready once `.fantastic` shows
    if not await pg.until("!!document.querySelector('.files-view .ft-row[data-path=\".fantastic\"]')", 15):
        return None
    return await pg.js(f"{frames}.pop().dataset.id")


# THE CHAT is the html view its agent's `reflect` returns, drawn in the cell's panel
# iframe (`fantastic-ai-core/src/chat_view.html`): the dialog `#dialog` (`.msg` rows,
# assistant ones `.assistant`), the input `#text`, `#send`, and the phase pill `#pill`
# (`data-phase`, `data-reason`). `cell` is the chat's frame by order on the page.
CHAT_DOC = "[...document.querySelectorAll('.agent-frame iframe')].map(f=>{try{return f.contentDocument}catch(e){return null}}).filter(d=>d&&d.getElementById('dialog'))"


async def say(pg: Page, text: str, cell: int = 0) -> None:
    """Send a turn in chat cell #cell: type into its input and press Send."""
    await pg.js(f"(()=>{{const d={CHAT_DOC}[{cell}]; const i=d.getElementById('text');"
                f" i.value={json.dumps(text)}; i.dispatchEvent(new Event('input',{{bubbles:true}})); d.getElementById('send').click();}})()")


async def wait_quiet(pg: Page, total: float, quiet: float, cell: int = 0) -> bool:
    """Until chat cell #cell's chain is OVER — its pill says idle with a reason that
    names no work still awaited — or, when that never arrives, until it has answered
    in words and gone QUIET `quiet` s."""
    deadline = time.time() + total
    last_n, since = -1, time.time()
    seen_live = False
    ended = 0.0
    doc = f"{CHAT_DOC}[{cell}]"
    while time.time() < deadline:
        n = await pg.js(f"({doc}?.querySelectorAll('#dialog .msg').length) ?? 0") or 0
        said = await pg.js(f"[...({doc}?.querySelectorAll('#dialog .msg.assistant') ?? [])].some(m=>m.textContent.trim())")
        phase = await pg.js(f"{doc}?.getElementById('pill')?.dataset.phase || ''")
        thinking = phase in ("thinking", "streaming", "tool_calling", "queued")
        if thinking:
            seen_live = True
        reason = await pg.js(f"{doc}?.getElementById('pill')?.dataset.reason || ''")
        if seen_live and said and not thinking and reason not in ("", "awaiting"):
            ended = ended or time.time()
            if time.time() - ended >= ENDED_HOLD_S:
                return True
        else:
            ended = 0.0
        if n != last_n or thinking:
            last_n, since = n, time.time()
        elif said and time.time() - since >= quiet:
            return True
        await asyncio.sleep(0.5)
    return False


async def answer(pg: Page, cell: int = 0) -> str:
    """The AI's last WORDS: its last assistant message with text, skipping one that
    is only call markup (a malformed call shows as text but is no answer)."""
    return await pg.js(f"(()=>{{const m=[...({CHAT_DOC}[{cell}]?.querySelectorAll('#dialog .msg.assistant') ?? [])]"
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


#: The verbs an AI's cell may call on the page's agents (the host's egress gate checks
#: every agent a call names): the view's control plane, its cells' own verbs, the
#: backgrounds', the skin's read, the file tree's and the previewer's.
VIEW_VERBS = ["place", "move", "fit", "update", "wire", "remove", "hide", "show", "open", "terminal_renderer", "reflect"]
CELL_VERBS = ["reflect", "get_html", "set_html", "emit"]
BG_VERBS = ["set_bg", "default_bg", "clear_bg", "get_bg", "reflect"]
BACKGROUNDS_VERBS = ["list", "set", "random", "none", "default", "delete", "restore_stock", "add", "rotate"]
SKIN_VERBS = ["get", "schema"]
FILES_VERBS = ["select", "expand", "collapse", "reveal"]
PREVIEW_VERBS = ["open", "pin", "unpin", "mode", "edit", "save", "revert"]


def peer_grants(rig: "Rig") -> dict:
    """The page reach an AI of the canvas holds: what its cell's view lets it call."""
    g = rig.page_guid()
    out = {
        f"{g}|view_root": VIEW_VERBS,
        f"{g}|skin": SKIN_VERBS,
        f"{g}|*": sorted({*CELL_VERBS, *VIEW_VERBS, *BACKGROUNDS_VERBS, *SKIN_VERBS, *FILES_VERBS, *PREVIEW_VERBS}),
    }
    for f in rig.cells_dir.glob("*/agent.yaml"):
        r = yaml.safe_load(oslayer.read_text(f)) or {}
        if r.get("handler_module") == "background.ts":
            out[f"{g}|{r['id']}"] = BG_VERBS
    return out


async def open_chat(pg: Page, x: int, y: int) -> str:
    """Make the canvas's AI and show it: the host spawns the AI from the saved set-up
    (`canvas_root spawn`, with the canvas's reach), the root view places a cell that
    shows it at (x, y) (`place {backend_id}`), and the cell draws the html chat view
    its `reflect` returns. Returns the AI's id once the view is up."""
    from canvas import host_call

    rig = pg.rig
    made = await host_call(rig.port, "canvas_root", {"type": "spawn", "peer_grants": peer_grants(rig)})
    if "id" not in made:
        raise SystemExit(f"open_chat: the AI was not made: {made}")
    placed = await host_call(rig.port, f"{rig.page_guid()}|view_root",
                             {"type": "place", "backend_id": made["id"], "x": x, "y": y, "width": 440, "height": 520})
    if "placed" not in placed:
        raise SystemExit(f"open_chat: the cell was not placed: {placed}")
    if not await pg.until(f"{CHAT_DOC}.length > 0", 30):
        raise SystemExit("open_chat: the chat view never drew")
    return str(made["id"])


async def ask(pg: Page, x: int, y: int, text: str, total: float, quiet: float) -> tuple[bool, str]:
    """Open the canvas's AI at (x, y) (`open_chat`) and send it `text`. Returns
    `(done, its last words)`."""
    await open_chat(pg, x, y)
    await say(pg, text)
    done = await wait_quiet(pg, total, quiet)
    return done, await answer(pg)


def html_cell(cid: str, name: str, x: int, y: int, html: str, w: int = 340, h: int = 240, **extra: Any) -> dict:
    """A trusted html panel record, as a ported project holds one."""
    return {"id": cid, "handler_module": "html_agent.ts", "display_name": name, "trusted": True,
            "html": html, "x": x, "y": y, "width": w, "height": h, **extra}


def probe_main(fn) -> None:
    """Run a probe's async main and exit with its verdict."""
    ok = fn()
    sys.exit(0 if ok else 1)
