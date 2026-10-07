import { documentPage } from "../verb_docs.ts";
import { demoHtml } from "../../demo_page.ts";
import { Agent, Host } from "../../lib.ts";
import type { Json, Kernel, Payload } from "../../lib.ts";
import { CANVAS_CSS } from "./styles.ts";
import type { ViewBundle, ViewHandle } from "../view.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { aiView } from "../ai_view/ai_view.ts";
import { setupView } from "../setup_view/setup_view.ts";
import { reflectView } from "../reflect_view/reflect_view.ts";
import { streamView } from "../stream_view/stream_view.ts";
import { CODING_AGENTS, openActionPopup } from "./action_popup.ts";
import { ROOT_VIEW, VIEW, VIEW_VERBS, hostViewCell } from "./canvas_agent.ts";
import { containerOf, hashFor, isContainer, onMac, pathTo, viewFromHash } from "./viewer.ts";
import type { Lookup } from "./viewer.ts";
import { BACKGROUND, BACKGROUND_ID, bgCallableBy, viewToBg } from "./background_agent.ts";

/** What a host agent (a cell's AI) may ask ANY cell: look at it, read and
 *  write a panel's html, emit to it. */
const CELL_VERBS = ["reflect", "get_html", "set_html", "emit"] as const;
/** What a cell's AI may ask the background. */
const BG_VERBS = ["set_bg", "clear_bg", "get_bg", "reflect"] as const;
import { CANVASES_VIEW, SIDEBAR_VIEW, canvasesView, renderSidebars, setSideDeps, sidebarView } from "../side_view/side_view.ts";
import type { Dialog, TreeNode } from "./sidebar.ts";
import type { Pick, Presets } from "./action_popup.ts";
import { htmlView } from "../html_agent/html_view.ts";
import { agentViewBundle } from "../agent_view/agent_view.ts";
import { seenHere, setCanvasNode, setHostRecords, sideOf, viewChangedFor } from "./host_views.ts";
import { createGlHost } from "./gl_host.ts";
import type { GlHost, GlView } from "./gl_host.ts";

// The canvas view-AGENT: the DOM-root compositor. Owns document.body, renders
// host member agents either INLINE (a TS view-agent for first-party content,
// keyed by the member's handler_module) or as an IFRAME (untrusted/external
// content that answers get_webapp). Translates pan/zoom/drag/dblclick into
// kernel sends. The kernel knows none of this.

// inline view registry — host handler_module → TS view bundle
const VIEW_BUNDLES: readonly ViewBundle[] = [terminalView, aiView, htmlView, agentViewBundle, setupView, reflectView, streamView, sidebarView, canvasesView];
// A PAGE BUNDLE's instance (handler_module = its folder name) is drawn by its
// BASE's view (`base: html_agent` → `html_agent.ts`), read off the catalog row.
function viewFor(handlerModule: unknown, kernel: Kernel): ViewBundle | undefined {
  if (typeof handlerModule !== "string") return undefined;
  const direct = VIEW_BUNDLES.find((b) => b.handles.includes(handlerModule));
  if (direct !== undefined) return direct;
  const row = kernel.availableBundles().find((b) => b.name === handlerModule) as { base?: unknown; error?: unknown } | undefined;
  if (row === undefined || row.error !== undefined || typeof row.base !== "string") return undefined;
  const base = `${row.base}.ts`;
  return VIEW_BUNDLES.find((b) => b.handles.includes(base));
}

/** The exec leg's `allowed` entry for EXACTLY `argv` (shlex-joined, regex-
 *  escaped) — `kern/terminal.rs` `line_record`. */
export function exactLine(argv: string[]): string {
  const quote = (a: string): string => (a !== "" && /^[\w@%+=:,./-]+$/.test(a) ? a : `'${a.replace(/'/g, `'"'"'`)}'`);
  return argv.map(quote).join(" ").replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

// THE VIEW A DOUBLE CLICK SPAWNS: a terminal. What it spawns on the host is the
// view's own declaration (`fronts`), looked up by that exact name in the host
// catalog — never matched, never guessed.
const DEFAULT_VIEW = terminalView;


export interface MountOptions {
  kernel: Kernel;
  mount: HTMLElement;
  selfId: string;
  /** Land every pending record through the loader NOW — a cell is on disk
   *  before its backend exists, so no page ever sees a backend without one. */
  flush: () => Promise<void>;
  /** THIS canvas's host node (`kern/backend`, a `canvas.tools` agent): every
   *  cell's backend is its direct child; it creates, deletes and spawns. */
  node: string;
  /** Every canvas on the host (flat peers), the main one first. */
  canvases: Array<{ id: string; loader: string; main: boolean; name: string }>;
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
  const { kernel, mount, selfId, flush } = opts;
  const BACKEND = opts.node;
  setCanvasNode(BACKEND);
  const host = new Host(kernel, selfId);

  // THE ROOT VIEW: every page has exactly one, under the canvas root.
  if (kernel.get(ROOT_VIEW) === undefined && kernel.rootId !== null) {
    kernel.register(new Agent({ id: ROOT_VIEW, parentId: kernel.rootId, handlerModule: VIEW, sentence: "The root view: the canvas's first view." }));
  }

  // THE PAIR: a cell the canvas spawned a backend for, and that backend. The
  // backend is a direct child of `canvas_root` on the host, carrying the
  // cell's id (`cell_id`); the cell carries the backend's (`backend_id`,
  // `owns_backend`) and the grant to call it (`can_call`). The backend lives
  // its own life — a page connecting, leaving or reloading never touches it;
  // only closing the cell deletes its backend.

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

  // Spawn the backend `bundle` fronts, for cell `cellId`, under `canvas_root`.
  // Returns its id, or null (said why).
  const spawnBackend = async (bundle: ViewBundle, cellId: string, fields: Record<string, unknown> = {}): Promise<string | null> => {
    const module = bundle.fronts;
    if (module === undefined) {
      console.error(`[canvas] ${bundle.handles.join(",")} declares no backend it fronts — cannot spawn one`);
      return null;
    }
    if (!(await inCatalog(module))) {
      console.error(`[canvas] the host does not link ${module}`);
      return null;
    }
    const created = (await host.callHost(BACKEND, {
      type: "create",
      record: { ...(bundle.backendFields ?? {}), ...fields, handler_module: module, cell_id: cellId },
    } as unknown as Payload)) as { id?: string; error?: string };
    if (!created || created.error || typeof created.id !== "string") {
      console.error("[canvas] backend create failed:", JSON.stringify(created));
      return null;
    }
    return created.id;
  };

  // DELETING a host agent is its own act (backend-owned views: ✕ only closes a
  // view) — the kernel's `delete_agent`, whose delete-lock gate refuses what is
  // locked; its cells are cleaned when the registry sees it `removed`.
  const deleteAgent = (id: string): void => {
    void host.callHost("kernel", { type: "delete_agent", id } as unknown as Payload).then((r) => {
      const e = (r as Rec | null)?.["error"];
      if (typeof e === "string") toast(`✕ ${e}`);
    });
  };

  // BIND a spawned backend to its cell: the id, and the cell's own grant.
  const bind = (cellId: string, bundle: ViewBundle, backendId: string): void => {
    kernel.updateMeta(cellId, {
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

  // THE BACKGROUND SINGLETON: exactly one per view, made here when missing,
  // delete-locked on the root view (it can only be updated). A second is never
  // made; one found beside the first is said loudly and left for the human.
  // A NESTED view's background carries no lock: a locked descendant would
  // refuse the nested view's own removal.
  const ensureBackground = (container: string): void => {
    const c = kernel.get(container);
    if (c === undefined) return;
    const bgs = [...c.children.values()].filter((a) => a.record().handler_module === BACKGROUND);
    if (bgs.length > 1) console.error(`[canvas] ${bgs.length} background agents — a view has exactly one: ${bgs.map((a) => a.id).join(", ")}`);
    if (bgs.length > 0) return;
    const isRoot = container === ROOT_VIEW;
    kernel.register(
      new Agent({
        id: isRoot ? BACKGROUND_ID : `${container}_bg`,
        parentId: c.id,
        handlerModule: BACKGROUND,
        meta: isRoot
          ? { delete_lock: true, bg_source: "", callable_by: bgCallableBy() }
          : { bg_source: "", callable_by: bgCallableBy() },
      }),
    );
    // the view reaches its background BY NAME (both ends; no implicit parent
    // reach)
    kernel.updateMeta(c.id, {
      can_call: viewToBg(c.meta["can_call"] as Record<string, unknown> | undefined),
    } as unknown as Payload);
  };

  // ─── THE VIEWER (`viewer.ts`): what this page shows ───────────────
  const look: Lookup = (id) => {
    const a = kernel.get(id);
    return a === undefined ? undefined : { id: a.id, parentId: a.parentId ?? null, handler: String(a.record().handler_module ?? "") };
  };
  const rootOr = (): string => kernel.rootId ?? selfId;
  // every page agent, the root first
  const everyAgent = (): string[] => {
    const out: string[] = [];
    const walk = (a: Agent | undefined): void => {
      if (a === undefined) return;
      out.push(a.id);
      for (const c of a.children.values()) walk(c);
    };
    walk(kernel.get(rootOr()));
    return out;
  };
  const fromHash = viewFromHash(location.hash);
  let viewed: string = fromHash !== null && kernel.get(fromHash) !== undefined ? fromHash : ROOT_VIEW;
  // the view whose world is drawn: the viewed one, or (viewing a cell FULL)
  // the view it sits on
  const containerNow = (): string => containerOf(look, viewed) ?? ROOT_VIEW;
  const fullNow = (): boolean => !isContainer(look, viewed);
  // the view a cell is drawn on (a nested view's CARD sits on its parent)
  const hostCanvasOf = (id: string): string | null => {
    const p = look(id)?.parentId ?? null;
    return p === null ? null : containerOf(look, p);
  };
  // each view keeps its own camera across switches
  const cameras = new Map<string, { z: number; ox: number; oy: number }>();
  // THE HOST REGISTRY (backend-owned views, tmp/backend_owned_views.md): the
  // host tree MIRRORED — a snapshot, then the host's state stream (`added` /
  // `removed` / `updated`). The host is the truth; the page keeps only layout.
  // A cell whose agent is gone is CLEANED (a page never deletes a backend); an
  // agent added in the background is listed NEW, unplaced; a change of an
  // agent's `view` re-renders its cells.
  const registry = new Map<string, Rec>();
  /** host agents added in the background, not shown in any cell yet */
  const fresh = new Set<string>();
  /** host agents THIS page is creating (no NEW badge for them) */
  const placing = new Set<string>();
  setHostRecords((id) => registry.get(id));
  // the page cells showing host agent `agent`
  const shownBy = (agent: string): string[] =>
    everyAgent().filter((id) => {
      const m = kernel.get(id)?.meta ?? {};
      return m["backend_id"] === agent || m["backend_root"] === agent;
    });
  const cleanCellsOf = (agent: string): void => {
    for (const cell of shownBy(agent)) kernel.remove(cell);
  };
  let syncing: Promise<void> | null = null;
  let again = false;
  let synced = false;
  const syncRegistry = (): Promise<void> => {
    if (syncing !== null) {
      again = true;
      return syncing;
    }
    syncing = (async (): Promise<void> => {
      do {
        again = false;
        const all = await hostAgents().catch(() => null);
        if (all === null) break;
        // THIS canvas's agents only (soft isolation, owner 2026-10-08: the
        // stream is shared, the page filters): its node's subtree, plus the
        // shell's own agents under no canvas; another canvas's never
        const byId = new Map<string, Rec>();
        for (const a of all) if (typeof a["id"] === "string") byId.set(a["id"], a as Rec);
        const parentOf = (id: string): string | undefined => {
          const p = byId.get(id)?.["parent_id"];
          return typeof p === "string" ? p : undefined;
        };
        const isCanvas = (id: string): boolean => byId.get(id)?.["handler_module"] === "canvas.tools";
        const next = new Map<string, Rec>();
        for (const [id, a] of byId) if (seenHere(id, parentOf, isCanvas)) next.set(id, a);
        const changed: string[] = [];
        for (const [id, r] of next) {
          const was = registry.get(id);
          if (was === undefined && synced && !placing.has(id)) fresh.add(id);
          if (was !== undefined && JSON.stringify(was["view"] ?? null) !== JSON.stringify(r["view"] ?? null)) changed.push(id);
        }
        for (const id of [...registry.keys()]) {
          if (!next.has(id)) {
            fresh.delete(id);
            cleanCellsOf(id);
          }
        }
        registry.clear();
        for (const [k, v] of next) registry.set(k, v);
        for (const id of changed) viewChangedFor(id);
        // a host view's GRANT is derived from its agent's record, never written
        // by hand: each `agent_view` cell's `can_call` onto its agent follows
        // the agent's `view.calls`
        for (const cell of everyAgent()) {
          const a = kernel.get(cell);
          if (a === undefined || a.record().handler_module !== "agent_view.ts") continue;
          const agent = typeof a.meta["backend_id"] === "string" ? (a.meta["backend_id"] as string) : undefined;
          const rec = agent !== undefined ? registry.get(agent) : undefined;
          if (agent === undefined || rec === undefined) continue;
          const want = hostViewCell(agent, rec).meta["can_call"] as Record<string, unknown>;
          const have = (a.meta["can_call"] as Record<string, unknown> | undefined) ?? {};
          if (JSON.stringify(have[agent]) !== JSON.stringify(want[agent])) {
            kernel.updateMeta(cell, { can_call: { ...have, [agent]: want[agent] } } as unknown as Payload);
          }
        }
        // the first sync also cleans cells whose agent went while no page was open
        if (!synced) {
          for (const id of everyAgent()) {
            const m = kernel.get(id)?.meta ?? {};
            const root = typeof m["backend_root"] === "string" ? m["backend_root"] : undefined;
            const shows = root ?? (typeof m["backend_id"] === "string" ? m["backend_id"] : undefined);
            if (shows !== undefined && !registry.has(shows)) kernel.remove(id);
          }
          synced = true;
        }
        for (const id of [...fresh]) if (shownBy(id).length > 0) fresh.delete(id);
      } while (again);
      syncing = null;
    })();
    return syncing;
  };
  // the host's lifecycle events, debounced (an `updated` comes with every record write)
  let syncTimer: ReturnType<typeof setTimeout> | null = null;
  kernel.subscribeState((frame) => {
    const k = (frame as Rec)["kind"];
    if (k !== "added" && k !== "removed" && k !== "updated") return;
    if (syncTimer !== null) clearTimeout(syncTimer);
    syncTimer = setTimeout(() => {
      syncTimer = null;
      void syncRegistry().then(() => renderSoon());
    }, 250);
  });
  const reconcile = (): Promise<void> => syncRegistry();

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
  mount.appendChild(canvas);

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
    el.dataset.id = rec.id; // the cell's agent id, for anyone addressing the DOM
    // FE (page content) or BE (a view of a host agent): drawn in two colors
    el.dataset.side = sideOf(rec);
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
          <span class="close" title="close the view (a host agent lives on)">×</span>
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

  // CLOSE a cell = close the VIEW (backend-owned views): the host agent lives
  // on, listed in the sidebar; deleting an agent is its own act. A delete_lock
  // refuses (the kernel's `remove` is the gate).
  const closeView = (cellId: string): void => {
    const out = kernel.remove(cellId) as unknown as Rec;
    if (out["deleted"] !== true) toast(`✕ ${typeof out["error"] === "string" ? out["error"] : `could not close ${cellId}`}`);
    renderSoon();
  };

  // ── AI CELLS ─────────────────────────────────────────────────────────
  // `canvas_root` (`kern/backend`) spawns the cell's AI from one of the user's
  // templates (only the CLI reads those), and answers the templates and the
  // kept dialogs.
  const toast = (text: string): void => {
    const t = document.createElement("div");
    t.className = "canvas-toast";
    t.textContent = text;
    document.body.appendChild(t);
    setTimeout(() => t.remove(), 6000);
  };
  // THE HUMAN'S PROGRAMS (`user path`, scanned when `user` boots): read once
  // per page; the menu picks the coding agents it knows out of them
  let programs: Promise<Record<string, string>> | null = null;
  const codingAgents = async (): Promise<Array<{ name: string; path: string }>> => {
    programs ??= host.callHost("user", { type: "path" } as unknown as Payload).then(
      (r) => ((r as Rec | null)?.["executables"] as Record<string, string> | undefined) ?? {},
      () => ({}),
    );
    const exes = await programs;
    return CODING_AGENTS.filter((a) => typeof exes[a.name] === "string").map((a) => ({ name: a.name, path: exes[a.name]! }));
  };
  const presets = async (): Promise<Presets | null> => {
    const r = (await host.callHost(BACKEND, { type: "presets" } as unknown as Payload).catch(() => null)) as Rec | null;
    if (!r || typeof r["error"] === "string") return null;
    const d = (await host.callHost(BACKEND, { type: "dialogs" } as unknown as Payload).catch(() => null)) as Rec | null;
    return {
      templates: (r["templates"] as Presets["templates"]) ?? [],
      preferred: (r["preferred"] as string | null) ?? null,
      dialogs: ((d?.["dialogs"] as Presets["dialogs"] | undefined) ?? []).map((x) => ({ id: x.id, name: x.name })),
      coding: await codingAgents(),
    };
  };
  // THE VIEW AS A PEER: a cell's AI (on the host) reaches the cell's view at
  // `<page guid>|<view id>` — its control plane and `reflect`.
  const peerGrants = (view: string): Record<string, string[]> => {
    if (kernel.guid === null) return {};
    const bg = [...(kernel.get(view)?.children.values() ?? [])].find((a) => a.record().handler_module === BACKGROUND)?.id;
    return {
      // the view's control plane over its cells
      [`${kernel.guid}|${view}`]: [...VIEW_VERBS],
      ...(bg !== undefined ? { [`${kernel.guid}|${bg}`]: [...BG_VERBS] } : {}),
      // any cell — and the view verbs, since the cells are their OPERANDS
      // (`move {id}`, `wire {sender, receiver}`): the host's egress gate checks
      // the agents a call names, not only its target
      [`${kernel.guid}|*`]: [...new Set([...CELL_VERBS, ...VIEW_VERBS])],
    };
  };
  const spawnAi = async (cellId: string, template: string | null, view?: string): Promise<Rec | null> => {
    const r = (await host.callHost(BACKEND, {
      type: "spawn",
      stamp: { cell_id: cellId },
      peer_grants: peerGrants(view ?? containerOf(look, cellId) ?? ROOT_VIEW),
      ...(template ? { template } : {}),
    } as unknown as Payload)) as Rec;
    if (typeof r["error"] === "string") {
      toast(`✕ ${r["error"]}${typeof r["hint"] === "string" && r["hint"] ? ` — ${r["hint"]}` : ""}`);
      return null;
    }
    return r;
  };
  const bindAi = (cellId: string, r: Rec): void => {
    kernel.updateMeta(cellId, {
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
      // fresh child of `canvas_root`, bound to this same cell).
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
    // the cell sends from outside any handler: as the page's operator; the
    // host judges every call as `user` (`user.forward`)
    const asView = kernel;
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
    // Members are this view's OWN children in the LOCAL tree (hydrated from
    // web/kernel_state, persisted back via proxy_loader). No host membership read.
    // THE VIEWED: a view's own children on its world, or one agent FULL.
    if (kernel.get(viewed) === undefined) viewed = ROOT_VIEW; // removed meanwhile
    const full = fullNow();
    const cont = kernel.get(containerNow());
    const all: Rec[] = cont !== undefined ? [...cont.children.values()].map((a) => a.record() as Rec) : [];
    // the background is drawn behind everything, not framed
    const members = full ? [kernel.get(viewed)!.record() as Rec] : all.filter((r) => r["handler_module"] !== BACKGROUND);
    const bgRec = full ? undefined : all.find((r) => r["handler_module"] === BACKGROUND);

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
        // the lock chip follows the record
        const lockEl = existing.el.querySelector(":scope > .frame-head .lock") as HTMLElement | null;
        if (lockEl !== null) {
          const locked = bool(rec, "delete_lock");
          lockEl.classList.toggle("locked", locked);
          lockEl.textContent = locked ? "🔒" : "🔓";
          lockEl.title = locked ? "unlock (allow delete)" : "lock (refuse delete)";
        }
        seenDom.add(mid);
        if (glHost?.hasView(mid)) seenGl.add(mid);
        idx++;
        continue;
      }
      // Every member is a `*.ts` frontend bundle — match its handler_module to
      // an inline ViewBundle (terminal/chat fronts a backend; content agents
      // fall through to the probe below).
      const bundle = viewFor(rec["handler_module"], kernel);
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
    // THE BACKGROUND: the view's singleton background agent's `bg_source`, mounted
    // as a GL view under its id, behind every member; swapped in place.
    const bgSource = bgRec !== undefined && typeof bgRec["bg_source"] === "string" ? (bgRec["bg_source"] as string) : "";
    if (bgSource !== "" && bgRec !== undefined) {
      const gh = await ensureGlHost();
      if (bgSource !== bgShown) {
        gh.removeView(bgRec.id);
        bgShown = bgSource;
      }
      if (!gh.hasView(bgRec.id)) gh.installView(bgRec.id, { source: bgSource });
      seenGl.add(bgRec.id);
    } else {
      bgShown = "";
    }
    for (const aid of [...frames.keys()]) if (!seenDom.has(aid)) removeFrame(aid);
    glHost?.prune(seenGl);
    canvas.classList.toggle("full-view", full);
    for (const [id, f] of frames) f.el.classList.toggle("full", full && id === viewed);
    const glCount = glHost === null ? 0 : seenGl.size;
  };

  // Re-render on LOCAL tree changes (added/updated/removed). The canvas owns
  // its membership; the proxy_loader syncs it to the host independently.
  kernel.addStateSubscriber(() => void refresh());

  // dblclick → the DEFAULT VIEW BUNDLE's cell and its backend, in THIS order:
  // the cell record lands on disk first (flushed through the loader), then the
  // backend is spawned for it, then the cell is bound. A crash in between leaves
  // a cell without a backend (shown lost, ⟳ respawns), never a backend without a
  // cell — so no page's reconcile can delete a live backend another page is binding.
  // every cell is CREATED with its geometry (the host answers `around` from the records)
  const spawnPairAt = async (
    bundle: ViewBundle,
    w: { x: number; y: number },
    size: { width?: number; height?: number } = { width: 480, height: 320 },
    extra: Record<string, unknown> = {},
    backendFields: Record<string, unknown> = {},
  ): Promise<void> => {
    // HOST-FIRST (backend-owned views): the host agent is made, then a cell
    // VIEWS it. The cell owns nothing: ✕ closes the view, the agent lives on.
    const cellId = `cell_${crypto.randomUUID().slice(0, 8)}`;
    const backendId = await spawnBackend(bundle, cellId, backendFields);
    if (backendId === null) return;
    placing.add(backendId);
    kernel.register(
      new Agent({
        id: cellId,
        parentId: containerNow(),
        handlerModule: bundle.handles[0]!,
        meta: {
          backend_id: backendId,
          owns_backend: false,
          can_call: { ...bundle.grants(backendId) },
          x: Math.round(w.x),
          y: Math.round(w.y),
          ...size,
          ...extra,
        } as unknown as Payload,
      }),
    );
  };
  const spawnTerminalAt = (w: { x: number; y: number }): Promise<void> => spawnPairAt(DEFAULT_VIEW, w);

  // MOVE TO a cell: centre it in the viewport (zoom kept) and flash it.
  const moveTo = (cellId: string): void => {
    // FOCUS: a cell on another view (or viewed full) — view its view first
    const hc = hostCanvasOf(cellId);
    if (hc !== null && (hc !== viewed || fullNow())) {
      setViewed(hc);
      void refresh().then(() => moveTo(cellId));
      return;
    }
    const f = frames.get(cellId);
    if (!f) return;
    const r = rectFromRec(f.rec, 480, 320, 0);
    const rect = canvas.getBoundingClientRect();
    view.ox = rect.width / 2 - (r.x + r.w / 2) * view.z;
    view.oy = rect.height / 2 - (r.y + r.h / 2) * view.z;
    applyView();
    f.el.classList.add("flash");
    setTimeout(() => f.el.classList.remove("flash"), 900);
  };

  // THE SET-UP CELL — one per view: opening it again moves to it.
  const openSetupAt = (w: { x: number; y: number }): void => {
    const c = kernel.get(containerNow());
    const open = c ? [...c.children.values()].find((a) => a.record().handler_module === setupView.handles[0]) : undefined;
    if (open) moveTo(open.id);
    else void spawnPairAt(setupView, w, { width: 640, height: 720 });
  };

  // An AI cell at the spot, its first turn what was typed. The same order as a
  // terminal: the cell lands on disk first, then its backend, then the bind.
  const spawnAiAt = async (w: { x: number; y: number }, template: string | null, text: string): Promise<void> => {
    await spawnAiIn(containerNow(), w, template, text);
  };
  // An AI cell on view `parent`: the cell, then its backend. Its id, or null
  // (the refusal toasted, the cell removed).
  const spawnAiIn = async (parent: string, w: { x: number; y: number }, template: string | null, text: string): Promise<string | null> => {
    // HOST-FIRST: the AI is made (stamped with the cell it will show in), then
    // the cell views it
    const viewId = `ai_view_${crypto.randomUUID().slice(0, 8)}`;
    const x = Math.round(w.x);
    const y = Math.round(w.y);
    const r = await spawnAi(viewId, template, parent);
    if (r === null) return null;
    placing.add(String(r["id"]));
    kernel.register(
      new Agent({
        id: viewId,
        parentId: parent,
        handlerModule: aiView.handles[0]!,
        meta: {
          owns_backend: false,
          x,
          y,
          width: 440,
          height: 520,
          ...(text ? { first_turn: { text, x, y } } : {}),
        },
      }),
    );
    bindAi(viewId, r);
    return viewId;
  };

  // A PLUGIN template placed (backend-owned views): `canvas_root spawn` makes
  // its host agent (a copy of the template), then a cell VIEWS it, drawn as the
  // agent's own record says — host-first, like a terminal.
  // A STARTER BUNDLE (`starter`): the same, from a NEW project bundle folder the
  // host scaffolds (`canvas_root spawn {bundle}`).
  const spawnPluginAt = async (w: { x: number; y: number }, template: string | null, starter?: string): Promise<void> => {
    const cellId = `view_${crypto.randomUUID().slice(0, 8)}`;
    const what = starter !== undefined ? { bundle: starter } : { template };
    const r = (await host.callHost(BACKEND, { type: "spawn", ...what, stamp: { cell_id: cellId } } as unknown as Payload)) as Rec;
    if (typeof r["error"] === "string") {
      toast(`✕ ${r["error"]}`);
      return;
    }
    const id = String(r["id"]);
    placing.add(id);
    await syncRegistry();
    const rec = registry.get(id);
    if (rec === undefined) {
      toast(`✕ ${id} was made but is not on the host`);
      return;
    }
    const { handler, meta } = hostViewCell(id, rec);
    kernel.register(
      new Agent({
        id: cellId,
        parentId: containerNow(),
        handlerModule: handler,
        meta: { ...meta, x: Math.round(w.x), y: Math.round(w.y), width: 480, height: 360 } as unknown as Payload,
      }),
    );
  };

  // A KEPT dialog in a new cell: the live AI when another cell has it open (one
  // dialog, several cells), else a fresh one on SQLite with its history.
  const reopenAt = async (w: { x: number; y: number }, dialog: { id: string; name: string }): Promise<void> => {
    const viewId = `ai_view_${crypto.randomUUID().slice(0, 8)}`;
    kernel.register(
      new Agent({
        id: viewId,
        parentId: containerNow(),
        handlerModule: aiView.handles[0]!,
        meta: { owns_backend: false, kept: true, dialog_name: dialog.name, x: Math.round(w.x), y: Math.round(w.y), width: 440, height: 520 },
      }),
    );
    await flush();
    const r = (await host.callHost(BACKEND, {
      type: "reopen",
      id: dialog.id,
      stamp: { cell_id: viewId },
      peer_grants: peerGrants(containerOf(look, viewId) ?? ROOT_VIEW),
    } as unknown as Payload)) as Rec;
    if (typeof r["error"] === "string") {
      toast(`✕ ${r["error"]}`);
      kernel.remove(viewId);
      return;
    }
    bindAi(viewId, r);
  };

  // VIEW an agent: a view (its world, its camera, its background) or any
  // other page agent FULL. `push`: a new history entry (`#view=<id>`).
  function setViewed(id: string, push = true): void {
    if (kernel.get(id) === undefined || id === viewed) return;
    const was = containerNow();
    cameras.set(was, { z: view.z, ox: view.ox, oy: view.oy });
    viewed = id;
    const now = containerNow();
    if (now !== was) {
      const cam = cameras.get(now) ?? { z: 1, ox: 0, oy: 0 };
      Object.assign(view, cam);
      targetZ = view.z;
      applyView();
    }
    if (isContainer(look, id)) ensureBackground(id);
    if (push && location.hash !== hashFor(id)) history.pushState(null, "", hashFor(id));
    void refresh();
    renderSoon();
  }
  // ── THE SIDEBAR (`sidebar.ts`): chats + every kernel's agent tree ──────
  const centre = (): { x: number; y: number } => screenToWorld(window.innerWidth / 2, window.innerHeight / 2);
  const pageRoot = (): Agent | undefined => (kernel.rootId !== null ? kernel.get(kernel.rootId) : undefined);
  // every AI cell on every view of this page
  const aiCells = (): Rec[] =>
    everyAgent()
      .map((id) => kernel.get(id)!.record() as Rec)
      .filter((r) => r["handler_module"] === aiView.handles[0]);
  const toNode = (a: Agent): TreeNode => {
    const rec = a.record() as Rec;
    return { id: a.id, handler: String(rec["handler_module"] ?? ""), rec, children: [...a.children.values()].map(toNode) };
  };
  // THE SIDEBAR and CANVAS-LIST cells (`side_view.ts`) read the canvas through
  // this wiring; none is made by default — the double-click places them
  setSideDeps({
    dialogs: async () => {
      const r = (await host.callHost(BACKEND, { type: "dialogs" } as unknown as Payload)) as Rec | null;
      return (r?.["dialogs"] as Dialog[] | undefined) ?? [];
    },
    aiCells,
    // this canvas's host agents (the registry: its subtree + the shell's own)
    hostAgents: async () => {
      await syncRegistry();
      return [...registry.values()];
    },
    pageTree: () => {
      const root = pageRoot();
      return root ? toNode(root) : null;
    },
    // the cell itself, or the cells fronting it (its backend, or its root)
    cellsFor: (id) => {
      if (kernel.get(id) !== undefined && id !== rootOr()) return [id];
      return everyAgent()
        .map((a) => kernel.get(a)!.record() as Rec)
        .filter((r) => r["backend_id"] === id || r["backend_root"] === id)
        .map((r) => String(r["id"]));
    },
    moveTo,
    view: (id, focus) => (focus && id !== rootOr() ? moveTo(id) : setViewed(id)),
    viewed: () => viewed,
    mac: onMac(),
    // an agent no cell shows: an AI opens in a chat cell, anything else as its
    // reflect card — a weak peer either way (✕ leaves the agent)
    open: (id) => {
      // a host agent, drawn as ITS RECORD says (`view`, else its kind's default,
      // else the stub) — the same path `place {backend_id}` takes
      const rec = registry.get(id);
      if (rec === undefined) return;
      const { handler, meta } = hostViewCell(id, rec);
      const viewId = `view_${crypto.randomUUID().slice(0, 8)}`;
      const w = centre();
      const ai = handler === aiView.handles[0];
      kernel.register(
        new Agent({
          id: viewId,
          parentId: containerNow(),
          handlerModule: handler,
          meta: { ...meta, x: Math.round(w.x), y: Math.round(w.y), width: ai ? 440 : 480, height: ai ? 520 : 360 } as unknown as Payload,
        }),
      );
      fresh.delete(id);
      setTimeout(() => moveTo(viewId), 400);
    },
    fresh: (id) => fresh.has(id),
    deleteAgent,
    reopen: (dlg) => void reopenAt(centre(), dlg),
    newChat: () => void spawnAiAt(centre(), null, ""),
  }, {
    canvases: opts.canvases,
    here: BACKEND,
    switchTo: (id: string) => {
      const u = new URL(location.href);
      u.searchParams.set("canvas", id);
      u.hash = "";
      location.href = u.toString();
    },
    newCanvas: async (name: string): Promise<string | null> => {
      const r = (await host.callHost(BACKEND, { type: "new_canvas", name } as unknown as Payload)) as Rec | null;
      if (typeof r?.["id"] === "string") return r["id"];
      toast(`✕ ${String(r?.["error"] ?? "new_canvas: no answer")}`);
      return null;
    },
  });
  let sidebarTimer: ReturnType<typeof setTimeout> | null = null;
  const renderChats = async (): Promise<void> => {
    if (sidebarTimer !== null) clearTimeout(sidebarTimer);
    sidebarTimer = null;
    await renderSidebars();
  };
  const renderSoon = (): void => {
    if (sidebarTimer === null) sidebarTimer = setTimeout(() => void renderChats(), 300);
  };

  // A SIDEBAR or the CANVAS LIST at the spot: page-only cells (no backend).
  const spawnPageCellAt = (w: { x: number; y: number }, handler: string, prefix: string, width: number, height: number): void => {
    kernel.register(
      new Agent({
        id: `${prefix}_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: handler,
        meta: { x: Math.round(w.x), y: Math.round(w.y), width, height },
      }),
    );
  };

  // THE MESSAGE STREAM at the spot: a page-only cell (no backend).
  const spawnStreamAt = (w: { x: number; y: number }): void => {
    kernel.register(
      new Agent({
        id: `stream_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: streamView.handles[0]!,
        meta: { x: Math.round(w.x), y: Math.round(w.y), width: 560, height: 320 },
      }),
    );
  };

  const spawnHtmlAt = (w: { x: number; y: number }): void => {
    kernel.register(
      new Agent({
        id: `html_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
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

  // THE DEMO (`src/tui/demo/web`): an html cell holding the packed demo page —
  // its own document in the panel's frame, so its hash and keys stay its own.
  const spawnDemoAt = (w: { x: number; y: number }): void => {
    const page = demoHtml();
    if (page === null) {
      console.error("canvas: this build carries no demo page (the bundle sets it; scripts/pack.sh packs it)");
      return;
    }
    kernel.register(
      new Agent({
        id: `demo_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: "html_agent.ts",
        meta: { x: Math.round(w.x), y: Math.round(w.y), width: 900, height: 560, display_name: "demo", html: page },
      }),
    );
  };

  // THE ACTION POPUP: a double click on empty canvas, or ⌘K / Ctrl-K at the pointer.
  let pointer = { x: window.innerWidth / 2, y: window.innerHeight / 2 };
  canvas.addEventListener("mousemove", (e) => {
    pointer = { x: e.clientX, y: e.clientY };
  });
  const spotAt = async (sx: number, sy: number): Promise<void> => {
    const w = screenToWorld(sx, sy);
    openActionPopup({
      screenX: sx,
      screenY: sy,
      presets: await presets(),
      onPick: (p: Pick): void => {
        if (p.kind === "terminal") void spawnTerminalAt(w);
        else if (p.kind === "html") spawnHtmlAt(w);
        else if (p.kind === "setup") openSetupAt(w);
        else if (p.kind === "stream") spawnStreamAt(w);
        else if (p.kind === "chat") void reopenAt(w, { id: p.id, name: p.name });
        else if (p.kind === "demo") spawnDemoAt(w);
        // a CODING AGENT: the human's own program, started in a terminal cell
        // (its exec agent runs EXACTLY that program, as `run COMMAND` does)
        else if (p.kind === "coding") void spawnPairAt(DEFAULT_VIEW, w, { width: 760, height: 480 }, { command: [p.path], display_name: p.name }, { allowed: [exactLine([p.path])] });
        else if (p.kind === "sidebar") spawnPageCellAt(w, SIDEBAR_VIEW, "sidebar", 320, 520);
        else if (p.kind === "canvases") spawnPageCellAt(w, CANVASES_VIEW, "canvases", 260, 300);
        else if (p.kind === "plugin") void spawnPluginAt(w, p.template);
        else if (p.kind === "bundle") void spawnPluginAt(w, null, p.starter);
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

  kernel.addStateSubscriber(renderSoon);
  // every page agent is documented as it appears (the page kernel's `docs`)
  documentPage(kernel);
  kernel.addStateSubscriber(() => documentPage(kernel));
  ensureBackground(ROOT_VIEW);
  if (isContainer(look, viewed)) ensureBackground(viewed);
  window.addEventListener("hashchange", () => {
    const want = viewFromHash(location.hash);
    if (want !== null && want !== viewed && kernel.get(want) !== undefined) setViewed(want, false);
  });
  await reconcile();
  await refresh();
  await renderChats();
}
