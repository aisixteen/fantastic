import { Host } from "../../lib.ts";
import type { Json, Payload } from "../../lib.ts";
import { readShell } from "../shell.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { HOME_CSS, HOME_STYLE_ID } from "./home_css.ts";
import { cardVerbs, colourVar, decodeState, label, lastSeen, shortcut, stateCard, verbLabel } from "./home_model.ts";
import type { HomeRow, HomeState } from "./home_model.ts";
import { FramePool, plan } from "./pages.ts";

// THE HOME VIEW — the screen of the host's `home` agent (`home.tools`). It
// renders the agent's `state` and sends its intents as `home` verbs, and does
// nothing else: NO LOGIC IN VIEWS. It keeps exactly three things — the last
// decoded state, the canvas-frame pool and an open log dialog — and never changes
// pages, selection, rows or problem itself: every click and key is an intent,
// and the screen changes only when a `state` arrives. Two tabs show one Home.
//
//   #home
//   ├── .carousel            the Home, then one .page per open page; slid to the selection
//   │   ├── .page.page-home  .home-card: Open Folder (the shell's) or a path field, YOUR FANTASTICS
//   │   └── .page[data-id]   the canvas frame while it runs, else its state card; .controls
//   ├── nav.nav-pill         the Home, then each page (a dot; the shown one named)
//   ├── .status-capsule      the shown page's status, name, address and pid
//   ├── .problem             the home's problem, with Dismiss
//   └── .notice              a refused intent, for 6 s

/** Every verb the view sends `home` (and its watch). `focus` is the host shell's. */
export const HOME_VERBS = ["list", "open", "register", "start", "stop", "remove", "select", "select_page", "close_page", "dismiss", "log", "watch"] as const;

/** How long a refused intent's notice shows. */
const NOTICE_MS = 6000;

type Rec = Record<string, unknown>;

/** A shell's hook into the Home: `open` sends `open {paths}` (a native drop, its own menu). */
interface HomeHook {
  open(paths: string[]): Promise<boolean>;
}

function ensureStyle(): void {
  if (document.getElementById(HOME_STYLE_ID)) return;
  const s = document.createElement("style");
  s.id = HOME_STYLE_ID;
  s.textContent = HOME_CSS;
  document.head.appendChild(s);
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: (Node | string)[]): HTMLElementTagNameMap[K] {
  const e = document.createElement(tag);
  Object.assign(e, props);
  for (const k of kids) e.append(k);
  return e;
}

/** A status dot, coloured by its status's custom property. */
function dot(row: HomeRow | undefined): HTMLElement {
  const d = el("span", { className: "dot" });
  if (row !== undefined) d.style.setProperty("--dot", `var(${colourVar(row.status)})`);
  return d;
}

export const homeView: ViewBundle = {
  handles: ["home_view.ts"],
  grants: (backend) => ({ [backend]: HOME_VERBS }),
  async mount(ctx: ViewContext): Promise<ViewHandle> {
    ensureStyle();
    const host = new Host(ctx.kernel, ctx.selfId);
    const call = (verb: string, args: Rec = {}): Promise<Json> => host.call(ctx.backend, { type: verb, ...args } as Payload);
    const shell = readShell();

    // ── what the view keeps ──
    let shown: HomeState | null = null;
    const pool = new FramePool();
    let logDialog: HTMLDialogElement | null = null;
    let broken = false;

    // ── the skeleton (made once; each state redraws its parts) ──
    const root = el("div", { id: "home" });
    const carousel = el("div", { className: "carousel" });
    const homePage = el("section", { className: "page page-home" });
    homePage.style.setProperty("--i", "0");
    const rowsBox = el("div", { className: "home-rows" });
    const emptyLine = el("div", { className: "home-empty" });
    const nav = el("nav", { className: "nav-pill" });
    const capsule = el("div", { className: "status-capsule" });
    const problem = el("div", { className: "problem" });
    const notice = el("div", { className: "notice" });
    capsule.hidden = problem.hidden = notice.hidden = true;
    carousel.append(homePage);
    root.append(carousel, nav, capsule, problem, notice);
    ctx.mount.append(root);

    // ── intents ──
    let noticeTimer: ReturnType<typeof setTimeout> | null = null;
    const refusedNotice = (r: Rec): void => {
      const hint = typeof r["hint"] === "string" && r["hint"] !== "" ? ` — ${r["hint"]}` : "";
      notice.replaceChildren(el("span", { className: "text" }, `${String(r["error"])}${hint}`));
      notice.hidden = false;
      if (noticeTimer !== null) clearTimeout(noticeTimer);
      noticeTimer = setTimeout(() => {
        notice.hidden = true;
        noticeTimer = null;
      }, NOTICE_MS);
    };
    /** Send `verb`: a refusal is a notice; any other reply is a state. True when it was not refused. */
    const intent = async (verb: string, args: Rec = {}): Promise<boolean> => {
      const r = (await call(verb, args)) as Rec | null;
      if (r !== null && typeof r === "object" && typeof r["error"] === "string") {
        refusedNotice(r);
        return false;
      }
      show(r);
      return true;
    };
    /** A button that sends `verb {id}` and goes no further (a row's click is `open`). */
    const verbButton = (verb: string, id: string, text: string, title = ""): HTMLButtonElement => {
      const b = el("button", { type: "button", textContent: text, title });
      b.dataset["intent"] = verb;
      b.addEventListener("click", (e) => {
        e.stopPropagation();
        void intent(verb, { id });
      });
      return b;
    };

    // ── the Home card (its static parts) ──
    const card = el("div", { className: "home-card" }, el("h1", {}, "Fantastic"), el("p", { className: "lead" }, "A live canvas for every folder"));
    if (shell?.pickFolder !== undefined) {
      const pick = shell.pickFolder;
      const b = el("button", { type: "button", className: "open-folder", textContent: "Open Folder…" });
      b.addEventListener("click", async () => {
        const paths = await pick();
        if (paths.length > 0) await intent("open", { paths });
      });
      card.append(b, el("p", { className: "open-hint" }, "or drop a folder anywhere"));
    } else {
      const input = el("input", { name: "path", type: "text", placeholder: "/path/to/a/folder", autocomplete: "off", spellcheck: false });
      const form = el("form", { className: "open-path" }, input, el("button", { type: "submit", textContent: "Open" }));
      form.addEventListener("submit", async (e) => {
        e.preventDefault();
        const value = input.value.trim();
        if (value === "") return;
        if (await intent("open", { paths: [value] })) input.value = "";
      });
      card.append(form);
    }
    card.append(el("h2", {}, "YOUR FANTASTICS"), emptyLine, rowsBox);
    homePage.append(card);

    // ── drawing a state ──
    const drawRows = (s: HomeState): void => {
      emptyLine.textContent = !s.listed ? "Looking for your fantastics…" : s.rows.length === 0 ? "No fantastics yet — open a folder." : "";
      emptyLine.hidden = emptyLine.textContent === "";
      const now = Date.now();
      rowsBox.replaceChildren(
        ...s.rows.map((row) => {
          const status = el("span", { className: "status" }, dot(row), label(row.status));
          status.dataset["status"] = row.status;
          const meta = el("div", { className: "meta" }, status);
          if (row.last_seen !== null && row.last_seen > 0 && row.status === "stopped") {
            meta.append(el("span", { className: "last-seen" }, lastSeen(row.last_seen, now)));
          }
          const acts = el("div", { className: "acts" }, ...row.actions.filter((a) => a !== "open").map((a) => verbButton(a, row.id, verbLabel(a))));
          const line = el(
            "div",
            { className: "home-row" },
            el("div", { className: "who" }, el("span", { className: "name" }, row.name), el("span", { className: "path", title: row.path }, row.path)),
            meta,
            acts,
          );
          line.dataset["id"] = row.id;
          line.addEventListener("click", () => void intent("open", { paths: [row.path] }));
          return line;
        }),
      );
    };

    const pageEls = new Map<string, { page: HTMLElement; slot: HTMLElement; body: HTMLElement; controls: HTMLElement }>();
    /** One .page per open page: made when it opens (appended), removed when it
     *  closes, and NEVER moved — moving its frame would reload it. Where a page
     *  shows is its `--i`; the agent only appends and removes pages. */
    const syncPages = (s: HomeState): void => {
      for (const [id, p] of pageEls) {
        if (!s.pages.includes(id)) {
          p.page.remove();
          pageEls.delete(id);
        }
      }
      s.pages.forEach((id, i) => {
        let p = pageEls.get(id);
        if (p === undefined) {
          const slot = el("div", { className: "frame-slot" });
          const body = el("div", { className: "page-body" });
          const controls = el("div", { className: "controls" });
          const page = el("section", { className: "page" }, slot, body, controls);
          page.dataset["id"] = id;
          carousel.append(page);
          p = { page, slot, body, controls };
          pageEls.set(id, p);
        }
        p.page.style.setProperty("--i", String(i + 1));
      });
    };

    const drawPage = (s: HomeState, id: string): void => {
      const p = pageEls.get(id)!;
      const row = s.rows.find((r) => r.id === id);
      const running = row?.status === "running" && row.canvas !== null;
      p.slot.hidden = !running;
      const controls: HTMLElement[] = [];
      if (row !== undefined && row.actions.includes("stop") && (row.status === "running" || row.status === "starting")) {
        controls.push(verbButton("stop", id, "Stop"));
      }
      if (running) {
        const reload = el("button", { type: "button", className: "reload", textContent: "Reload", title: "Reload this canvas" });
        reload.addEventListener("click", () => {
          const f = pool.frame(id);
          if (f !== undefined) f.src = f.src; // its own frame only: no home state changes
        });
        const tab = el("a", { className: "open-tab", href: row.canvas!.url, target: "_blank", rel: "noopener", textContent: "Open in browser" });
        controls.push(reload, tab);
      }
      controls.push(verbButton("close_page", id, "Close", "Close — it keeps running"));
      p.controls.replaceChildren(...controls);
      if (running) {
        p.body.replaceChildren();
        return;
      }
      const c = row === undefined ? { title: "Not in the list", detail: id } : (stateCard(row) ?? { title: row.name, detail: row.path });
      const box = el("div", { className: "state-card" }, dot(row), el("h2", {}, c.title), el("p", {}, c.detail));
      box.dataset["status"] = row?.status ?? "missing";
      const acts = el("div", { className: "acts" }, ...(row === undefined ? [] : cardVerbs(row).map((v) => verbButton(v, id, verbLabel(v, true)))));
      if (row !== undefined && row.log !== null && (row.status === "stopped" || row.status === "unverified")) {
        const b = el("button", { type: "button", className: "show-log", textContent: "Show log" });
        b.addEventListener("click", () => void showLog(id));
        acts.append(b);
      }
      box.append(acts);
      p.body.replaceChildren(box);
    };

    const drawNav = (s: HomeState): void => {
      const home = el("button", { type: "button", className: "nav-home", textContent: "Home", title: "Home" });
      if (s.selection === null) home.setAttribute("aria-current", "page");
      home.addEventListener("click", () => void intent("select", { id: null }));
      const pages = s.pages.map((id) => {
        const row = s.rows.find((r) => r.id === id);
        const b = el("button", { type: "button", className: "nav-page", title: row?.name ?? id }, dot(row));
        b.dataset["id"] = id;
        if (s.selection === id) {
          b.setAttribute("aria-current", "page");
          b.append(el("span", { className: "name" }, row?.name ?? id));
        }
        const x = el("span", { className: "x", textContent: "×", title: "Close — it keeps running" });
        x.addEventListener("click", (e) => {
          e.stopPropagation();
          void intent("close_page", { id });
        });
        b.append(x);
        b.addEventListener("click", () => void intent("select", { id }));
        b.addEventListener("contextmenu", (e) => {
          e.preventDefault();
          void intent("close_page", { id });
        });
        return b;
      });
      nav.replaceChildren(home, ...pages);
    };

    const drawCapsule = (s: HomeState): void => {
      const row = s.selection === null ? undefined : s.rows.find((r) => r.id === s.selection);
      capsule.hidden = row === undefined;
      if (row === undefined) return;
      const parts: HTMLElement[] = [dot(row), el("span", { className: "label" }, label(row.status)), el("span", { className: "name" }, row.name)];
      if (row.status === "running" && row.canvas !== null) parts.push(el("span", { className: "addr" }, `127.0.0.1:${row.canvas.port}`));
      if (row.pid !== null) parts.push(el("span", { className: "pid" }, `pid ${row.pid}`));
      capsule.replaceChildren(...parts);
      capsule.dataset["status"] = row.status;
    };

    const drawProblem = (s: HomeState): void => {
      problem.hidden = s.problem === null;
      if (s.problem === null) return problem.replaceChildren();
      const b = el("button", { type: "button", textContent: "Dismiss" });
      b.dataset["intent"] = "dismiss";
      b.addEventListener("click", () => void intent("dismiss"));
      problem.replaceChildren(el("span", { className: "text" }, s.problem), b);
    };

    const fatal = (why: string): void => {
      broken = true;
      pool.clear();
      console.error(`[home] ${why}`);
      root.replaceChildren(el("p", { className: "home-error" }, why));
    };

    /** A reply or an event: decoded, an older one dropped, then drawn. */
    function show(v: unknown): void {
      if (broken) return;
      const s = decodeState(v);
      if (typeof s === "string") return fatal(`The home sent a state this page cannot read — ${s}.`);
      if (shown !== null && s.seq <= shown.seq) return;
      shown = s;
      drawRows(s);
      const p = plan(pool.held(), s);
      pool.apply({ keep: [], create: [], drop: p.drop }, () => root); // a closed page's frame goes before its page does
      syncPages(s);
      pool.apply({ ...p, drop: [] }, (id) => pageEls.get(id)!.slot);
      for (const id of s.pages) drawPage(s, id);
      drawNav(s);
      drawCapsule(s);
      drawProblem(s);
      const at = s.selection === null ? 0 : s.pages.indexOf(s.selection) + 1;
      const first = carousel.style.transform === "";
      // the first state places the carousel; only a change of page slides it
      if (first) carousel.style.transition = "none";
      carousel.style.transform = `translateX(${-Math.max(0, at) * 100}%)`;
      if (first) {
        void carousel.offsetWidth; // the position lands before the transition returns
        carousel.style.transition = "";
      }
    }

    // ── the launch log ──
    async function showLog(id: string): Promise<void> {
      const r = (await call("log", { id })) as Rec | null;
      if (r === null || typeof r !== "object" || typeof r["error"] === "string") {
        refusedNotice(r ?? { error: "home: log answered nothing" });
        return;
      }
      logDialog?.remove();
      const close = el("button", { type: "button", textContent: "Close" });
      const d = el("dialog", { className: "log" }, el("header", {}, el("span", {}, String(r["file"] ?? "")), close), el("pre", {}, String(r["tail"] ?? "")));
      close.addEventListener("click", () => d.close());
      d.addEventListener("close", () => {
        d.remove();
        if (logDialog === d) logDialog = null;
      });
      logDialog = d;
      root.append(d);
      d.showModal();
    }

    // ── keys and the shell's hook ──
    const isMac = /Mac/.test((navigator as { userAgentData?: { platform?: string } }).userAgentData?.platform ?? navigator.platform);
    const onKey = (e: KeyboardEvent): void => {
      if (shown === null || broken) return;
      if (e.key === "Escape" && shown.problem !== null && logDialog === null) {
        e.preventDefault();
        void intent("dismiss");
        return;
      }
      const it = shortcut(e, isMac, shown);
      if (it === null) return;
      e.preventDefault();
      void intent(it.verb, it.args);
    };
    window.addEventListener("keydown", onKey);
    const hook: HomeHook = { open: (paths) => intent("open", { paths }) };
    (window as { fantasticHome?: HomeHook }).fantasticHome = hook;

    // ── the stream: watched first, listed second ──
    const off = host.on(ctx.backend, "state", (s) => show(s));
    const first = (await call("list")) as Rec | null;
    if (first !== null && typeof first === "object" && typeof first["error"] === "string") fatal(`The home refused to list — ${first["error"]}.`);
    else show(first);

    return {
      unmount(): void {
        off();
        host.unwatch(ctx.backend);
        window.removeEventListener("keydown", onKey);
        if ((window as { fantasticHome?: HomeHook }).fantasticHome === hook) delete (window as { fantasticHome?: HomeHook }).fantasticHome;
        if (noticeTimer !== null) clearTimeout(noticeTimer);
        logDialog?.remove();
        pool.clear();
        root.remove();
      },
    };
  },
};
