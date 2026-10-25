import { TIPS_HTML } from "../tips/tips_html.ts";
import { BACKGROUNDS, BACKGROUNDS_VERBS, grantsFor } from "../backgrounds/backgrounds_agent.ts";
import { backgroundsView } from "../backgrounds/backgrounds_view.ts";
import { DEFAULT_BG_SOURCE } from "./default_bg.ts";
import { documentPage } from "../verb_docs.ts";
import { Agent, Host } from "../../lib.ts";
import type { Json, Kernel, Payload } from "../../lib.ts";
import { CANVAS_CSS } from "./styles.ts";
import type { CanvasActions, ViewBundle, ViewHandle } from "../view.ts";
import { terminalView } from "../terminal_view/terminal_view.ts";
import { aiView } from "../ai_view/ai_view.ts";
import { setupView } from "../setup_view/setup_view.ts";
import { reflectView } from "../reflect_view/reflect_view.ts";
import { streamView } from "../stream_view/stream_view.ts";
import { CODING_AGENTS, exactLine, openActionPopup, tiles } from "./action_popup.ts";
import { SELECTOR, SELECTOR_PREFIX, setSelectorHost } from "./selector_agent.ts";
import { ROOT_VIEW, VIEW, VIEW_VERBS, grantViewLists, hostViewCell } from "./canvas_agent.ts";
import { containerOf, hashFor, isContainer, onMac, pathTo, viewFromHash } from "./viewer.ts";
import type { Lookup } from "./viewer.ts";
import { BACKGROUND, BACKGROUND_ID, bgCallableBy, viewToBg } from "./background_agent.ts";
import { SKIN_ID, SKIN_VERBS, ensureSkin } from "../skin/skin_agent.ts";
import { onSkin, skinNow, watchSkin } from "../skin/apply.ts";
import { EMPTY_HTML } from "../html_agent/empty_html.ts";

/** What a host agent (a cell's AI) may ask ANY cell: look at it, read and
 *  write a panel's html, emit to it. */
const CELL_VERBS = ["reflect", "get_html", "set_html", "emit"] as const;
/** What a cell's AI may ask the background. */
const BG_VERBS = ["set_bg", "default_bg", "clear_bg", "get_bg", "reflect"] as const;
import { MCP_ADD_VIEW, mcpAddView, mcpView, setMcpDeps } from "../mcp_view/mcp_view.ts";
import { canvasesView, setCanvasesDeps } from "../side_view/side_view.ts";
import { FILES_VERBS } from "../files/files_controller.ts";
import { filesView } from "../files/files_view.ts";
import { renderFileTrees, setFilesDeps } from "../files/files_deps.ts";
import { DRAG_MIME, boxCentre, decodeDrag, dropPlan, worldAt, zoomAbout } from "../files/drag.ts";
import { mountTreeSidebar } from "./tree_sidebar.ts";
import { fileGrant, onStateFrame } from "../files/file_bridge.ts";
import { PREVIEW_VERBS } from "../preview/preview_controller.ts";
import { previewView } from "../preview/preview_view.ts";
import type { Pick, Presets } from "./action_popup.ts";
import { htmlView } from "../html_agent/html_view.ts";
import { agentViewBundle } from "../agent_view/agent_view.ts";
import { headTitle, hostRecordFresh, seenHere, setCanvasNode, setHostRecords, setHostSync, sideOf, viewChangedFor } from "./host_views.ts";
import { createGlHost } from "./gl_host.ts";
import { COLUMN, LAYOUT, framesToRemove, hiddenCellFor, hiddenOf, layoutOf, minimizedOf, pinnedOf, placeMissing, prune, rectsOf, reorderY, setHidden, setMinimized, setPinned, setRect, showHidden, stack, themeOf, themesOf, writeRects } from "./layout.ts";
import { CELL_MENU, closeIntent, exportCall, exportTarget } from "./cell_menu.ts";
import type { Rect, Theme } from "./layout.ts";
import type { GlHost, GlView } from "./gl_host.ts";

// The canvas view-AGENT: the DOM-root compositor. Owns document.body, renders
// host member agents either INLINE (a TS view-agent for first-party content,
// keyed by the member's handler_module) or as an IFRAME (untrusted/external
// content that answers get_webapp). Translates pan/zoom/drag/dblclick into
// kernel sends. The kernel knows none of this.

// inline view registry — host handler_module → TS view bundle
const VIEW_BUNDLES: readonly ViewBundle[] = [terminalView, aiView, htmlView, agentViewBundle, setupView, reflectView, streamView, filesView, previewView, canvasesView, mcpView, mcpAddView, backgroundsView];
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

// THE VIEW A DOUBLE CLICK SPAWNS: a terminal. What it spawns on the host is the
// view's own declaration (`fronts`), looked up by that exact name in the host
// catalog — never matched, never guessed.
const DEFAULT_VIEW = terminalView;

// THE SLOT a dropped item takes around the drop point (#134): the largest cell the
// tree opens (a preview, a folder), so no two dropped cells overlap.
const DROP_SLOT = { width: 520, height: 560 };


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
  // the views may list the project to `open` what a drop names (#134, #135): granted from above, here —
  // every view, the root and each nested one, and each one placed later (an added agent)
  grantViewLists(kernel);
  kernel.addStateSubscriber((e) => {
    if (e.kind === "added") grantViewLists(kernel);
  });

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

  // DELETING a host agent is its own act (backend-owned views: ✕ only hides a
  // cell) — the kernel's `delete_agent`, whose delete-lock gate refuses what is
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
          ? { delete_lock: true, bg_source: DEFAULT_BG_SOURCE, callable_by: bgCallableBy() }
          : { bg_source: DEFAULT_BG_SOURCE, callable_by: bgCallableBy() },
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
  /** every agent on the host as the last sync saw it (another canvas's too) */
  const onHostIds = new Set<string>();
  /** host agents added in the background, not shown in any cell yet */
  const fresh = new Set<string>();
  /** host agents THIS page is creating (no NEW badge for them) */
  const placing = new Set<string>();
  setHostRecords((id) => registry.get(id));
  setHostSync(() => syncRegistry());
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
        onHostIds.clear();
        for (const id of byId.keys()) onHostIds.add(id);
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
    // a view the state stream reports removed releases the paths it serves
    onStateFrame(kernel, frame as Rec);
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
  // the label of an agent viewed full: "<name> · press Esc to return to the canvas"
  const fullHint = document.createElement("div");
  fullHint.className = "full-hint";
  fullHint.hidden = true;
  const fullName = document.createElement("b");
  fullHint.append(fullName, " · press ", Object.assign(document.createElement("kbd"), { textContent: "Esc" }), " to return to the canvas");
  canvas.appendChild(fullHint);

  // ─── view: zoom + pan (DOM transform on .canvas-world; GL camera locked) ──
  const view = { z: 1, ox: 0, oy: 0 };
  let targetZ = view.z;
  const ZOOM_MIN = 0.1;
  const ZOOM_MAX = 5;
  const ZOOM_LERP = 0.72;
  let glHost: GlHost | null = null;
  let placePinned = (): void => {}; // set once the frames exist (themes, below)
  // MOBILE'S CAMERA: the column fills the tab's width (a gutter for the bezels)
  // and only scrolls — from its top down to its last cell's bottom
  const MOBILE_GUTTER = 0; // flat, edge to edge (owner 2026-10-12)
  let columnBottom = 0; // the column's last cell's bottom, view coordinates
  const lockMobile = (): void => {
    if (!canvas.classList.contains("theme-mobile")) return;
    const w = canvas.clientWidth;
    const h = canvas.clientHeight;
    view.z = targetZ = (w - 2 * MOBILE_GUTTER) / COLUMN.width;
    view.ox = w / 2 - (COLUMN.x + COLUMN.width / 2) * view.z;
    // the pinned cell covers the screen's bottom: the column scrolls past it
    const pinH = pinnedId !== null ? Math.min(rectsNow()[pinnedId]?.height ?? 320, h * 0.5) * view.z + 16 : 0;
    const low = Math.min(0, h - pinH - (columnBottom + COLUMN.gap) * view.z);
    view.oy = Math.max(low, Math.min(0, view.oy));
  };
  const applyView = (): void => {
    lockMobile();
    // WHOLE PIXELS (owner 2026-10-13, the flicker): a fractional translate put
    // every cell's text and chrome between pixels, re-rasterized on each repaint
    world.style.transform = `translate(${Math.round(view.ox)}px, ${Math.round(view.oy)}px) scale(${view.z})`;
    glHost?.applyCamera();
    placePinned();
  };
  applyView();

  // The GL host (shared THREE renderer/scene) is created lazily on the first
  // GL member, so a canvas with no GL views never loads three.
  const ensureGlHost = async (): Promise<GlHost> => {
    if (glHost === null) {
      glHost = await createGlHost({
        bg,
        view,
        box: () => canvas.getBoundingClientRect(),
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
    // `auto` follows the width: crossing 768px switches the theme
    if (themeShown !== null && themeNow() !== themeShown) void refresh();
    // and the sidebar's place: beside the canvas on a wide screen, over it on a narrow one
    treeSidebar.setWide(innerWidth > 768);
  });

  // THE WORLD IS ITS OWN LAYER ONLY WHILE IT MOVES (owner 2026-10-09, the load
  // test): a permanent `will-change: transform` made the whole world one layer,
  // so ANY cell's change (a terminal's line) re-rasterized all of it. Promoted
  // for a pan / zoom, dropped once it rests — the text re-rasters sharp at the
  // new scale.
  let restTimer: ReturnType<typeof setTimeout> | undefined;
  const moving = (): void => {
    canvas.classList.add("moving");
    clearTimeout(restTimer);
    restTimer = setTimeout(() => canvas.classList.remove("moving"), 200);
  };
  // the zoom animates on frames only WHILE it zooms (no idle frame loop)
  let ticking = false;
  const tick = (): void => {
    ticking = false;
    if (view.z !== targetZ) {
      moving();
      // about the canvas's own middle: beside the sidebar that is not the viewport's
      const box = canvas.getBoundingClientRect();
      let nextZ = view.z + (targetZ - view.z) * ZOOM_LERP;
      if (Math.abs(targetZ - nextZ) < 0.0005) nextZ = targetZ;
      Object.assign(view, zoomAbout(view, { x: box.width / 2, y: box.height / 2 }, nextZ));
      applyView();
      zoomSoon();
    }
  };
  const zoomSoon = (): void => {
    if (ticking) return;
    ticking = true;
    requestAnimationFrame(tick);
  };

  canvas.addEventListener(
    "wheel",
    (e) => {
      // mobile: the wheel SCROLLS the column — anywhere, unless the content under
      // the pointer can still scroll that way itself
      if (canvas.classList.contains("theme-mobile")) {
        if (scrollsItself(e.target as Element, e.deltaY)) return;
        e.preventDefault();
        view.oy -= e.deltaY;
        applyView();
        return;
      }
      if ((e.target as Element).closest(".agent-frame")) return;
      e.preventDefault();
      const factor = e.deltaY > 0 ? 0.95 : 1.05;
      const next = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, targetZ * factor));
      // BACK TO 1 EXACTLY (owner 2026-10-13, the flicker): 0.95 and 1.05 never
      // cancel (1.05 × 0.95 = 0.9975), so a zoom out and in again rested at a
      // scale a hair off 1 — a step landing near 1, or crossing it, lands on it
      targetZ = Math.abs(next - 1) < 0.02 || (targetZ - 1) * (next - 1) < 0 ? 1 : next;
      zoomSoon();
    },
    { passive: false },
  );

  // mobile: a finger dragged anywhere scrolls the column (the same rule as the wheel)
  let touchY: number | null = null;
  canvas.addEventListener("touchstart", (e) => {
    // the reorder grip drags its cell, never the column
    touchY = e.touches.length === 1 && !(e.target as Element).closest(".grip") ? e.touches[0]!.clientY : null;
  }, { passive: true });
  canvas.addEventListener("touchmove", (e) => {
    if (touchY === null || !canvas.classList.contains("theme-mobile") || e.touches.length !== 1) return;
    const y = e.touches[0]!.clientY;
    const dy = y - touchY;
    touchY = y;
    if (scrollsItself(e.target as Element, -dy)) return;
    e.preventDefault();
    view.oy += dy;
    applyView();
  }, { passive: false });

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
        moving();
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

  // can the element under the pointer (or one of its boxes up to its cell's body)
  // still scroll by `dy` itself?
  const scrollsItself = (el: Element | null, dy: number): boolean => {
    for (let e = el; e !== null && e !== canvas; e = e.parentElement) {
      if (!(e instanceof HTMLElement)) continue;
      const oy = getComputedStyle(e).overflowY;
      if ((oy === "auto" || oy === "scroll") && e.scrollHeight > e.clientHeight) {
        if (dy > 0 ? e.scrollTop + e.clientHeight < e.scrollHeight - 1 : e.scrollTop > 0) return true;
      }
      if (e.classList.contains("agent-frame")) break;
    }
    return false;
  };

  const screenToWorld = (clientX: number, clientY: number): { x: number; y: number } => worldAt(clientX, clientY, canvas.getBoundingClientRect(), view);

  // ─── themes: where the cells sit (`layout.ts`) ────────────────────
  // A cell's rect is in its view's layout for the theme in use — never on the
  // cell. The theme: the view's `theme` (canvas | mobile), or auto by this
  // tab's width.
  const themeNow = (): Theme => themeOf(kernel, containerNow(), innerWidth);
  const rectsNow = (): Record<string, Rect> => rectsOf(kernel, containerNow(), themeNow());
  // THE ONE WAY THE CANVAS MAKES A CELL: the x/y/width/height it is made with go
  // to the layout of the theme in use (first — so it is not placed twice), the
  // cell itself is registered without them
  const registerCell = (a: Agent): void => {
    const g: Partial<Rect> = {};
    for (const k of ["x", "y", "width", "height"] as const) {
      const v = a.meta[k];
      if (typeof v === "number" && Number.isFinite(v)) g[k] = v;
      delete a.meta[k];
    }
    const v = a.parentId ?? containerNow();
    // the rect lands before the cell: a refresh the rect's write starts must not
    // prune it as a removed cell's (it read the size as 480×320 — measured)
    registering.add(a.id);
    try {
      if (Object.keys(g).length > 0) setRect(kernel, v, themeOf(kernel, v, innerWidth), a.id, g);
      kernel.register(a);
    } finally {
      registering.delete(a.id);
    }
  };
  const registering = new Set<string>(); // cells whose rect is written, the cell not yet

  // ─── frames ──────────────────────────────────────────────────────
  const frames = new Map<string, Frame>();

  const rectFromRec = (rec: Rec, defW: number, defH: number): { x: number; y: number; w: number; h: number } => {
    const r = rectsNow()[rec.id];
    return { x: r?.x ?? 0, y: r?.y ?? 0, w: r?.width ?? defW, h: r?.height ?? defH };
  };

  // Geometry is FRONTEND state — the layout agent's record, which the
  // proxy_loader debounce-persists to the host like any other
  const persistRect = (aid: string, x?: number, y?: number, w?: number, h?: number): void => {
    const patch: Partial<Rect> = {};
    if (x !== undefined) Object.assign(patch, { x, y: y ?? 0 });
    if (w !== undefined) Object.assign(patch, { width: w, height: h ?? 0 });
    setRect(kernel, containerNow(), themeNow(), aid, patch);
  };


  const startDrag = (e: MouseEvent, aid: string, el: HTMLElement): void => {
    e.preventDefault();
    if (el.classList.contains("pinned")) return; // held on the screen, not the world
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

  // MOBILE REORDER (owner 2026-10-12): the grip in the middle of a cell's head
  // drags it up or down the column (finger or mouse); dropped, it takes the slot
  // its centre is in — the column restacks by `y`
  const startReorder = (e: PointerEvent, aid: string, el: HTMLElement, grip: HTMLElement): void => {
    e.preventDefault();
    e.stopPropagation();
    if (el.classList.contains("pinned")) return;
    grip.setPointerCapture(e.pointerId);
    el.classList.add("dragging");
    document.body.classList.add("dragging-frame");
    const sy = e.clientY;
    const oy = parseFloat(el.style.top);
    const move = (ev: PointerEvent): void => {
      el.style.top = `${oy + (ev.clientY - sy) / view.z}px`;
    };
    const up = (): void => {
      grip.removeEventListener("pointermove", move);
      grip.removeEventListener("pointerup", up);
      grip.removeEventListener("pointercancel", up);
      el.classList.remove("dragging");
      document.body.classList.remove("dragging-frame");
      const rects = rectsNow();
      const me = rects[aid];
      if (me === undefined) return;
      const y = reorderY(rects, aid, parseFloat(el.style.top));
      if (y === me.y) {
        el.style.top = `${me.y}px`;
        return;
      }
      void persistRect(aid, me.x, y);
    };
    grip.addEventListener("pointermove", move);
    grip.addEventListener("pointerup", up);
    grip.addEventListener("pointercancel", up);
  };

  // RESIZE FROM ANY CORNER (owner 2026-10-11): the corner dragged moves, the
  // opposite one stays put
  const startResize = (e: MouseEvent, aid: string, el: HTMLElement, corner: string): void => {
    e.preventDefault();
    e.stopPropagation();
    if (el.classList.contains("pinned")) return;
    el.classList.add("resizing");
    document.body.classList.add("dragging-frame");
    const sx = e.clientX;
    const sy = e.clientY;
    const ox = parseFloat(el.style.left);
    const oy = parseFloat(el.style.top);
    const ow = parseFloat(el.style.width);
    const oh = parseFloat(el.style.height);
    const west = corner.includes("w");
    const north = corner.includes("n");
    const move = (ev: MouseEvent): void => {
      const dx = (ev.clientX - sx) / view.z;
      const dy = (ev.clientY - sy) / view.z;
      const w = Math.max(180, west ? ow - dx : ow + dx);
      const h = Math.max(120, north ? oh - dy : oh + dy);
      el.style.width = `${w}px`;
      el.style.height = `${h}px`;
      if (west) el.style.left = `${ox + ow - w}px`;
      if (north) el.style.top = `${oy + oh - h}px`;
    };
    const up = (): void => {
      el.classList.remove("resizing");
      document.body.classList.remove("dragging-frame");
      window.removeEventListener("mousemove", move);
      window.removeEventListener("mouseup", up);
      void persistRect(aid, parseFloat(el.style.left), parseFloat(el.style.top), parseFloat(el.style.width), parseFloat(el.style.height));
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
        <span class="id"></span>
        <span class="grip" title="drag to reorder">⠿</span>
        <span class="actions">
          <span class="pin" title="pin to the screen's bottom (mobile)">📌</span>
          <span class="lock ${locked ? "locked" : ""}" title="${locked ? "unlock (allow delete)" : "lock (refuse delete)"}">${locked ? "🔒" : "🔓"}</span>
          <span class="reload" title="reload">⟳</span>
          <span class="min" title="minimize to its bar (again: back)">▁</span>
          <span class="max" title="view full (again: back)">⤢</span>
          <span class="close" title="hide — the agent keeps running (Shift+×: delete it)">×</span>
        </span>
      </div>
      <div class="frame-body"></div>
      <div class="resize-handle se" data-corner="se"></div>
      <div class="resize-handle sw" data-corner="sw"></div>
      <div class="resize-handle ne" data-corner="ne"></div>
      <div class="resize-handle nw" data-corner="nw"></div>
    `;
    const name = headTitle(title, rec.id);
    const idEl = el.querySelector(".frame-head .id") as HTMLElement;
    idEl.textContent = name.id === null ? name.title : `${name.title} · `;
    if (name.id !== null) {
      const code = document.createElement("code");
      code.style.cssText = "opacity:0.6;font-size:10px";
      code.textContent = name.id;
      idEl.appendChild(code);
    }
    world.appendChild(el);
    const head = el.querySelector(".frame-head") as HTMLElement;
    head.addEventListener("mousedown", (e) => {
      if (e.button !== 0) return; // a right click opens the head's menu
      if ((e.target as Element).closest(".actions")) return;
      if (canvas.classList.contains("theme-mobile")) return; // mobile: the grip reorders
      startDrag(e, rec.id, el);
    });
    // MINIMIZE TO ITS BAR (owner 2026-10-12): a double click on the head; again: back
    const toggleMinimized = (): void => {
      const v = containerNow();
      const t = themeNow();
      setMinimized(kernel, v, t, rec.id, !minimizedOf(kernel, v, t).has(rec.id));
    };
    head.addEventListener("dblclick", (e) => {
      if ((e.target as Element).closest(".actions, .grip")) return;
      e.stopPropagation();
      toggleMinimized();
    });
    (el.querySelector(".min") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      toggleMinimized();
    });
    // VIEW FULL, as a click on it in the tree; viewed full, back to its view
    (el.querySelector(".max") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      setViewed(viewed === rec.id ? containerNow() : rec.id);
    });
    const grip = el.querySelector(":scope > .frame-head .grip") as HTMLElement;
    grip.addEventListener("pointerdown", (e) => startReorder(e, rec.id, el, grip));
    for (const h of el.querySelectorAll<HTMLElement>(":scope > .resize-handle")) {
      h.addEventListener("mousedown", (e) => startResize(e, rec.id, el, h.dataset.corner ?? "se"));
    }
    (el.querySelector(".reload") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      onReload();
    });
    (el.querySelector(".pin") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      const v = containerNow();
      const t = themeNow();
      setPinned(kernel, v, t, pinnedOf(kernel, v, t) === rec.id ? null : rec.id);
    });
    (el.querySelector(".lock") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      const cur = bool(frames.get(rec.id)?.rec ?? rec, "delete_lock");
      const lockEl = el.querySelector(".lock") as HTMLElement;
      lockEl.classList.toggle("locked", !cur);
      lockEl.textContent = !cur ? "🔒" : "🔓";
      kernel.updateMeta(rec.id, { delete_lock: !cur }); // local; proxy_loader syncs
    });
    // ✕ HIDES the cell; Shift+✕ deletes it (`cell_menu.ts`)
    (el.querySelector(".close") as HTMLElement).addEventListener("click", (e) => {
      e.stopPropagation();
      if (closeIntent(e.shiftKey) === "delete") deleteCell(rec.id);
      else hideCell(rec.id);
    });
    head.addEventListener("contextmenu", (e) => {
      e.preventDefault();
      openCellMenu(rec.id, e.clientX, e.clientY);
    });
    return {
      el,
      body: el.querySelector(".frame-body") as HTMLElement,
      actions: el.querySelector(".actions") as HTMLElement,
    };
  };

  // REMOVE a cell from its view; the agent behind it, if any, is untouched. It is
  // every mounted bundle's `close` (a terminal whose job ended), the wizard's, and
  // ✕'s delete for a page-only cell (#133: Shift+✕ on a cell with a backend
  // deletes the agent instead). A delete_lock refuses (the kernel's `remove` is
  // the gate).
  const closeView = (cellId: string): void => {
    const out = kernel.remove(cellId) as unknown as Rec;
    if (out["deleted"] !== true) toast(`✕ ${typeof out["error"] === "string" ? out["error"] : `could not close ${cellId}`}`);
    renderSoon();
  };

  // HIDE a cell (#133): off its view, its agent running on — its record and rect
  // kept, its frame (and stream) torn down by the next draw; `show` brings it back
  const hideCell = (cellId: string): void => {
    const view = kernel.get(cellId)?.parentId ?? containerNow();
    if (viewed === cellId) setViewed(view);
    setHidden(kernel, view, cellId, true);
    renderSoon();
  };

  // EXPORT a cell's agent and everything under it as a blueprint folder (#268):
  // the head's menu asks for the folder, `user` writes it
  const exportCell = async (cellId: string): Promise<void> => {
    const a = kernel.get(cellId);
    if (a === undefined) return;
    const agent = exportTarget(a.record() as Rec);
    if (agent === undefined) {
      toast(`export: ${cellId} has no host agent`);
      return;
    }
    const path = prompt("Export to folder", `export_${agent}`)?.trim();
    if (path === undefined || path === "") return;
    const r = (await host.callHost("user", exportCall(agent, path) as unknown as Payload)) as Rec | undefined;
    if (typeof r?.["error"] === "string") toast(`✕ ${r["error"]}`);
    else toast(`exported ${String(r?.["exported"] ?? 0)} agents to ${path}`);
  };

  // A PICK of a hidden cell (the file tree's, #133) shows it where it was first, so
  // the pick views it live rather than bouncing back to its view
  const showPicked = (cellId: string): void => {
    if (showHidden(kernel, cellId)) renderSoon();
  };

  // DELETE a cell's agent (#133: Shift+✕ and the head's menu): a host-backed
  // cell deletes its host agent through the file tree's delete path (the host's
  // lock gate refuses what is locked); a page-only cell is removed. A cell's own
  // delete_lock refuses both, said.
  const deleteCell = (cellId: string): void => {
    const a = kernel.get(cellId);
    if (a === undefined) return;
    const rec = a.record() as Rec;
    if (bool(rec, "delete_lock")) {
      toast(`✕ ${cellId} is locked — unlock it to delete`);
      return;
    }
    const agent = str(rec, "backend_id");
    if (agent !== undefined) deleteAgent(agent);
    else closeView(cellId);
  };

  // THE HEAD'S MENU (right-click, #133): one at a time, closed by a pick, a click
  // outside it or Esc
  let cellMenuEl: HTMLElement | null = null;
  const closeCellMenu = (): void => {
    cellMenuEl?.remove();
    cellMenuEl = null;
  };
  const openCellMenu = (cellId: string, x: number, y: number): void => {
    closeCellMenu();
    const menu = document.createElement("div");
    menu.className = "cell-menu";
    menu.setAttribute("role", "menu");
    for (const item of CELL_MENU) {
      const b = document.createElement("button");
      b.type = "button";
      b.setAttribute("role", "menuitem");
      b.dataset["intent"] = item.intent;
      b.textContent = item.label;
      b.title = item.title;
      b.addEventListener("click", (e) => {
        e.stopPropagation();
        closeCellMenu();
        if (item.intent === "hide") hideCell(cellId);
        else if (item.intent === "export") void exportCell(cellId);
        else deleteCell(cellId);
      });
      menu.appendChild(b);
    }
    document.body.appendChild(menu);
    // inside the tab: a menu opened past the edge is pulled back in
    const box = menu.getBoundingClientRect();
    menu.style.left = `${Math.max(0, Math.min(x, innerWidth - box.width - 4))}px`;
    menu.style.top = `${Math.max(0, Math.min(y, innerHeight - box.height - 4))}px`;
    cellMenuEl = menu;
  };
  window.addEventListener("pointerdown", (e) => {
    if (cellMenuEl !== null && !cellMenuEl.contains(e.target as Node)) closeCellMenu();
  });

  // ── AI CELLS ─────────────────────────────────────────────────────────
  // `canvas_root` (`kern/backend`) spawns the cell's AI from one of the user's
  // saved set-ups (only the CLI reads those), and answers the saved set-ups and the
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
      preferred: (r["preferred"] as string | null) ?? null,
      dialogs: ((d?.["dialogs"] as Presets["dialogs"] | undefined) ?? []).map((x) => ({ id: x.id, name: x.name })),
      coding: await codingAgents(),
      bundles: (r["bundles"] as Presets["bundles"]) ?? [],
      system: (r["system"] as string[] | undefined) ?? [],
      // this canvas's page bundles: folder rows with a base, loaded (not refused)
      pageBundles: kernel
        .availableBundles()
        .map((b) => b as unknown as Record<string, unknown>)
        .filter((b) => typeof b["base"] === "string" && b["error"] === undefined)
        .map((b) => ({ name: String(b["name"]), sentence: typeof b["sentence"] === "string" ? b["sentence"] : "" })),
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
      // the page's skin (`skin_agent.ts`): an AI writes a skin and switches to it
      [`${kernel.guid}|${SKIN_ID}`]: [...SKIN_VERBS],
      // any cell — and the view verbs, since the cells are their OPERANDS
      // (`move {id}`, `wire {sender, receiver}`): the host's egress gate checks
      // the agents a call names, not only its target
      // (the file tree's and the previewer's verbs too: an AI shows a file, edits it)
      [`${kernel.guid}|*`]: [...new Set([...CELL_VERBS, ...VIEW_VERBS, ...BACKGROUNDS_VERBS, ...SKIN_VERBS, ...FILES_VERBS, ...PREVIEW_VERBS])],
    };
  };
  const spawnAi = async (cellId: string, saved: string | null, view?: string): Promise<Rec | null> => {
    const r = (await host.callHost(BACKEND, {
      type: "spawn",
      stamp: { cell_id: cellId },
      peer_grants: peerGrants(view ?? containerOf(look, cellId) ?? ROOT_VIEW),
      ...(saved ? { saved } : {}),
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
      unconfigured: false,
      // every dialog is kept from its start (no ephemeral chats)
      kept: true,
      ...(typeof r["dialog_name"] === "string" ? { dialog_name: r["dialog_name"] } : {}),
      saved: r["saved"],
      can_call: { ...aiView.grants(String(r["id"])) },
    } as unknown as Payload);
  };

  // Mount a first-party TS view-agent inline into a fresh frame.
  const mountInline = async (rec: Rec, bundle: ViewBundle, idx: number): Promise<void> => {
    const rect = rectFromRec(rec, 480, 320);
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
    if (bundle === aiView && bool(rec, "unconfigured") && str(rec, "backend_id") === undefined) {
      // AN AI CELL NOT SET UP YET (the wizard's skip): how to set it up
      body.appendChild(notSetUp(rec.id));
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
          void spawnAi(rec.id, str(rec, "saved") ?? null).then((r) => {
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
    const handle = await bundle.mount({ kernel: asView, mount: body, selfId: rec.id, backend, record: rec, close, actions: canvasActions });
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
      // the record may have been refreshed since this closure was made (#222)
      const live = frames.get(rec.id)?.rec ?? rec;
      void Promise.resolve(bundle.mount({ kernel: asView, mount: body, selfId: rec.id, backend, record: live, close, actions: canvasActions })).then((h) => {
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
    const rect = rectFromRec(rec, wa.default_width ?? 340, wa.default_height ?? 240);
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

  const applyRect = (el: HTMLElement, r: Rect | undefined): void => {
    if (r === undefined) return;
    el.style.left = `${r.x}px`;
    el.style.top = `${r.y}px`;
    el.style.width = `${r.width}px`;
    el.style.height = `${r.height}px`;
  };

  let bgShown = ""; // the background source the GL host shows now
  let themeShown: Theme | null = null; // the theme this tab last drew
  let pinnedId: string | null = null; // mobile: the cell held on the screen
  // the pinned cell stays in the world (an iframe moved in the DOM would reload)
  // but is placed against the camera: full width, on the screen's bottom, at
  // screen scale, half the screen at most
  placePinned = (): void => {
    const f = pinnedId !== null ? frames.get(pinnedId) : undefined;
    if (f === undefined) return;
    const box = canvas.getBoundingClientRect();
    const h = Math.min(rectsNow()[f.rec.id]?.height ?? 320, box.height * 0.5);
    f.el.style.transformOrigin = "0 0";
    f.el.style.transform = `scale(${1 / view.z})`;
    f.el.style.left = `${(8 - view.ox) / view.z}px`;
    f.el.style.top = `${(box.height - h - 8 - view.oy) / view.z}px`;
    f.el.style.width = `${box.width - 16}px`;
    f.el.style.height = `${h}px`;
  };
  // LEAVING A FULL VIEW (owner 2026-10-11: "agent → canvas: for a moment the
  // agents show over the old background"): 1) the full agent goes (the world is
  // hidden at once), 2) the view's background mounts and draws its first frame,
  // 3) the cells mount — the world shows again only then, all at once
  let fullShown = false; // the full state the last refresh drew
  let refreshSeq = 0;
  let hiddenBy = 0; // the refresh that hid the world, 0: shown
  const refresh = async (): Promise<void> => {
    const my = ++refreshSeq;
    try {
      await refreshView(my);
    } finally {
      if (hiddenBy === my) {
        hiddenBy = 0;
        world.style.visibility = "";
      }
    }
  };
  const refreshView = async (my: number): Promise<void> => {
    // Members are this view's OWN children in the LOCAL tree (hydrated from
    // web/kernel_state, persisted back via proxy_loader). No host membership read.
    // THE VIEWED: a view's own children on its world, or one agent FULL.
    if (kernel.get(viewed) === undefined) viewed = ROOT_VIEW; // removed meanwhile
    // a cell hidden while it was viewed full: back to its view (#133)
    const viewedView = kernel.get(viewed)?.parentId ?? null;
    if (viewedView !== null && hiddenOf(kernel, viewedView).has(viewed)) viewed = viewedView;
    const full = fullNow();
    if (fullShown && !full) {
      hiddenBy = my;
      world.style.visibility = "hidden";
      canvas.classList.remove("full-view");
      fullHint.hidden = true;
      for (const f of frames.values()) {
        f.el.classList.remove("full");
        const max = f.el.querySelector(":scope > .frame-head .max");
        if (max) max.textContent = "⤢";
      }
    }
    fullShown = full;
    const cont = kernel.get(containerNow());
    const all: Rec[] = cont !== undefined ? [...cont.children.values()].map((a) => a.record() as Rec) : [];
    // the cells HIDDEN from this view (#133): not drawn, their rects kept
    const hid = hiddenOf(kernel, containerNow());
    // the background is drawn behind everything, not framed
    // (a selector is the popup on top, never a cell)
    // a background viewed full is the SAME background path (swapped in place on
    // set_bg), not a member: nothing is torn down when Esc goes back
    const viewedRec = full ? (kernel.get(viewed)!.record() as Rec) : undefined;
    const viewingBg = viewedRec?.["handler_module"] === BACKGROUND;
    const isCell = (r: Rec): boolean => r["handler_module"] !== BACKGROUND && r["handler_module"] !== SELECTOR && r["handler_module"] !== LAYOUT;
    const members = viewedRec !== undefined ? (viewingBg ? [] : [viewedRec]) : all.filter((r) => isCell(r) && !hid.has(r.id));
    const bgRec = viewingBg ? viewedRec : full ? undefined : all.find((r) => r["handler_module"] === BACKGROUND);

    // THE LAYOUT OF THE THEME IN USE: a removed cell's rects go; a cell with no
    // rect in this theme is placed by it — canvas: free space around the centre
    // of what this tab shows; mobile: the column's bottom (`layout.ts`)
    const theme = themeNow();
    const column = layoutOf(kernel, containerNow(), theme) === "column";
    const cells = all.filter(isCell);
    let rects = rectsNow();
    if (cont !== undefined) {
      prune(kernel, cont.id, new Set([...cells.map((c) => c.id), ...registering]));
      const other = themesOf(kernel, cont.id).map((d) => d.name).filter((t) => t !== theme).map((t) => rectsOf(kernel, cont.id, t));
      const sized = cells.map((c) => {
        const o = other.map((m) => m[c.id]).find((x) => x !== undefined);
        return { id: c.id, size: { width: o?.width ?? 480, height: o?.height ?? 320 } };
      });
      const box = canvas.getBoundingClientRect();
      rects = placeMissing(kernel, cont.id, theme, sized, screenToWorld(box.left + box.width / 2, box.top + box.height / 2));
      // a COLUMN theme: always ONE full-width column, cells in their order
      if (column) {
        const mins = minimizedOf(kernel, cont.id, theme);
        // the column holds the cells shown; a hidden one keeps its rect, off it
        const shown = Object.fromEntries(Object.entries(rects).filter(([id]) => !hid.has(id)));
        const col = { ...rects, ...stack(shown, mins) };
        if (JSON.stringify(col) !== JSON.stringify(rects)) writeRects(kernel, cont.id, theme, col);
        rects = col;
        columnBottom = Object.entries(col).reduce((m, [id, r]) => (hid.has(id) ? m : Math.max(m, r.y + (mins.has(id) ? 0 : r.height))), 0);
      }
    }
    // a tab that turns mobile looks at the column (its top, centred)
    if (theme !== themeShown) {
      themeShown = theme;
      if (column) {
        view.oy = 0;
        canvas.classList.add("theme-mobile");
        applyView();
      }
    }
    let idx = 0;
    const seenDom = new Set<string>();
    const seenGl = new Set<string>();
    // THE BACKGROUND FIRST (owner 2026-10-09): the view's singleton background
    // agent's `bg_source`, mounted as a GL view under its id, behind every member,
    // swapped in place — BEFORE the cells mount, so leaving a full view (Esc) never
    // shows the cells over the old background while they mount.
    // ONLY UNDER A WEB SKIN: a native backdrop shows the window's own material,
    // so the view's background is not drawn (its record untouched — a web skin
    // brings it back); `#bg[data-background]` says which, for probes
    const drawBg = skinNow().skin.backdrop === "web";
    bg.dataset["background"] = drawBg ? "drawn" : "none";
    const bgSource = drawBg && bgRec !== undefined && typeof bgRec["bg_source"] === "string" ? (bgRec["bg_source"] as string) : "";
    if (bgSource !== "" && bgRec !== undefined) {
      const gh = await ensureGlHost();
      if (bgSource !== bgShown) {
        gh.removeView(bgRec.id);
        bgShown = bgSource;
      }
      if (!gh.hasView(bgRec.id)) gh.installView(bgRec.id, { source: bgSource });
      seenGl.add(bgRec.id);
      // leaving a full view: the background is ON SCREEN before any cell is
      if (hiddenBy === my) await gh.rendered();
    } else {
      bgShown = "";
    }
    for (const rec of members) {
      const mid = rec.id;
      // A REFRESH'S MEMBERS ARE A SNAPSHOT: one removed while this refresh awaited
      // an earlier mount is not mounted again (a newer refresh already took its
      // frame down)
      if (kernel.get(mid) === undefined) continue;
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
        applyRect(existing.el, rects[mid]);
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
    // only the NEWEST refresh sweeps: this one's members are a snapshot, and a frame a newer
    // refresh mounted meanwhile (a second cell shown by one drop) is not in them
    for (const aid of framesToRemove(frames.keys(), seenDom, (id) => kernel.get(id) !== undefined, my === refreshSeq)) removeFrame(aid);
    // MOBILE'S PIN: the pinned cell is held on the screen's bottom
    canvas.classList.toggle("theme-mobile", column && !full);
    applyView();
    pinnedId = column && !full ? pinnedOf(kernel, containerNow(), theme) : null;
    if (pinnedId !== null && hid.has(pinnedId)) pinnedId = null; // a hidden cell holds no screen bottom
    for (const [id, f] of frames) {
      const on = id === pinnedId;
      if (!on && f.el.classList.contains("pinned")) {
        f.el.classList.remove("pinned");
        f.el.style.transform = "";
        applyRect(f.el, rects[id]);
      }
      f.el.classList.toggle("pinned", on);
      f.el.querySelector(":scope > .frame-head .pin")?.classList.toggle("on", on);
    }
    placePinned();
    const mins = cont !== undefined && !full ? minimizedOf(kernel, cont.id, theme) : new Set<string>();
    for (const [id, f] of frames) f.el.classList.toggle("minimized", mins.has(id));
    glHost?.prune(seenGl);
    canvas.classList.toggle("full-view", full);
    for (const [id, f] of frames) {
      f.el.classList.toggle("full", full && id === viewed);
      const max = f.el.querySelector(":scope > .frame-head .max");
      if (max) max.textContent = full && id === viewed ? "⤡" : "⤢";
    }
    // VIEWED FULL (owner 2026-10-11): its name on the middle of its top border,
    // and the way back
    fullHint.hidden = !full;
    if (full) {
      const r = kernel.get(viewed)?.record() as Rec | undefined;
      // its name: the head's, else a dialog's renamed name (an AI cell — its default
      // is its AI's id), else its saved set-up's, else its id
      const dn = r !== undefined ? str(r, "dialog_name") : undefined;
      const renamed = dn !== undefined && dn !== (r !== undefined ? str(r, "backend_id") : undefined) ? dn : undefined;
      fullName.textContent = (r !== undefined ? str(r, "display_name") ?? renamed ?? str(r, "saved") : undefined) ?? viewed;
    }
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
    // VIEWS it. The cell owns nothing: ✕ hides the cell, the agent lives on.
    const cellId = `cell_${crypto.randomUUID().slice(0, 8)}`;
    const backendId = await spawnBackend(bundle, cellId, backendFields);
    if (backendId === null) return;
    placing.add(backendId);
    registerCell(
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
    // a hidden cell comes back first; the frame is drawn by the next render
    if (showHidden(kernel, cellId)) {
      void refresh().then(() => moveTo(cellId));
      return;
    }
    // FOCUS: a cell on another view (or viewed full) — view its view first
    const hc = hostCanvasOf(cellId);
    if (hc !== null && (hc !== viewed || fullNow())) {
      setViewed(hc);
      void refresh().then(() => moveTo(cellId));
      return;
    }
    const f = frames.get(cellId);
    if (!f) return;
    const r = rectFromRec(f.rec, 480, 320);
    const rect = canvas.getBoundingClientRect();
    view.ox = rect.width / 2 - (r.x + r.w / 2) * view.z;
    view.oy = rect.height / 2 - (r.y + r.h / 2) * view.z;
    applyView();
    f.el.classList.add("flash");
    setTimeout(() => f.el.classList.remove("flash"), 900);
  };

  // THE AI SET-UP WIZARD — one per view: opening it again moves to it; opened
  // FOR an AI cell (its settings, or one not set up yet) it replaces the open one.
  const SETUP_SIZE = { width: 900, height: 640 };
  const openSetupAt = (w: { x: number; y: number }, extra: Record<string, unknown> = {}): void => {
    const c = kernel.get(containerNow());
    const open = c ? [...c.children.values()].find((a) => a.record().handler_module === setupView.handles[0]) : undefined;
    if (open && Object.keys(extra).length === 0) return moveTo(open.id);
    if (open) closeView(open.id);
    void spawnPairAt(setupView, w, SETUP_SIZE, extra);
  };
  // where a cell sits (its rect in the theme in use), or the screen's centre
  const spotOf = (cellId: string): { x: number; y: number; w: number; h: number } => {
    const r = rectsNow()[cellId];
    if (r !== undefined) return { x: r.x, y: r.y, w: r.width, h: r.height };
    const c = centre();
    return { x: c.x, y: c.y, w: 0, h: 0 };
  };
  // THE CANVAS'S MOVES a view may ask for (the wizard, an AI cell)
  const canvasActions: CanvasActions = {
    async spawnAi(saved, from, cell) {
      if (cell !== undefined && kernel.get(cell) !== undefined) {
        const r = await spawnAi(cell, saved);
        if (r === null) return;
        removeFrame(cell);
        bindAi(cell, r);
      } else {
        const at = spotOf(from);
        if ((await spawnAiIn(containerOf(look, from) ?? containerNow(), at, saved, "")) === null) return;
      }
      closeView(from);
    },
    skipAi(from) {
      const at = spotOf(from);
      registerCell(
        new Agent({
          id: `ai_view_${crypto.randomUUID().slice(0, 8)}`,
          parentId: containerOf(look, from) ?? containerNow(),
          handlerModule: aiView.handles[0]!,
          meta: { owns_backend: false, unconfigured: true, x: Math.round(at.x), y: Math.round(at.y), width: 440, height: 360 },
        }),
      );
      closeView(from);
    },
    openSetup(cell, ai) {
      const at = cell !== undefined ? spotOf(cell) : { ...centre(), w: 0, h: 0 };
      openSetupAt({ x: at.x + at.w + 40, y: at.y }, { ...(cell !== undefined ? { target_cell: cell } : {}), ...(ai !== undefined ? { edit_ai: ai } : {}) });
    },
    // a chat card's ⤢ (#339): the agent's own cell full screen — the one that shows it, else a new one
    viewFull(agent) {
      const shows = everyAgent().find((id) => (kernel.get(id)?.record() as Rec | undefined)?.["backend_id"] === agent);
      if (shows !== undefined) {
        showPicked(shows);
        return setViewed(shows);
      }
      const rec = registry.get(agent);
      if (rec === undefined) return toast(`✕ no agent ${agent}`);
      const { handler, meta } = hostViewCell(agent, rec);
      const viewId = `view_${crypto.randomUUID().slice(0, 8)}`;
      const w = centre();
      registerCell(new Agent({ id: viewId, parentId: containerNow(), handlerModule: handler, meta: { ...meta, x: Math.round(w.x - 240), y: Math.round(w.y - 180), width: 480, height: 360 } as unknown as Payload }));
      setViewed(viewId);
    },
    deleteAgent,
    toast,
    async startCoding(near) {
      const found = (await codingAgents()).filter((a) => a.name !== "fantastic"); // a coding AGENT to help — not the shell itself
      const first = found[0];
      if (first === undefined) return toast("no coding agent found on your PATH (claude, codex, aider or opencode)");
      const at = spotOf(near);
      await spawnPairAt(DEFAULT_VIEW, { x: at.x + at.w + 40, y: at.y }, { width: 760, height: 480 }, { command: [first.path], display_name: first.name }, { allowed: [exactLine([first.path])] });
    },
  };
  // AN AI CELL NOT SET UP YET: what it needs, and the two ways to get there
  const notSetUp = (cellId: string): HTMLElement => {
    const box = document.createElement("div");
    box.className = "ai-not-set-up";
    box.innerHTML = `<div class="nsu-title">⚙ Not set up yet</div>
      <p>Press settings to set me up, or start your coding AI agent here to help set me up.</p>
      <div class="nsu-row"><button class="nsu-settings">⚙ settings</button><button class="nsu-coding">▸ coding agent</button></div>`;
    (box.querySelector(".nsu-settings") as HTMLButtonElement).onclick = (): void => canvasActions.openSetup(cellId);
    (box.querySelector(".nsu-coding") as HTMLButtonElement).onclick = (): void => void canvasActions.startCoding(cellId);
    return box;
  };

  // An AI cell at the spot, its first turn what was typed. The same order as a
  // terminal: the cell lands on disk first, then its backend, then the bind.
  const spawnAiAt = async (w: { x: number; y: number }, saved: string | null, text: string): Promise<void> => {
    await spawnAiIn(containerNow(), w, saved, text);
  };
  // An AI cell on view `parent`: the cell, then its backend. Its id, or null
  // (the refusal toasted, the cell removed).
  const spawnAiIn = async (parent: string, w: { x: number; y: number }, saved: string | null, text: string): Promise<string | null> => {
    // HOST-FIRST: the AI is made (stamped with the cell it will show in), then
    // the cell views it
    const viewId = `ai_view_${crypto.randomUUID().slice(0, 8)}`;
    const x = Math.round(w.x);
    const y = Math.round(w.y);
    const r = await spawnAi(viewId, saved, parent);
    if (r === null) return null;
    placing.add(String(r["id"]));
    registerCell(
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

  // A BUNDLE'S AGENT placed (backend-owned views): `canvas_root spawn` makes the
  // host agent — from a NEW project bundle folder the host scaffolds (`starter`:
  // `spawn {bundle}`) or one more instance of a project bundle (`instance_of`) —
  // then a cell VIEWS it, drawn as the agent's own record says, like a terminal.
  const spawnBundleAt = async (w: { x: number; y: number }, what: { bundle: string } | { instance_of: string }): Promise<void> => {
    const cellId = `view_${crypto.randomUUID().slice(0, 8)}`;
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
    registerCell(
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
    // a HIDDEN cell already on this dialog (#133) is the dialog's cell again: it
    // is re-bound (the host re-makes the AI if it is gone) and shown where it was,
    // rather than a second cell beside it
    const hiddenCell = hiddenCellFor(kernel, dialog.id, everyAgent());
    const viewId = hiddenCell ?? `ai_view_${crypto.randomUUID().slice(0, 8)}`;
    if (hiddenCell === undefined) {
      registerCell(
        new Agent({
          id: viewId,
          parentId: containerNow(),
          handlerModule: aiView.handles[0]!,
          meta: { owns_backend: false, kept: true, dialog_name: dialog.name, x: Math.round(w.x), y: Math.round(w.y), width: 440, height: 520 },
        }),
      );
      await flush();
    }
    const r = (await host.callHost(BACKEND, {
      type: "reopen",
      id: dialog.id,
      stamp: { cell_id: viewId },
      peer_grants: peerGrants(containerOf(look, viewId) ?? ROOT_VIEW),
    } as unknown as Payload)) as Rec;
    if (typeof r["error"] === "string") {
      toast(`✕ ${r["error"]}`);
      // a new cell is withdrawn; the hidden one stays hidden
      if (hiddenCell === undefined) kernel.remove(viewId);
      return;
    }
    bindAi(viewId, r);
    if (hiddenCell !== undefined) showPicked(viewId);
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
  // ── THE FILE TREE (`files/files_view.ts`): the project's files, every agent a folder ──
  const centre = (): { x: number; y: number } => {
    const mid = boxCentre(canvas.getBoundingClientRect());
    return screenToWorld(mid.x, mid.y);
  };
  // every AI cell on every view of this page
  const aiCells = (): Rec[] =>
    everyAgent()
      .map((id) => kernel.get(id)!.record() as Rec)
      .filter((r) => r["handler_module"] === aiView.handles[0]);
  // every agent of the page's own kernel, as a record (the root first)
  const pageAgents = (): Rec[] => everyAgent().map((id) => kernel.get(id)!.record() as Rec);
  // AN MCP SERVER (`mcp_view.ts`): the form adds it on this canvas's node, then
  // the agent's own view takes the form's place
  setMcpDeps({
    add: async (url, key) => {
      const r = (await host.callHost(BACKEND, { type: "add_mcp", url, ...(key ? { key } : {}) } as unknown as Payload)) as Rec | null;
      if (typeof r?.["id"] === "string") {
        placing.add(r["id"]);
        await syncRegistry();
        return { id: r["id"] };
      }
      return { error: String(r?.["error"] ?? "add_mcp: no answer") };
    },
    show: (id, cell) => {
      const at = rectsNow()[cell];
      const rec = registry.get(id);
      if (rec === undefined) return;
      const { handler, meta } = hostViewCell(id, rec);
      kernel.remove(cell);
      registerCell(new Agent({
        id: `view_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: handler,
        meta: { ...meta, x: at?.x ?? 0, y: at?.y ?? 0, width: 460, height: 420 } as unknown as Payload,
      }));
    },
  });
  // A PAGE AGENT as a card, for the previewer's agent mode: its record, read-only,
  // its verbs, and the two ways to go to it (what a click and a ⌘-click did in the tree)
  const pageReflect = (a: Agent, el: HTMLElement): ViewHandle => {
    const card = document.createElement("div");
    card.className = "reflect";
    const h = document.createElement("h3");
    h.textContent = a.id;
    const rec = document.createElement("pre");
    rec.textContent = JSON.stringify(a.record(), null, 2);
    card.append(h, rec);
    const verbs = Object.entries(a.verbs ?? {});
    if (verbs.length > 0) {
      const v = document.createElement("pre");
      v.textContent = verbs.map(([k, doc]) => `${k} — ${doc}`).join("\n");
      card.appendChild(v);
    }
    const acts = document.createElement("div");
    acts.className = "acts";
    for (const [label, focus] of [["view it", false], ["focus it", true]] as const) {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "pv-chip";
      b.textContent = label;
      b.addEventListener("click", () => (focus && a.id !== rootOr() ? moveTo(a.id) : setViewed(a.id)));
      acts.appendChild(b);
    }
    card.appendChild(acts);
    el.appendChild(card);
    return { unmount: (): void => card.remove() };
  };
  // THE FILE TREE and PREVIEW cells (`files_view.ts`, `preview_view.ts`) and the
  // CANVAS-LIST cell (`side_view.ts`) read the canvas through this wiring; none is
  // made by default — the double-click places them
  setFilesDeps({
    // this canvas's host agents (the registry: its subtree + the shell's own)
    hostAgents: async () => {
      await syncRegistry();
      return [...registry.values()];
    },
    hostRecords: () => [...registry.values()],
    pageAgents,
    seenHere: (id) => registry.has(id) || kernel.get(id) !== undefined,
    onHost: (id) => onHostIds.has(id),
    freshIds: () => [...fresh],
    // agent `agentId`'s own live view inside `el`: a host agent through its own
    // view bundle (as a cell would draw it), a page agent as a reflect card
    mountAgentView: async (agentId, el, selfCell) => {
      const local = kernel.get(agentId);
      if (local !== undefined) return pageReflect(local, el);
      const rec = registry.get(agentId) ?? (await hostRecordFresh(agentId));
      if (rec === undefined) throw new Error(`no agent ${agentId}`);
      const { handler, meta } = hostViewCell(agentId, rec);
      const bundle = viewFor(handler, kernel);
      if (bundle === undefined) throw new Error(`no view for ${handler}`);
      return bundle.mount({
        kernel,
        mount: el,
        selfId: selfCell,
        backend: agentId,
        record: { id: selfCell, handler_module: handler, ...meta },
        close: () => {},
        actions: canvasActions,
      });
    },
    // the cell itself, or the cells fronting it (its backend, or its root)
    cellsFor: (id) => {
      if (kernel.get(id) !== undefined && id !== rootOr()) return [id];
      return everyAgent()
        .map((a) => kernel.get(a)!.record() as Rec)
        .filter((r) => r["backend_id"] === id || r["backend_root"] === id)
        .map((r) => String(r["id"]));
    },
    // whether a cell is off its view (#133): its agent runs, its frame is not drawn
    isHidden: (cell) => {
      const view = kernel.get(cell)?.parentId;
      return typeof view === "string" && hiddenOf(kernel, view).has(cell);
    },
    moveTo,
    view: (id, focus) => {
      // a focus pick goes through `moveTo`, which shows a hidden cell itself
      if (focus && id !== rootOr()) return moveTo(id);
      showPicked(id);
      return setViewed(id);
    },
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
      // CENTRED on what the person is looking at, and the camera stays put
      // (owner 2026-10-12: no refocus after a tree click spawns a cell)
      const w = centre();
      const ai = handler === aiView.handles[0];
      const [width, height] = ai ? [440, 520] : [480, 360];
      registerCell(
        new Agent({
          id: viewId,
          parentId: containerNow(),
          handlerModule: handler,
          meta: { ...meta, x: Math.round(w.x - width / 2), y: Math.round(w.y - height / 2), width, height } as unknown as Payload,
        }),
      );
      fresh.delete(id);
    },
    fresh: (id) => fresh.has(id),
    deleteAgent,
  });
  setCanvasesDeps({
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
  // The trees draw again from the agents' state. Changes arrive in bursts (a spawn
  // touches several records), so one redraw 300 ms after the first of a burst.
  let treeTimer: ReturnType<typeof setTimeout> | null = null;
  const renderTrees = (): void => {
    if (treeTimer !== null) clearTimeout(treeTimer);
    treeTimer = null;
    renderFileTrees();
  };
  const renderSoon = (): void => {
    if (treeTimer === null) treeTimer = setTimeout(renderTrees, 300);
  };

  // A FILE TREE, a PREVIEW or the CANVAS LIST at the spot: page-only cells (no
  // backend); `extra` is more of the cell's record (its grant, how it binds).
  const spawnPageCellAt = (w: { x: number; y: number }, handler: string, prefix: string, width: number, height: number, name?: string, extra: Record<string, unknown> = {}): void => {
    registerCell(
      new Agent({
        id: `${prefix}_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: handler,
        meta: { x: Math.round(w.x), y: Math.round(w.y), width, height, ...(name !== undefined ? { display_name: name } : {}), ...extra } as unknown as Payload,
      }),
    );
  };

  // THE BACKGROUNDS AGENT at the spot: a page agent that IS its cell, granted
  // onto its view's background on both ends (`backgrounds_agent.grantsFor`)
  const spawnBackgroundsAt = (w: { x: number; y: number }): void => {
    const parent = containerNow();
    const id = `backgrounds_${crypto.randomUUID().slice(0, 8)}`;
    const g = grantsFor(kernel, id, parent);
    if (g.bg !== undefined) kernel.updateMeta(g.bg.id, { callable_by: g.bg.callableBy } as unknown as Payload);
    registerCell(
      new Agent({
        id,
        parentId: parent,
        handlerModule: BACKGROUNDS,
        meta: { x: Math.round(w.x), y: Math.round(w.y), width: 400, height: 620, display_name: "canvas theme",
          can_call: g.canCall as unknown as Json, callable_by: { $self: "*" }, user_bgs: [], hidden_stock: [], current: null, rotate: { on: false, every_s: 60 } },
      }),
    );
  };

  // TIPS & TRICKS at the spot: a PLAIN html cell, its default body the tips
  // card (`tips/tips_html.ts`) — edited like any html cell
  const spawnTipsAt = (w: { x: number; y: number }): void => {
    registerCell(
      new Agent({
        id: `tips_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: "html_agent.ts",
        meta: { x: Math.round(w.x), y: Math.round(w.y), width: 440, height: 290, display_name: "tips & tricks", html: TIPS_HTML },
      }),
    );
  };

  // THE MESSAGE STREAM at the spot: a page-only cell (no backend).
  const spawnStreamAt = (w: { x: number; y: number }): void => {
    registerCell(
      new Agent({
        id: `stream_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: streamView.handles[0]!,
        meta: { x: Math.round(w.x), y: Math.round(w.y), width: 560, height: 320 },
      }),
    );
  };

  const spawnHtmlAt = (w: { x: number; y: number }): void => {
    registerCell(
      new Agent({
        id: `html_${crypto.randomUUID().slice(0, 8)}`,
        parentId: containerNow(),
        handlerModule: "html_agent.ts",
        meta: {
          x: Math.round(w.x),
          y: Math.round(w.y),
          width: 360,
          height: 240,
          html: EMPTY_HTML,
        },
      }),
    );
  };


  // THE ACTION POPUP: a double click on empty canvas, or ⌘K / Ctrl-K at the pointer.
  // the pointer before any move is the canvas's middle (read when it is needed: the box is laid out by then)
  let pointer: { x: number; y: number } | null = null;
  const pointerAt = (): { x: number; y: number } => pointer ?? boxCentre(canvas.getBoundingClientRect());
  canvas.addEventListener("mousemove", (e) => {
    pointer = { x: e.clientX, y: e.clientY };
  });
  const spotAt = async (sx: number, sy: number, title: string, onClose?: () => void, selector?: string): Promise<void> => {
    let w = screenToWorld(sx, sy);
    openActionPopup({
      screenX: sx,
      screenY: sy,
      presets: await presets(),
      title,
      ...(onClose ? { onClose } : {}),
      // the popup dragged: the pick lands at the moved insert point, and the
      // selector's own record says where it now is
      onMove: (nx: number, ny: number): void => {
        w = screenToWorld(nx, ny);
        if (selector !== undefined && kernel.get(selector) !== undefined) {
          kernel.updateMeta(selector, { x: Math.round(w.x), y: Math.round(w.y), sx: Math.round(nx), sy: Math.round(ny) } as unknown as Payload);
        }
      },
      onPick: (p: Pick): void => {
        if (p.kind === "terminal") void spawnTerminalAt(w);
        else if (p.kind === "html") spawnHtmlAt(w);
        else if (p.kind === "setup") openSetupAt(w);
        else if (p.kind === "stream") spawnStreamAt(w);
        else if (p.kind === "chat") void reopenAt(w, { id: p.id, name: p.name });
        // a CODING AGENT: the human's own program, started in a terminal cell
        // (its exec agent runs EXACTLY that program, as `run COMMAND` does)
        else if (p.kind === "coding") void spawnPairAt(DEFAULT_VIEW, w, { width: 760, height: 480 }, { command: [p.path], display_name: p.name }, { allowed: [exactLine([p.path])] });
        else if (p.kind === "mcp") spawnPageCellAt(w, MCP_ADD_VIEW, "mcp_add", 420, 260);
        else if (p.kind === "files") spawnPageCellAt(w, filesView.handles[0]!, "files", 320, 560, "file tree", { can_call: fileGrant() });
        else if (p.kind === "preview") spawnPageCellAt(w, previewView.handles[0]!, "preview", 560, 520, "preview", { can_call: fileGrant(), follow: true });
        else if (p.kind === "backgrounds") spawnBackgroundsAt(w);
        // THE USER STORE: a view of the host's `user_store` agent (its own html view)
        else if (p.kind === "user_store") void kernel.send([{ target: containerNow(), payload: { type: "place", backend_id: "user_store", x: Math.round(w.x), y: Math.round(w.y), width: 440, height: 560 } as unknown as Payload }]).then((r) => {
          const e = (r[0]?.result as Rec | null)?.["error"];
          if (typeof e === "string") toast(`✕ ${e}`);
        });
        // THE MACHINE'S AGENTS (`system` recipe): the agent's own html view, 380x420
        else if (p.kind === "system") void kernel.send([{ target: containerNow(), payload: { type: "place", backend_id: p.id, x: Math.round(w.x), y: Math.round(w.y), width: 380, height: 420 } as unknown as Payload }]).then((r) => {
          const e = (r[0]?.result as Rec | null)?.["error"];
          if (typeof e === "string") toast(`✕ ${e}`);
        });
        else if (p.kind === "tips") spawnTipsAt(w);
        else if (p.kind === "bundle") void spawnBundleAt(w, { bundle: p.starter });
        else if (p.kind === "instance") void spawnBundleAt(w, { instance_of: p.bundle });
        else if (p.kind === "page_instance") spawnPageCellAt(w, p.bundle, p.bundle, 360, 260);
        else void spawnAiAt(w, null, p.text);
      },
    });
  };
  // THE SELECTOR (`selector_agent.ts`): a double click ADDS one at the spot; its
  // boot opens the popup on top; it deletes itself once the human is done. The
  // canvas lends it the popup and its tiles. A selector left by a closed tab
  // (its popup gone with the tab) is removed at load.
  for (const id of everyAgent()) if (kernel.get(id)?.record().handler_module === SELECTOR) kernel.remove(id);
  setSelectorHost({
    open: (id, sx, sy, title, done) => spotAt(sx ?? pointerAt().x, sy ?? pointerAt().y, title, done, id),
    tiles: async (typed) => tiles(typed, await presets()).map((t) => ({ label: t.label, kind: t.pick.kind, section: t.section, hint: t.hint })),
  });
  const openSelector = (sx: number, sy: number): void => {
    const w = screenToWorld(sx, sy);
    const id = `${SELECTOR_PREFIX}${crypto.randomUUID().slice(0, 8)}`;
    kernel.register(new Agent({
      id, parentId: containerNow(), handlerModule: SELECTOR,
      sentence: "The double-click popup, at its spot: it deletes itself once the human picks.",
      meta: { x: Math.round(w.x), y: Math.round(w.y), sx: Math.round(sx), sy: Math.round(sy) },
    }));
    void kernel.send([{ target: id, payload: { type: "boot" } as unknown as Payload }]);
  };
  // THE DROP (#134, #135): tree rows dragged onto the canvas open where they land — an
  // agent shown (moved if it is on the view, never duplicated), a file as a preview
  // pinned to it, a folder as a tree rooted at it. Each one is the view's `open` verb.
  canvas.addEventListener("dragover", (e) => {
    if (!e.dataTransfer?.types.includes(DRAG_MIME)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
    canvas.classList.add("drop-target");
  });
  canvas.addEventListener("dragleave", (e) => {
    if (!canvas.contains(e.relatedTarget as Node | null)) canvas.classList.remove("drop-target");
  });
  canvas.addEventListener("drop", (e) => {
    canvas.classList.remove("drop-target");
    if (!e.dataTransfer?.types.includes(DRAG_MIME)) return;
    e.preventDefault();
    const got = decodeDrag(e.dataTransfer.getData(DRAG_MIME));
    if ("error" in got) return toast(`✕ ${got.error}`);
    // several items sit around the drop point, one slot each (no two overlap): the same
    // plan for a drag from the tree cell and from the sidebar (#148)
    const plan = dropPlan(got.items, e.clientX, e.clientY, canvas.getBoundingClientRect(), view, DROP_SLOT);
    void (async (): Promise<void> => {
      for (const { item: it, x, y } of plan) {
        const what = it.kind === "agent" ? { agent: it.id } : { path: it.path };
        try {
          const r = (await kernel.send([{ target: containerNow(), payload: { type: "open", ...what, x, y } as unknown as Payload }]))[0]?.result as Rec | null;
          const err = r?.["error"];
          if (typeof err === "string") toast(`✕ ${err}`);
        } catch (err) {
          // the send itself failed: say so, and go on with the rest of the drop
          toast(`✕ ${(err as Error).message}`);
        }
      }
    })();
  });
  canvas.addEventListener("dblclick", (e) => {
    if ((e.target as Element).closest(".agent-frame")) return;
    openSelector(e.clientX, e.clientY);
  });
  // THE CANVAS'S OWN SIDEBAR (#148): the file tree beside the zoomed world, never in it,
  // so a zoom or a pan leaves it where it is. Its rows drag onto `#canvas` like the tree cell's.
  // Mounted after `setFilesDeps` (the tree reads the canvas through it).
  // (its open or its place moves the canvas's box: the GL camera reads that box, so it follows)
  const treeSidebar = mountTreeSidebar({ kernel, view: ROOT_VIEW, mount, wide: innerWidth > 768, onPlace: () => glHost?.applyCamera() });
  // ESC LEAVES FULL VIEW (owner 2026-10-08: nothing floats, so this is the way
  // back): to the canvas's root view — unless the keyboard is in an editor (a
  // terminal, a text field), whose Esc is its own
  window.addEventListener("keydown", (e) => {
    // Esc first closes an open head menu (#133); the next one leaves full view
    if (e.key === "Escape" && cellMenuEl !== null) {
      closeCellMenu();
      return;
    }
    if (e.key !== "Escape" || !fullNow() || document.querySelector(".action-popup")) return;
    const t = document.activeElement as HTMLElement | null;
    if (t && (t.isContentEditable || t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.closest(".xterm"))) return;
    e.preventDefault();
    setViewed(ROOT_VIEW);
  });
  window.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      openSelector(pointerAt().x, pointerAt().y);
    }
  });

  kernel.addStateSubscriber(renderSoon);
  // every page agent is documented as it appears (the page kernel's `docs`)
  documentPage(kernel);
  kernel.addStateSubscriber(() => documentPage(kernel));
  ensureBackground(ROOT_VIEW);
  if (isContainer(look, viewed)) ensureBackground(viewed);
  // THE SKIN (`skin_agent.ts`): made once, then the one in use applied — and
  // again, live, whenever its record changes; a change draws the view again (a
  // native backdrop hides the background)
  ensureSkin(kernel);
  watchSkin(kernel, toast);
  onSkin(() => void refresh());
  window.addEventListener("hashchange", () => {
    const want = viewFromHash(location.hash);
    if (want !== null && want !== viewed && kernel.get(want) !== undefined) setViewed(want, false);
  });
  await reconcile();
  await refresh();
  renderTrees();
}
