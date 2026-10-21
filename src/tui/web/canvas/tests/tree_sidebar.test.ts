import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { CANVAS_CSS } from "../src/bundles/canvas/styles.ts";
import { ROOT_VIEW, VIEW_VERBS, viewAgent } from "../src/bundles/canvas/canvas_agent.ts";
import { BESIDE, OPEN, SIDEBAR_KEY, mountTreeSidebar, saveSidebar, sidebarOpenOf, sidebarSavedOf } from "../src/bundles/canvas/tree_sidebar.ts";
import { PAGE_VERB_DOCS } from "../src/bundles/verb_docs.ts";
import { decodeDrag, dropPlan, encodeDrag, worldAt } from "../src/bundles/files/drag.ts";
import type { DragItem, ViewPose } from "../src/bundles/files/drag.ts";
import { servedOf } from "../src/bundles/files/file_bridge.ts";
import { filesView } from "../src/bundles/files/files_view.ts";
import { setFilesDeps } from "../src/bundles/files/files_deps.ts";
import type { ViewContext, ViewHandle } from "../src/bundles/view.ts";
import { installDom, stub } from "./dom_stub.ts";
import { FakeBridge, settle } from "./fake_bridge.ts";
import { project } from "./rig.ts";

// #148: the file tree as the canvas's own sidebar. It is mounted beside the zoomed
// world (never in it), it collapses and keeps that choice on the canvas's root view,
// its drops are the canvas's own (the same plan the tree cell's drop gets), and its
// verbs reach it through the root view. The tree itself is `tree_panel.ts`, tested
// through the tree cell's own tests.

type Dict = Record<string, unknown>;

/** A page kernel with the canvas's root and its root view, and a bridge to the fake project. A view's calls go to the view. */
function pageKernel(b: FakeBridge): Kernel {
  const k = new Kernel();
  k.bridge = {
    forward: async (t: string, p: Payload) => (await (t.startsWith("view:") ? b.viewCall(t, p as Dict) : b.call(p as Dict))) as never,
    subscribeState: b.subscribeState,
    watchRemote: () => {},
    unwatchRemote: () => {},
  } as unknown as Kernel["bridge"];
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  k.register(new Agent({ id: ROOT_VIEW, parentId: "canvas", handlerModule: "view.ts" }));
  return k;
}

/** The canvas's deps for a tree, as the canvas hands them over (no agents here). */
function deps(): void {
  setFilesDeps({
    hostAgents: async () => [],
    hostRecords: () => [],
    pageAgents: () => [],
    seenHere: () => false,
    onHost: () => false,
    cellsFor: () => [],
    isHidden: () => false,
    moveTo: () => {},
    view: () => {},
    viewed: () => "",
    mac: false,
    open: () => {},
    fresh: () => false,
    freshIds: () => [],
    deleteAgent: () => {},
    mountAgentView: async () => ({ unmount: () => {} }),
  });
}

/** A page's mount that records what is appended to it (the sidebar's element, and nothing else) and the classes the sidebar sets on it. */
function pageMount(): { mount: HTMLElement; appended: unknown[]; classes: Set<string> } {
  const appended: unknown[] = [];
  const classes = new Set<string>();
  const mount = {
    appendChild: (el: unknown) => (appended.push(el), el),
    classList: {
      add: (c: string) => classes.add(c),
      remove: (...cs: string[]) => cs.forEach((c) => classes.delete(c)),
      toggle: (c: string, force: boolean) => (force ? classes.add(c) : classes.delete(c)),
    },
  } as unknown as HTMLElement;
  return { mount, appended, classes };
}

// ─── MOUNTED BESIDE THE WORLD ───────────────────────────────────────────────

test("the sidebar is appended to the page, and its tree is in its own body: never the zoomed world's", () => {
  installDom();
  deps();
  const b = new FakeBridge();
  project(b);
  const k = pageKernel(b);
  const { mount, appended } = pageMount();
  const sb = mountTreeSidebar({ kernel: k, view: ROOT_VIEW, mount, wide: true });
  assert.equal(appended.length, 1, "one element goes onto the page");
  assert.equal(appended[0], sb.el, "and it is the sidebar itself");
  sb.dispose();
});

test("the sidebar's rule is fixed to the viewport's left edge, and the world is the only thing zoomed", () => {
  const css = CANVAS_CSS.replace(/\s+/g, " ");
  assert.match(css, /\.tree-sidebar \{ position: fixed; top: 0; bottom: 0; left: 0;/, "fixed to the viewport, not to the world");
  assert.match(css, /\.canvas-world \{ position: absolute; inset: 0; transform-origin: 0 0; \}/, "the world keeps its own transform");
});

test("the canvas mounts the sidebar into its page, beside the world, and gives it no drop of its own", () => {
  const src = readFileSync(new URL("../src/bundles/canvas/canvas.ts", import.meta.url), "utf8");
  assert.match(src, /mountTreeSidebar\(\{ kernel, view: ROOT_VIEW, mount, wide: innerWidth > 768 \}\)/);
  assert.doesNotMatch(src, /world\.appendChild\(\s*mountTreeSidebar/, "never inside the zoomed layer");
  const side = readFileSync(new URL("../src/bundles/canvas/tree_sidebar.ts", import.meta.url), "utf8");
  assert.doesNotMatch(side, /addEventListener\("drop"/, "a drop is the canvas's, whichever tree it came from");
});

// ─── COLLAPSE, AND ITS PERSISTENCE PER CANVAS ───────────────────────────────

test("open or collapsed is the saved choice; with none saved, it opens on a wide screen and not on a narrow one", () => {
  assert.equal(sidebarOpenOf({}, true), true);
  assert.equal(sidebarOpenOf({}, false), false);
  assert.equal(sidebarOpenOf({ open: false }, true), false, "a saved collapse wins on a wide screen");
  assert.equal(sidebarOpenOf({ open: true }, false), true, "a saved open wins on a narrow one");
});

test("the saved state is read whole, or not at all: a bad field is not guessed at", () => {
  assert.deepEqual(sidebarSavedOf(undefined), {});
  assert.deepEqual(sidebarSavedOf({ [SIDEBAR_KEY]: "x" }), {});
  assert.deepEqual(sidebarSavedOf({ [SIDEBAR_KEY]: [1] }), {});
  assert.deepEqual(sidebarSavedOf({ [SIDEBAR_KEY]: { open: "yes" } }), {});
  assert.deepEqual(
    sidebarSavedOf({ [SIDEBAR_KEY]: { open: false, expanded: ["src", 3], selected: "src/one.txt", junk: 1 } }),
    { open: false, expanded: ["src"], selected: "src/one.txt" },
  );
});

test("a save changes only what it names: the rest of the canvas's sidebar state stays", () => {
  const b = new FakeBridge();
  project(b);
  const k = pageKernel(b);
  saveSidebar(k, ROOT_VIEW, { open: false });
  saveSidebar(k, ROOT_VIEW, { expanded: ["src"] });
  assert.deepEqual(sidebarSavedOf(k.get(ROOT_VIEW)?.meta), { open: false, expanded: ["src"] });
});

test("collapse and expand are kept on the canvas, and a reload restores the same state", () => {
  installDom();
  deps();
  const b = new FakeBridge();
  project(b);
  const k = pageKernel(b);
  const first = pageMount();
  const sb = mountTreeSidebar({ kernel: k, view: ROOT_VIEW, mount: first.mount, wide: true });
  assert.equal(sb.open, true, "open on a wide screen when nothing is saved");
  sb.setOpen(false);
  assert.equal(sb.open, false);
  assert.equal(sidebarSavedOf(k.get(ROOT_VIEW)?.meta).open, false, "the collapse is kept on the canvas's root view");
  sb.dispose();
  // a reload: the sidebar is mounted again over the same canvas, and it comes back collapsed
  const second = pageMount();
  const again = mountTreeSidebar({ kernel: k, view: ROOT_VIEW, mount: second.mount, wide: true });
  assert.equal(again.open, false, "a reload keeps it collapsed");
  again.setOpen(true);
  assert.equal(sidebarSavedOf(k.get(ROOT_VIEW)?.meta).open, true, "and an expand is kept the same way");
  again.dispose();
});

// ─── A DROP, AT THE POINT IT LANDS ──────────────────────────────────────────

const SLOT = { width: 520, height: 560 };
const RECT = { left: 0, top: 40 };
const POSES: ViewPose[] = [
  { z: 0.1, ox: 300, oy: -200 },
  { z: 0.5, ox: 0, oy: 0 },
  { z: 1, ox: -120, oy: 64 },
  { z: 2.5, ox: 900, oy: 300 },
  { z: 5, ox: -4000, oy: -2000 },
];
const DROPS: Array<[number, number]> = [
  [640, 480],
  [120, 900],
];

test("a drop lands where it was dropped, in the canvas's world, at any zoom and pan", () => {
  for (const pose of POSES) {
    for (const [cx, cy] of DROPS) {
      for (const n of [1, 3]) {
        const items: DragItem[] = Array.from({ length: n }, (_, i) => ({ kind: "agent" as const, id: `a${i}` }));
        const plan = dropPlan(items, cx, cy, RECT, pose, SLOT);
        // the grid of spots is centred on the point, to half a world unit (the rounding of each spot)
        const minX = Math.min(...plan.map((p) => p.x));
        const maxX = Math.max(...plan.map((p) => p.x)) + SLOT.width;
        const minY = Math.min(...plan.map((p) => p.y));
        const maxY = Math.max(...plan.map((p) => p.y)) + SLOT.height;
        const at = worldAt(cx, cy, RECT, pose);
        assert.ok(Math.abs((minX + maxX) / 2 - at.x) <= 0.5, `z=${pose.z} n=${n}: centred in x`);
        assert.ok(Math.abs((minY + maxY) / 2 - at.y) <= 0.5, `z=${pose.z} n=${n}: centred in y`);
        // and that world point, put back on the screen by the world's own transform, is the drop point
        const back = { x: ((minX + maxX) / 2) * pose.z + RECT.left + pose.ox, y: ((minY + maxY) / 2) * pose.z + RECT.top + pose.oy };
        assert.ok(Math.abs(back.x - cx) <= 0.5 * pose.z, `z=${pose.z}: the drop's x on screen`);
        assert.ok(Math.abs(back.y - cy) <= 0.5 * pose.z, `z=${pose.z}: the drop's y on screen`);
      }
    }
  }
});

test("a point is the world's: the screen's corner is the world's origin at zero pan, and zoom divides the offset", () => {
  assert.deepEqual(worldAt(10, 50, { left: 10, top: 50 }, { z: 1, ox: 0, oy: 0 }), { x: 0, y: 0 });
  assert.deepEqual(worldAt(410, 250, { left: 10, top: 50 }, { z: 2, ox: 0, oy: 0 }), { x: 200, y: 100 });
  assert.deepEqual(worldAt(110, 150, { left: 10, top: 50 }, { z: 2, ox: 20, oy: 20 }), { x: 40, y: 40 });
});

test("a drag from the sidebar or the tree cell carries the same payload, so it plans the same spots", () => {
  // both trees write their rows' items with the one encoder (`tree_panel.ts`); a drop reads them with the one decoder
  const items: DragItem[] = [{ kind: "agent", id: "ea" }, { kind: "file", path: "src/one.txt" }, { kind: "dir", path: "docs" }];
  const fromSidebar = decodeDrag(encodeDrag(items));
  const fromCell = decodeDrag(encodeDrag(items));
  assert.ok(!("error" in fromSidebar) && !("error" in fromCell));
  for (const pose of POSES) {
    assert.deepEqual(
      dropPlan(fromSidebar.items, 700, 300, RECT, pose, SLOT),
      dropPlan(fromCell.items, 700, 300, RECT, pose, SLOT),
    );
  }
});

// ─── ITS VERBS, THROUGH THE ROOT VIEW ───────────────────────────────────────

test("the tree's verbs reach the sidebar through the root view: `tree` names its verb, and a bad one is refused", async () => {
  installDom();
  deps();
  const b = new FakeBridge();
  project(b);
  const k = pageKernel(b);
  const { mount } = pageMount();
  const sb = mountTreeSidebar({ kernel: k, view: ROOT_VIEW, mount, wide: true });
  assert.ok((VIEW_VERBS as readonly string[]).includes("tree"), "the view answers `tree`");
  assert.ok(PAGE_VERB_DOCS["view.ts"]?.["tree"]?.includes("select"), "and says what it takes");
  const none = (await viewAgent(ROOT_VIEW, { type: "tree" } as Payload, k)) as Dict;
  assert.equal(none["reason"], "bad_arg", "no verb is refused");
  assert.equal(none["arg"], "verb", "and the refusal names the argument the caller gave: `verb`");
  const empty = (await viewAgent(ROOT_VIEW, { type: "tree", verb: "" } as Payload, k)) as Dict;
  assert.equal(empty["arg"], "verb", "an empty verb is refused the same way");
  const bogus = (await viewAgent(ROOT_VIEW, { type: "tree", verb: "bogus" } as Payload, k)) as Dict;
  assert.equal(bogus["reason"], "unhandled", "an unknown verb is refused, said");
  const out = (await viewAgent(ROOT_VIEW, { type: "tree", verb: "expand", path: "../x" } as Payload, k)) as Dict;
  assert.equal(out["reason"], "outside_root", "a path outside the project is refused by the tree's own rule");
  sb.dispose();
  const gone = (await viewAgent(ROOT_VIEW, { type: "tree", verb: "expand", path: "src" } as Payload, k)) as Dict;
  assert.equal(gone["reason"], "not_mounted", "with the sidebar gone, its verbs say so");
});

test("the canvas's margin is the sidebar's width: the open and the rail widths are the sidebar's own", () => {
  const css = CANVAS_CSS.replace(/\s+/g, " ");
  assert.match(css, /\.tree-sidebar \{ position: fixed; top: 0; bottom: 0; left: 0;[^}]*width: 300px;/, "open, the sidebar is 300px");
  assert.match(css, /\.tree-sidebar\.collapsed \{ width: 22px; \}/, "collapsed, a 22px rail");
  assert.match(css, /\.tree-beside \{ --tree-w: 22px; \}/, "beside the canvas, a collapsed sidebar takes the rail's width");
  assert.match(css, /\.tree-beside\.tree-open \{ --tree-w: 300px; \}/, "and an open one takes the sidebar's");
  assert.match(css, /\.tree-beside #canvas \{ margin-left: var\(--tree-w\); width: calc\(100% - var\(--tree-w\)\); \}/, "the canvas starts where the sidebar ends");
  assert.match(css, /#canvas\.full-view \.agent-frame\.full \{ left: 12px !important; top: 44px !important; width: calc\(100% - 24px\) !important;/, "a full cell fits the canvas, not the viewport");
});

test("on a wide screen the canvas sits beside the sidebar and it is open; on a narrow one it is an overlay, and closed", () => {
  installDom();
  deps();
  const b = new FakeBridge();
  project(b);
  const wide = pageMount();
  const sb = mountTreeSidebar({ kernel: pageKernel(b), view: ROOT_VIEW, mount: wide.mount, wide: true });
  assert.ok(wide.classes.has(BESIDE), "the canvas is beside it");
  assert.ok(wide.classes.has(OPEN), "and it is open");
  sb.setOpen(false);
  assert.ok(wide.classes.has(BESIDE), "collapsed, it is still beside: its rail is the canvas's margin");
  assert.ok(!wide.classes.has(OPEN), "and the margin is the rail's");
  sb.dispose();
  assert.equal(wide.classes.size, 0, "a disposed sidebar leaves no class on the page");
  const narrow = pageMount();
  const nb = mountTreeSidebar({ kernel: pageKernel(b), view: ROOT_VIEW, mount: narrow.mount, wide: false });
  assert.equal(narrow.classes.size, 0, "a narrow screen's sidebar overlays the canvas: no margin");
  nb.dispose();
});

test("the tree cell still binds its own folder beside the sidebar: the sidebar holds none, and the cell lets go on unmount", async () => {
  installDom();
  deps();
  const b = new FakeBridge();
  project(b);
  const k = pageKernel(b);
  const sb = mountTreeSidebar({ kernel: k, view: ROOT_VIEW, mount: pageMount().mount, wide: true });
  await settle();
  assert.equal(servedOf(k).has(""), false, "the sidebar serves no folder of its own: it reads the project through the bridge");
  const ctx = (selfId: string, record: Dict): ViewContext =>
    ({ kernel: k, mount: stub(), selfId, backend: "files", record, close: () => {} }) as unknown as ViewContext;
  const cell = (await filesView.mount(ctx("tree_1", { root: "src" }))) as ViewHandle;
  await settle();
  assert.ok(servedOf(k).get("src") !== undefined, "the tree cell holds its own folder");
  cell.unmount();
  assert.equal(servedOf(k).has("src"), false, "the cell let its folder go");
  sb.dispose();
});
