import { Agent, Host } from "../../lib.ts";
import type { Kernel, Payload } from "../../lib.ts";
import { CANVAS_CSS } from "./styles.ts";
import type { ViewBundle, ViewHandle } from "../view.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { aiView } from "../ai_view/ai_view.ts";
import { openSpot } from "./spot_input.ts";
import type { Pick, Presets } from "./spot_input.ts";
import { htmlView } from "../html_agent/html_view.ts";
import { createGlHost } from "./gl_host.ts";
import type { GlHost, GlView } from "./gl_host.ts";

// The canvas view-AGENT: the DOM-root compositor. Owns document.body, renders
// host member agents either INLINE (a TS view-agent for first-party content,
// keyed by the member's handler_module) or as an IFRAME (untrusted/external
// content that answers get_webapp). Translates pan/zoom/drag/dblclick into
// kernel sends. The kernel knows none of this.

// inline view registry — host handler_module → TS view bundle
const VIEW_BUNDLES: readonly ViewBundle[] = [terminalView, aiView, htmlView];
function viewFor(handlerModule: unknown): ViewBundle | undefined {
  if (typeof handlerModule !== "string") return undefined;
  return VIEW_BUNDLES.find((b) => b.handles.includes(handlerModule));
}

// THE VIEW A DOUBLE CLICK SPAWNS: a terminal. What it spawns on the host is the
// view's own declaration (`fronts`), looked up by that exact name in the host
// catalog — never matched, never guessed.
const DEFAULT_VIEW = terminalView;

export interface MountOptions {
  kernel: Kernel;
  mount: HTMLElement;
  selfId: string;
  /** The host socket LEG this page speaks through, in the host's own words
   *  (its loader's parent). Every backend the canvas spawns for a view is the
   *  leg's CHILD — its own subtree, both gates covered by the born-with
   *  baseline. Absent ⇒ the canvas spawns nothing, and says so. */
  leg?: string;
  /** Land every pending record through the loader NOW — a view is on disk
   *  before its backend exists, so no page ever sees a backend without one. */
  flush: () => Promise<void>;
}

interface Rec {
  id: string;
  [key: string]: unknown;
}
interface Webapp {
  url: string;
  default_width?: number;
  default_height?: number;
  title?: string;
}
interface Frame {
  el: HTMLElement;
  rec: Rec;
  kind: "inline" | "iframe";
  handle?: ViewHandle; // inline only
}

const num = (r: Rec, k: string): number | undefined =>
  typeof r[k] === "number" ? (r[k] as number) : undefined;
const str = (r: Rec, k: string): string | undefined =>
  typeof r[k] === "string" ? (r[k] as string) : undefined;
const bool = (r: Rec, k: string): boolean => r[k] === true;

export async function mountCanvas(opts: MountOptions): Promise<void> {
  const { kernel, mount, selfId, leg, flush } = opts;
  const host = new Host(kernel, selfId);

  // THE PAIR: a view the canvas spawned a backend for, and that backend. The
  // backend is a child of the leg carrying its view's id and this canvas's
  // (`view_id`, `canvas_id`); the view carries the backend's (`backend_id`,
  // `owns_backend`) and the grant to call it (`can_call`). The leg lives its
  // own life — a page connecting, leaving or reloading never touches the
  // backends behind it; only closing the view deletes its backend.

  // The host catalog, paged to the end, for ONE exact handler_module.
  const inCatalog = async (module: string): Promise<boolean> => {
    let cursor: unknown = "all";
    while (cursor !== undefined && cursor !== null) {
      const cat = (await host.callHost("kernel", { type: "reflect", tree: "none", bundles: cursor } as unknown as Payload)) as {
        bundles?: Array<{ handler_module?: string }>;
        bundles_next?: unknown;
      };
      if ((cat?.bundles ?? []).some((b) => b.handler_module === module)) return true;
      cursor = cat?.bundles_next;
    }
    return false;
  };

  // Spawn the backend `bundle` fronts, for view `viewId`, as a child of the leg.
  // Returns its id, or null (said why).
  const spawnBackend = async (bundle: ViewBundle, viewId: string): Promise<string | null> => {
    if (leg === undefined) {
      console.error("[canvas] the host loader names no parent leg — cannot spawn a backend");
      return null;
    }
    const module = bundle.fronts;
    if (module === undefined) {
      console.error(`[canvas] ${bundle.handles.join(",")} declares no backend it fronts — cannot spawn one`);
      return null;
    }
    if (!(await inCatalog(module))) {
      console.error(`[canvas] the host does not link ${module}`);
      return null;
    }
    const created = (await host.callHost("kernel", {
      ...(bundle.backendFields ?? {}),
      type: "create_agent",
      parent_id: leg,
      handler_module: module,
      view_id: viewId,
      canvas_id: selfId,
    } as unknown as Payload)) as { id?: string; error?: string };
    if (!created || created.error || typeof created.id !== "string") {
      console.error("[canvas] backend create failed:", JSON.stringify(created));
      return null;
    }
    return created.id;
  };

  // BIND a spawned backend to its view: the id, and the view's own grant.
  const bind = (viewId: string, bundle: ViewBundle, backendId: string): void => {
    kernel.updateMeta(viewId, {
      backend_id: backendId,
      backend_lost: false,
      can_call: { ...bundle.grants(backendId) },
    } as unknown as Payload);
  };

  // Every agent on the host, paged to the end.
  const hostAgents = async (): Promise<Array<Record<string, unknown>>> => {
    const out: Array<Record<string, unknown>> = [];
    let after: unknown = null;
    for (;;) {
      const page = (await host.callHost("kernel", { type: "list_agents", limit: 200, after } as unknown as Payload)) as {
        agents?: Array<Record<string, unknown>>;
        next?: unknown;
      };
      out.push(...(page?.agents ?? []));
      if (page?.next === null || page?.next === undefined) return out;
      after = page.next;
    }
  };

  // RECONCILE THIS CANVAS'S PAIRS, once, before the first render. Nothing on
  // the host enforces the pairing, so the one party that sees both sides keeps
  // it by rule — and only for backends carrying ITS canvas id:
  //   · a backend whose view is gone, or whose view is bound to another
  //     backend, is DELETED (a crash, or a close while the host was down);
  //   · a backend whose view exists but is not bound yet is LEFT (a spawn in
  //     flight in another tab — the view lands on disk before its backend);
  //   · a spawned view whose backend is gone is marked `backend_lost` and waits
  //     for ⟳ — never respawned silently.
  const reconcile = async (): Promise<void> => {
    if (leg === undefined) return;
    const backends = new Map<string, string>(); // backend id → its view id
    for (const a of await hostAgents()) {
      if (a["parent_id"] === leg && a["canvas_id"] === selfId && typeof a["id"] === "string" && typeof a["view_id"] === "string") {
        backends.set(a["id"], a["view_id"]);
      }
    }
    const root = kernel.rootId !== null ? kernel.get(kernel.rootId) : undefined;
    const views = new Map<string, Rec>(
      root !== undefined ? [...root.children.values()].map((a) => [a.id, a.record() as Rec]) : [],
    );
    for (const [bid, vid] of backends) {
      const v = views.get(vid);
      // an AI cell's backend is a SUBTREE (its store, the AI under it): the view
      // names its root in `backend_root`
      const bound = v === undefined ? undefined : (str(v, "backend_root") ?? str(v, "backend_id"));
      if (v === undefined || (bound !== undefined && bound !== bid)) {
        await host.callHost("kernel", { type: "delete_agent", id: bid });
      }
    }
    for (const v of views.values()) {
      const bid = str(v, "backend_root") ?? str(v, "backend_id");
      if (bool(v, "owns_backend") && bid !== undefined && !backends.has(bid) && !bool(v, "backend_lost")) {
        kernel.updateMeta(v.id, { backend_lost: true });
      }
    }
  };

  // ─── DOM scaffold ───────────────────────────────────────────────
  const style = document.createElement("style");
  style.textContent = CANVAS_CSS;
  document.head.appendChild(style);

  const bg = document.createElement("canvas");
  bg.id = "bg";
  mount.appendChild(bg);

  const canvas = document.createElement("div");
  canvas.id = "canvas";
  const world = document.createElement("div");
  world.className = "canvas-world";
  canvas.appendChild(world);
  const toolbar = document.createElement("div");
  toolbar.id = "toolbar";
  const status = document.createElement("span");
  status.textContent = "…";
  toolbar.appendChild(status);
  // The AI set-up page, beside the canvas (same host, same socket leg).
  const setupLink = document.createElement("a");
  setupLink.href = "setup.html";
  setupLink.target = "_blank";
  setupLink.textContent = "⚙ AI set-up";
  setupLink.className = "setup-link";
  toolbar.appendChild(setupLink);
  mount.appendChild(canvas);
  mount.appendChild(toolbar);

  // ─── view: zoom + pan (DOM transform on .canvas-world; GL camera locked) ──
  const view = { z: 1, ox: 0, oy: 0 };
  let targetZ = view.z;
  const ZOOM_MIN = 0.1;
  const ZOOM_MAX = 5;
  const ZOOM_LERP = 0.72;
  let glHost: GlHost | null = null;
  const applyView = (): void => {
    world.style.transform = `translate(${view.ox}px, ${view.oy}px) scale(${view.z})`;
    glHost?.applyCamera();
  };
  applyView();

  // The GL host (shared THREE renderer/scene) is created lazily on the first
  // GL member, so a canvas with no GL views never loads three.
  const ensureGlHost = async (): Promise<GlHost> => {
    if (glHost === null) {
      glHost = await createGlHost({
        bg,
        view,
        host,
        kernel,
        selfId,
        fetchSource: async (aid) =>
          (await host.call(aid, { type: "get_gl_view" }).catch(() => null)) as GlView | null,
      });
    }
    return glHost;
  };
  window.addEventListener("resize", () => {
    glHost?.resize();
    applyView();
  });

  const tick = (): void => {
    if (view.z !== targetZ) {
      const cx = window.innerWidth / 2;
      const cy = window.innerHeight / 2;
      let nextZ = view.z + (targetZ - view.z) * ZOOM_LERP;
      if (Math.abs(targetZ - nextZ) < 0.0005) nextZ = targetZ;
      const ratio = nextZ / view.z;
      view.ox = cx - (cx - view.ox) * ratio;
      view.oy = cy - (cy - view.oy) * ratio;
      view.z = nextZ;
      applyView();
    }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);

  canvas.addEventListener(
    "wheel",
    (e) => {
      if ((e.target as Element).closest(".agent-frame")) return;
      e.preventDefault();
      const factor = e.deltaY > 0 ? 0.95 : 1.05;
      targetZ = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, targetZ * factor));
    },
    { passive: false },
  );

  canvas.addEventListener("mousedown", (e) => {
    if ((e.target as Element).closest(".agent-frame")) return;
    if (e.button !== 0 && e.button !== 1) return;
    e.preventDefault();
    const sx = e.clientX;
    const sy = e.clientY;
    const ox0 = view.ox;
    const oy0 = view.oy;
    let engaged = e.button === 1;
    const move = (ev: MouseEvent): void => {
      const dx = ev.clientX - sx;
      const dy = ev.clientY - sy;
      if (!engaged && dx * dx + dy * dy > 16) {
        engaged = true;
        canvas.classList.add("panning");
      }
      if (engaged) {
        view.ox = ox0 + dx;
        view.oy = oy0 + dy;
        applyView();
      }
    };
    const up = (): void => {
      canvas.classList.remove("panning");
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  });

  const screenToWorld = (clientX: number, clientY: number): { x: number; y: number } => {
    const rect = canvas.getBoundingClientRect();
    return {
      x: (clientX - rect.left - view.ox) / view.z,
      y: (clientY - rect.top - view.oy) / view.z,
    };
  };

  // ─── frames ──────────────────────────────────────────────────────
  const frames = new Map<string, Frame>();

  const rectFromRec = (
    rec: Rec,
    defW: number,
    defH: number,
    idx: number,
  ): { x: number; y: number; w: number; h: number } => ({
    x: num(rec, "x") ?? 40 + (idx % 4) * 360,
    y: num(rec, "y") ?? 40 + Math.floor(idx / 4) * 280,
    w: num(rec, "width") ?? defW,
    h: num(rec, "height") ?? defH,
  });

  const persistRect = (aid: string, x?: number, y?: number, w?: number, h?: number): void => {
    // Geometry is FRONTEND state — patch the LOCAL record; the proxy_loader
    // debounce-persists it to the host web/kernel_state.
    const patch: Payload = {};
    if (x !== undefined) {
      patch["x"] = Math.round(x);
      patch["y"] = Math.round(y ?? 0);
    }
    if (w !== undefined) {
      patch["width"] = Math.round(w);
      patch["height"] = Math.round(h ?? 0);
    }
    kernel.updateMeta(aid, patch);
  };

  const startDrag = (e: MouseEvent, aid: string, el: HTMLElement): void => {
    e.preventDefault();
    el.classList.add("dragging");
    document.body.classList.add("dragging-frame");
    const sx = e.clientX;
    const sy = e.clientY;
    const ox = parseFloat(el.style.left);
    const oy = parseFloat(el.style.top);
    const move = (ev: MouseEvent): void => {
      el.style.left = `${ox + (ev.clientX - sx) / view.z}px`;
      el.style.top = `${oy + (ev.clientY - sy) / view.z}px`;
    };
    const up = (): void => {
      el.classList.remove("dragging");
      document.body.classList.remove("dragging-frame");
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      void persistRect(aid, parseFloat(el.style.left), parseFloat(el.style.top));
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  const startResize = (e: MouseEvent, aid: string, el: HTMLElement): void => {
    e.preventDefault();
    e.stopPropagation();
    el.classList.add("resizing");
    document.body.classList.add("dragging-frame");
    const sx = e.clientX;
    const sy = e.clientY;
    const ow = parseFloat(el.style.width);
    const oh = parseFloat(el.style.height);
    const move = (ev: MouseEvent): void => {
      el.style.width = `${Math.max(180, ow + (ev.clientX - sx) / view.z)}px`;
      el.style.height = `${Math.max(120, oh + (ev.clientY - sy) / view.z)}px`;
    };
    const up = (): void => {
      el.classList.remove("resizing");
      document.body.classList.remove("dragging-frame");
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      void persistRect(aid, undefined, undefined, parseFloat(el.style.width), parseFloat(el.style.height));
    };
    window.addEventListener("mousemove", move);
    window.addEventListener("mouseup", up);
  };

  // Build the frame chrome (head + empty body + resize handle). Kind-agnostic:
  // the caller fills `.frame-body` (inline mount or iframe) and wires reload.
  const makeShell = (
    rec: Rec,
    title: string,
    rect: { x: number; y: number; w: number; h: number },
    onReload: () => void,
  ): { el: HTMLElement; body: HTMLElement; actions: HTMLElement } => {
    const el = document.createElement("div");
    el.className = "agent-frame";
    el.style.left = `${rect.x}px`;
    el.style.top = `${rect.y}px`;
    el.style.width = `${rect.w}px`;
    el.style.height = `${rect.h}px`;
    const locked = bool(rec, "delete_lock");
    el.innerHTML = `
      <div class="frame-head">
        <span class="id">${title} · <code style="opacity:0.6;font-size:10px">${rec.id}</code></span>
        <span class="actions">
          <span class="lock ${locked ? "locked" : ""}" title="${locked ? "unlock (allow delete)" : "lock (refuse delete)"}">${locked ? "🔒" : "🔓"}</span>
          <span class="reload" title="reload">⟳</span>
          <span class="close" title="delete">×</span>
        </span>
      </div>
      <div class="frame-body"></div>
      <div class="resize-handle"></div>
    `;
    world.appendChild(el);
    const head = el.querySelector(".frame-head") as HTMLElement;
    head.addEventListener("mousedown", (e) => {
      if ((e.target as Element).closest(".actions")) return;
      startDrag(e, rec.id, el);
    });
    (el.querySelector(".resize-handle") as HTMLElement).addEventListener("mousedown", (e) =>
      startResize(e as MouseEvent, rec.id, el),
    );
    (el.querySelector(".reload") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      onReload();
    });
    (el.querySelector(".lock") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      const cur = bool(frames.get(rec.id)?.rec ?? rec, "delete_lock");
      const lockEl = el.querySelector(".lock") as HTMLElement;
      lockEl.classList.toggle("locked", !cur);
      lockEl.textContent = !cur ? "🔒" : "🔓";
      kernel.updateMeta(rec.id, { delete_lock: !cur }); // local; proxy_loader syncs
    });
    (el.querySelector(".close") as HTMLElement).addEventListener("click", () => closeView(rec.id));
    return {
      el,
      body: el.querySelector(".frame-body") as HTMLElement,
      actions: el.querySelector(".actions") as HTMLElement,
    };
  };

  // CLOSE a view: if it SPAWNED its host backend (owns_backend — e.g. the
  // dblclick terminal), that peer is deleted too, so the pair goes TOGETHER
  // (the backend's on_delete tears down its process). A view bound to a
  // PRE-EXISTING / shared backend leaves it running (weak peer — reopening
  // re-attaches). A delete_lock refuses both.
  const closeView = (viewId: string): void => {
    const r = frames.get(viewId)?.rec ?? (kernel.get(viewId)?.record() as Rec | undefined);
    if (r === undefined) return;
    if (bool(r, "delete_lock")) {
      console.warn("[canvas] delete refused — delete_lock on", viewId);
      return;
    }
    // An AI cell RELEASES its AI: the back deletes it once no cell record names
    // it (a kept dialog may be open in other cells; its rows stay in SQLite).
    if (r["handler_module"] === aiView.handles[0] && str(r, "backend_id")) {
      const ai = str(r, "backend_id")!;
      kernel.remove(viewId);
      void flush().then(() => host.callHost("ai_cells", { type: "release", id: ai } as unknown as Payload)).then(() => void renderChats());
      return;
    }
    // the backend's ROOT goes (a terminal's exec agent)
    const backendId = str(r, "backend_root") ?? str(r, "backend_id");
    if (bool(r, "owns_backend") && backendId) {
      void host.callHost("kernel", { type: "delete_agent", id: backendId });
    }
    kernel.remove(viewId);
  };

  // ── AI CELLS ─────────────────────────────────────────────────────────
  // The host's `ai_cells` agent spawns a cell's backend from one of the user's
  // templates (only the CLI reads those): a RAM store, the AI under it.
  const AI_CELLS = "ai_cells";
  const toast = (text: string): void => {
    const t = document.createElement("div");
    t.className = "canvas-toast";
    t.textContent = text;
    document.body.appendChild(t);
    setTimeout(() => t.remove(), 6000);
  };
  const presets = async (): Promise<Presets | null> => {
    const r = (await host.callHost(AI_CELLS, { type: "presets" } as unknown as Payload).catch(() => null)) as Rec | null;
    if (!r || typeof r["error"] === "string") return null;
    return { templates: (r["templates"] as Presets["templates"]) ?? [], preferred: (r["preferred"] as string | null) ?? null };
  };
  const spawnAi = async (viewId: string, template: string | null): Promise<Rec | null> => {
    const r = (await host.callHost(AI_CELLS, {
      type: "spawn",
      view_id: viewId,
      canvas_id: selfId,
      ...(template ? { template } : {}),
    } as unknown as Payload)) as Rec;
    if (typeof r["error"] === "string") {
      toast(`✕ ${r["error"]}${typeof r["hint"] === "string" && r["hint"] ? ` — ${r["hint"]}` : ""}`);
      return null;
    }
    return r;
  };
  const bindAi = (viewId: string, r: Rec): void => {
    kernel.updateMeta(viewId, {
      backend_id: r["id"],
      backend_root: r["root"],
      backend_lost: false,
      template: r["template"],
      can_call: { ...aiView.grants(String(r["id"])) },
    } as unknown as Payload);
  };

  // Mount a first-party TS view-agent inline into a fresh frame.
  const mountInline = async (rec: Rec, bundle: ViewBundle, idx: number): Promise<void> => {
    const rect = rectFromRec(rec, 480, 320, idx);
    let reload = (): void => {};
    const { el, body, actions } = makeShell(rec, str(rec, "display_name") ?? rec.id, rect, () => reload());
    if ((bool(rec, "owns_backend") || bool(rec, "kept")) && str(rec, "backend_id") === undefined && !bool(rec, "backend_lost")) {
      // Saved, its backend not spawned (or a kept dialog not reopened) yet: a
      // placeholder until `bind` lands and the refresh remounts it.
      const note = document.createElement("div");
      note.className = "backend-starting";
      note.textContent = "starting its backend…";
      body.appendChild(note);
      frames.set(rec.id, { el, rec, kind: "inline" });
      return;
    }
    if (bool(rec, "backend_lost")) {
      // Its backend is gone: say so, mount nothing, and let ⟳ respawn it (a
      // fresh child of the leg, bound to this same view).
      const note = document.createElement("div");
      note.className = "backend-lost";
      note.textContent = `backend ${str(rec, "backend_id") ?? "?"} is gone — ⟳ respawns it`;
      body.appendChild(note);
      frames.set(rec.id, { el, rec, kind: "inline" });
      reload = (): void => {
        if (bundle === aiView) {
          void spawnAi(rec.id, str(rec, "template") ?? null).then((r) => {
            if (r === null) return;
            removeFrame(rec.id);
            bindAi(rec.id, r);
          });
          return;
        }
        void spawnBackend(bundle, rec.id).then((bid) => {
          if (bid === null) return;
          removeFrame(rec.id);
          bind(rec.id, bundle, bid);
        });
      };
      return;
    }
    const backend = str(rec, "backend_id") ?? rec.id; // weak peer ref to a host backend (or self)
    // THE VIEW SENDS AS ITSELF: its kernel is scoped to its own id, so the
    // page's egress gate checks the view's `can_call` before anything leaves.
    const asView = kernel.scopedFor(rec.id, null);
    const close = (): void => closeView(rec.id);
    // The frame is REGISTERED before the mount's awaits: a refresh landing
    // meanwhile must see it, or it mounts the same view a second time.
    frames.set(rec.id, { el, rec, kind: "inline" });
    const handle = await bundle.mount({ kernel: asView, mount: body, selfId: rec.id, backend, record: rec, close });
    const mine = frames.get(rec.id);
    if (mine === undefined || mine.el !== el) {
      handle.unmount(); // removed while it mounted
      return;
    }
    mine.handle = handle;
    // reload = remount the inline view in place
    reload = (): void => {
      handle.unmount();
      body.innerHTML = "";
      void Promise.resolve(bundle.mount({ kernel: asView, mount: body, selfId: rec.id, backend, record: rec, close })).then((h) => {
        const f = frames.get(rec.id);
        if (f) f.handle = h;
      });
    };
    // declarative header chips, wired locally (no bus)
    for (const btn of handle.headerButtons ?? []) {
      const chip = document.createElement("span");
      chip.className = "ext";
      chip.textContent = btn.glyph;
      chip.title = btn.title;
      chip.addEventListener("click", (e) => {
        e.stopPropagation();
        const active = btn.onClick();
        if (btn.toggle) chip.classList.toggle("active", active === true);
      });
      actions.insertBefore(chip, actions.firstChild);
    }
  };

  // Mount untrusted/external content as an iframe.
  const mountIframe = (rec: Rec, wa: Webapp, idx: number): void => {
    const rect = rectFromRec(rec, wa.default_width ?? 340, wa.default_height ?? 240, idx);
    const { el, body } = makeShell(rec, wa.title ?? rec.id, rect, () =>
      host.emit(rec.id, { type: "reload_html" }),
    );
    const iframe = document.createElement("iframe");
    iframe.src = wa.url;
    body.appendChild(iframe);
    frames.set(rec.id, { el, rec, kind: "iframe" });
  };

  const removeFrame = (aid: string): void => {
    const f = frames.get(aid);
    if (f === undefined) return;
    if (f.kind === "inline" && f.handle) {
      try {
        f.handle.unmount();
      } catch (e) {
        console.error("[canvas] unmount raised:", e);
      }
    }
    f.el.remove();
    frames.delete(aid);
  };

  const applyRectFromRec = (el: HTMLElement, rec: Rec): void => {
    const x = num(rec, "x");
    if (x !== undefined) {
      el.style.left = `${x}px`;
      el.style.top = `${num(rec, "y") ?? 0}px`;
    }
    const w = num(rec, "width");
    if (w !== undefined) el.style.width = `${w}px`;
    const h = num(rec, "height");
    if (h !== undefined) el.style.height = `${h}px`;
  };

  let bgShown = ""; // the background source the GL host shows now
  const refresh = async (): Promise<void> => {
    // Members are this canvas's OWN children in the LOCAL tree (hydrated from
    // web/kernel_state, persisted back via proxy_loader). No host membership read.
    const root = kernel.rootId !== null ? kernel.get(kernel.rootId) : undefined;
    const members: Rec[] =
      root !== undefined
        ? [...root.children.values()].map((a) => a.record() as Rec)
        : [];

    let idx = 0;
    const seenDom = new Set<string>();
    const seenGl = new Set<string>();
    for (const rec of members) {
      const mid = rec.id;
      let existing = frames.get(mid);
      // A pair that changed state (bound, lost, respawned) is REMOUNTED: the
      // view fronts a different backend now, or none.
      if (
        existing !== undefined &&
        (str(existing.rec, "backend_id") !== str(rec, "backend_id") ||
          bool(existing.rec, "backend_lost") !== bool(rec, "backend_lost"))
      ) {
        removeFrame(mid);
        existing = undefined;
      }
      if (existing !== undefined) {
        existing.rec = rec;
        applyRectFromRec(existing.el, rec);
        seenDom.add(mid);
        if (glHost?.hasView(mid)) seenGl.add(mid);
        idx++;
        continue;
      }
      // Every member is a `*.ts` frontend bundle — match its handler_module to
      // an inline ViewBundle (terminal/chat fronts a backend; content agents
      // fall through to the probe below).
      const bundle = viewFor(rec["handler_module"]);
      if (bundle !== undefined) {
        await mountInline(rec, bundle, idx);
        seenDom.add(mid);
        idx++;
        continue;
      }
      // Otherwise probe the member's BACKEND peer (by id) for DOM/GL presence
      // — a content member (gl_agent) answers on its own id; a host-backed one
      // answers on its `backend_id`.
      const backend = str(rec, "backend_id") ?? mid;
      const [waR, glR] = await Promise.all([
        host.call(backend, { type: "get_webapp" }).catch(() => null),
        host.call(backend, { type: "get_gl_view" }).catch(() => null),
      ]);
      const wa = waR as Webapp | null;
      if (wa !== null && typeof wa.url === "string" && !("error" in (wa as object))) {
        mountIframe(rec, wa, idx);
        seenDom.add(mid);
        idx++;
      }
      const gl = glR as GlView | null;
      if (gl !== null && typeof gl.source === "string" && gl.source) {
        const gh = await ensureGlHost();
        if (!gh.hasView(mid)) gh.installView(mid, gl);
        seenGl.add(mid);
      }
    }
    // THE BACKGROUND: the root's own `bg_source`, mounted as a GL view under the
    // canvas's id, behind every member; a changed source is swapped in place.
    const bgSource = root !== undefined && typeof root.meta["bg_source"] === "string" ? (root.meta["bg_source"] as string) : "";
    if (bgSource !== "") {
      const gh = await ensureGlHost();
      if (bgSource !== bgShown) {
        gh.removeView(selfId);
        bgShown = bgSource;
      }
      if (!gh.hasView(selfId)) gh.installView(selfId, { source: bgSource });
      seenGl.add(selfId);
    } else {
      bgShown = "";
    }
    for (const aid of [...frames.keys()]) if (!seenDom.has(aid)) removeFrame(aid);
    glHost?.prune(seenGl);
    const glCount = glHost === null ? 0 : seenGl.size;
    status.innerHTML = `canvas <code>${selfId}</code> · ${frames.size} dom · ${glCount} gl`;
  };

  // Re-render on LOCAL tree changes (added/updated/removed). The canvas owns
  // its membership; the proxy_loader syncs it to the host independently.
  kernel.addStateSubscriber(() => void refresh());

  // dblclick → the DEFAULT VIEW and its backend, in THIS order: the view
  // record lands on disk first (flushed through the loader), then the backend
  // is spawned for it, then the view is bound. A crash in between leaves a view
  // without a backend (shown lost, ⟳ respawns), never a backend without a view
  // — so no page's reconcile can delete a live backend another page is binding.
  const spawnTerminalAt = async (w: { x: number; y: number }): Promise<void> => {
    const viewId = `view_${crypto.randomUUID().slice(0, 8)}`;
    kernel.register(
      new Agent({
        id: viewId,
        parentId: kernel.rootId ?? selfId,
        handlerModule: DEFAULT_VIEW.handles[0]!,
        meta: {
          owns_backend: true, // this canvas spawns it → closing removes the pair together
          x: Math.round(w.x),
          y: Math.round(w.y),
        },
      }),
    );
    await flush();
    const backendId = await spawnBackend(DEFAULT_VIEW, viewId);
    if (backendId === null) {
      kernel.remove(viewId);
      return;
    }
    bind(viewId, DEFAULT_VIEW, backendId);
  };

  // An AI cell at the spot, its first turn what was typed. The same order as a
  // terminal: the view lands on disk first, then its backend, then the bind.
  const spawnAiAt = async (w: { x: number; y: number }, template: string | null, text: string): Promise<void> => {
    const viewId = `ai_view_${crypto.randomUUID().slice(0, 8)}`;
    const x = Math.round(w.x);
    const y = Math.round(w.y);
    kernel.register(
      new Agent({
        id: viewId,
        parentId: kernel.rootId ?? selfId,
        handlerModule: aiView.handles[0]!,
        meta: {
          owns_backend: true,
          x,
          y,
          width: 440,
          height: 520,
          ...(text ? { first_turn: { text, x, y } } : {}),
        },
      }),
    );
    await flush();
    const r = await spawnAi(viewId, template);
    if (r === null) {
      kernel.remove(viewId);
      return;
    }
    bindAi(viewId, r);
  };

  // A KEPT dialog in a new cell: the live AI when another cell has it open (one
  // dialog, several cells), else a fresh one on SQLite with its history.
  const reopenAt = async (w: { x: number; y: number }, dialog: { id: string; name: string }): Promise<void> => {
    const viewId = `ai_view_${crypto.randomUUID().slice(0, 8)}`;
    kernel.register(
      new Agent({
        id: viewId,
        parentId: kernel.rootId ?? selfId,
        handlerModule: aiView.handles[0]!,
        meta: { owns_backend: false, kept: true, dialog_name: dialog.name, x: Math.round(w.x), y: Math.round(w.y), width: 440, height: 520 },
      }),
    );
    await flush();
    const r = (await host.callHost(AI_CELLS, { type: "reopen", id: dialog.id } as unknown as Payload)) as Rec;
    if (typeof r["error"] === "string") {
      toast(`✕ ${r["error"]}`);
      kernel.remove(viewId);
      return;
    }
    bindAi(viewId, r);
  };

  // ── THE CHATS SIDEBAR (collapsible, left edge) ─────────────────────────
  // Kept dialogs (● — SQLite, the number of cells showing each) and this
  // canvas's ephemeral cells (○ — RAM). A kept one opens in a new cell at the
  // view's centre; an ephemeral one is flashed where it is.
  const chats = document.createElement("div");
  chats.id = "chats";
  chats.innerHTML = `<div class="chats-head"><span>chats</span><button class="chats-toggle" title="collapse">«</button></div><ul class="chats-list"></ul><div class="chats-new">+ new chat here</div>`;
  mount.appendChild(chats);
  const chatsList = chats.querySelector(".chats-list") as HTMLElement;
  (chats.querySelector(".chats-toggle") as HTMLElement).addEventListener("click", () => {
    chats.classList.toggle("collapsed");
    (chats.querySelector(".chats-toggle") as HTMLElement).textContent = chats.classList.contains("collapsed") ? "»" : "«";
  });
  (chats.querySelector(".chats-new") as HTMLElement).addEventListener("click", () => {
    void spawnAiAt(screenToWorld(window.innerWidth / 2, window.innerHeight / 2), null, "");
  });
  const aiCells = (): Rec[] => {
    const root = kernel.rootId !== null ? kernel.get(kernel.rootId) : undefined;
    return root ? [...root.children.values()].map((a) => a.record() as Rec).filter((r) => r["handler_module"] === aiView.handles[0]) : [];
  };
  async function renderChats(): Promise<void> {
    const r = (await host.callHost(AI_CELLS, { type: "dialogs" } as unknown as Payload).catch(() => null)) as Rec | null;
    const kept = ((r?.["dialogs"] as Array<{ id: string; name: string }> | undefined) ?? []);
    const cells = aiCells();
    const items: HTMLElement[] = [];
    for (const d of kept) {
      const n = cells.filter((c) => c["backend_id"] === d.id).length;
      const li = document.createElement("li");
      li.className = "kept";
      li.innerHTML = `<span class="dot">●</span><span class="nm"></span><span class="ct"></span>`;
      (li.children[1] as HTMLElement).textContent = d.name;
      (li.children[2] as HTMLElement).textContent = n > 0 ? String(n) : "";
      li.title = "open in a new cell";
      li.addEventListener("click", () => void reopenAt(screenToWorld(window.innerWidth / 2, window.innerHeight / 2), d));
      items.push(li);
    }
    for (const c of cells.filter((c) => c["kept"] !== true && typeof c["backend_id"] === "string")) {
      const li = document.createElement("li");
      li.className = "eph";
      li.innerHTML = `<span class="dot">○</span><span class="nm"></span><span class="ct">1</span>`;
      (li.children[1] as HTMLElement).textContent = String(c["backend_id"]);
      li.title = "show it";
      li.addEventListener("click", () => {
        const f = frames.get(String(c["id"]));
        f?.el.classList.add("flash");
        setTimeout(() => f?.el.classList.remove("flash"), 900);
      });
      items.push(li);
    }
    chatsList.replaceChildren(...items);
  }

  const spawnHtmlAt = (w: { x: number; y: number }): void => {
    kernel.register(
      new Agent({
        id: `html_${crypto.randomUUID().slice(0, 8)}`,
        parentId: kernel.rootId ?? selfId,
        handlerModule: "html_agent.ts",
        meta: {
          x: Math.round(w.x),
          y: Math.round(w.y),
          width: 360,
          height: 240,
          html: "<p style='font:14px system-ui;color:#cde;padding:14px'>an empty html panel</p>",
        },
      }),
    );
  };

  // THE SPOT INPUT: a double click on empty canvas, or ⌘K / Ctrl-K at the pointer.
  let pointer = { x: window.innerWidth / 2, y: window.innerHeight / 2 };
  canvas.addEventListener("mousemove", (e) => {
    pointer = { x: e.clientX, y: e.clientY };
  });
  const spotAt = async (sx: number, sy: number): Promise<void> => {
    const w = screenToWorld(sx, sy);
    openSpot({
      screenX: sx,
      screenY: sy,
      presets: await presets(),
      onPick: (p: Pick): void => {
        if (p.kind === "terminal") void spawnTerminalAt(w);
        else if (p.kind === "html") spawnHtmlAt(w);
        else if (p.kind === "setup") window.open("setup.html", "_blank");
        else void spawnAiAt(w, p.template, p.text);
      },
    });
  };
  canvas.addEventListener("dblclick", (e) => {
    if ((e.target as Element).closest(".agent-frame")) return;
    void spotAt(e.clientX, e.clientY);
  });
  window.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      void spotAt(pointer.x, pointer.y);
    }
  });

  kernel.addStateSubscriber(() => void renderChats());
  await reconcile();
  await refresh();
  await renderChats();
}
