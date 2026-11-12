import { registerBackgrounds } from "./bundles/backgrounds/backgrounds_agent.ts";
import { Agent, Kernel, ProxyLoader, WsBridge, loadBundle, registerRamStore } from "./lib.ts";
import type { FolderText } from "./lib.ts";
import { GATEWAY, UserBridge } from "./user_bridge.ts";
import { registerGlAgent } from "./bundles/gl_agent/gl_agent.ts";
import { registerHtmlAgent } from "./bundles/html_agent/html_agent.ts";
import { registerAgentView } from "./bundles/agent_view/agent_view.ts";
import { mountCanvas } from "./bundles/canvas/canvas.ts";
import { setCanvasNode } from "./bundles/canvas/host_views.ts";
import { onHostStopping, registerCanvas } from "./bundles/canvas/canvas_agent.ts";
import { registerBackground } from "./bundles/canvas/background_agent.ts";
import { registerSelector } from "./bundles/canvas/selector_agent.ts";
import { registerLayout } from "./bundles/canvas/layout.ts";
import { registerFilesView } from "./bundles/files/files_controller.ts";
import { registerPreviewView } from "./bundles/preview/preview_controller.ts";
import { registerSkin } from "./bundles/skin/skin_agent.ts";
import { bootSkin, installSkin } from "./bundles/skin/apply.ts";
import { tok } from "./bundles/skin/css.ts";
import { readShell } from "./bundles/shell.ts";
import { canvasesOf } from "./canvas_list.ts";
import type { CanvasEntry } from "./canvas_list.ts";
import { pickCanvas } from "./canvas_pick.ts";

// The canvas page. The frontend kernel boots as a PEER of the host: it dials the host's `web_loader` (by id) over the bridge (a host-side
// `kernel_state` rooted at `.fantastic/web/`, which the operator created — no
// automation), hydrates its OWN agent tree from there (`load_tree`), and
// persists every local change back (`proxy_loader`). The canvas renders the
// LOCAL member tree; host backends are weak peers referenced by id. One shared
// frontend tree, addressed by the loader's id — no per-session id.

// FLAT PEER CANVASES (phase 4): every canvas is a host `canvas.tools` node under
// `canvas_page` with its OWN page records (its `kernel_state` child) — so its own
// page kernel, its own guid. `?canvas=<node>` picks one; without it, the main
// one (`main: true`, declarative), else the first.
const origin =
  (location.protocol === "https:" ? "wss://" : "ws://") + location.host;

/** This app's bundles — the ones whose records this kernel runs locally.
 *  Anything else (e.g. a `*.tools` host record that strays in) weak-loads + skips.
 *  (The library registers its own memory store; the page never starts the
 *  kernel — an unstarted kernel keeps its execution rows in RAM, bounded by the
 *  ledger's ring; `kernel_start` is the operator's act.) */
function register(kernel: Kernel): void {
  registerGlAgent(kernel);
  registerHtmlAgent(kernel);
  registerAgentView(kernel); // a host agent's own view (its record's `view`)
  registerCanvas(kernel); // the compositor root and its control plane
  registerBackground(kernel); // its singleton, delete-locked background (set_bg/default_bg/clear_bg/get_bg)
  registerSelector(kernel); // the double-click popup, as its singleton, delete-locked agent
  registerLayout(kernel); // a view's layouts (canvas | mobile column): where its cells sit
  registerBackgrounds(kernel); // the backgrounds manager: stock + the user's own, random, every minute
  registerSkin(kernel); // the page's skin: its design tokens and backdrop (`skin`, made by mountCanvas)
  // View bundles that front a host backend are real frontend agents too — a
  // no-op handler makes their records load-able; the ViewBundle renders them.
  kernel.registerBundle("terminal_view.ts", () => null);
  kernel.registerBundle("reflect_view.ts", () => null);
  kernel.registerBundle("stream_view.ts", () => null);
  registerFilesView(kernel); // the file tree cell, a page agent with verbs (sidebar_view.ts is the cell's name before the file tree (#106): saved layouts still hold it, and it opens as the tree)
  registerPreviewView(kernel); // the previewer / editor cell: a page agent with verbs
  kernel.registerBundle("canvases_view.ts", () => null); // the canvas-list cell
  kernel.registerBundle("mcp_view.ts", () => null); // an MCP server agent's view
  kernel.registerBundle("mcp_add_view.ts", () => null); // the add-an-MCP-server form
  // the read-only views of the agents that had only the stub
  for (const v of ["resources", "tasks", "store", "scheduler", "credentials"]) kernel.registerBundle(`${v}_view.ts`, () => null);

  // Each frontend bundle declares its role via reflect: the view
  // CLIENTS name the host CAPABILITY + verb surface they front (an LLM weaves the
  // pairing from this + the host's capability readme — no hardcoded pairing);
  // content agents say what they hold. Direction is frontend→host: the JS knows
  // it fronts a host capability; the host stays ignorant of the frontend.
  kernel.setBundleReadme(
    "html_agent.ts",
    "An html cell: its record's `html` body, rendered in a sandboxed frame (unsandboxed when trusted). A PANEL'S JS (an html cell's body) has `fantastic`: `await fantastic.send(id, payload)` → the reply · `fantastic.watch(id, ev => …)` → events (returns off) · `fantastic.emit(id, payload)` · `fantastic.onMessage(p => …)`. Its calls are judged by the CELL's can_call — grant what it calls (`{<id>: [verbs…, watch]}`); a reply's `grants_missing` names any it lacks. On the host they arrive as `user`: a host agent you create for a panel needs `callable_by: {user: [<the verbs the panel calls>, watch]}` in its create_agent (a new agent admits no one); never drop `user` from an existing agent's callable_by. The view (`view_root`) places, moves, grants and removes cells.",
  );
  kernel.setBundleReadme(
    "agent_view.ts",
    "A HOST agent's own view: the cell shows `backend_id`, drawn as that agent's record declares (`view: {kind: html, bytes | bytes_file, calls}`). The cell holds no content — the body is the host agent's; its grant onto the agent is the record's `calls`. Show a host agent: place {backend_id, x, y} on the view (no kind).",
  );
  kernel.setBundleReadme(
    "canvases_view.ts",
    "A canvas-list cell: every canvas on the host (flat peers, ★ the main one); a click opens one, `+ new canvas` makes a peer. Page content, no backend.",
  );
  kernel.setBundleReadme(
    "gl_agent.ts",
    "Frontend WebGL content agent. Renders its record's `gl_source` shader and reacts to events it watches by id. Content, not a host client.",
  );
  kernel.setBundleReadme(
    "terminal_view.ts",
    "A terminal cell: an xterm onto a host exec agent's PTY job (`backend_id`) — it starts the job, writes keystrokes (write), resizes, and draws its output.",
  );
}

/** Fresh (the host namespace holds no records): seed the canvas root — the
 *  operator's first conscious agent. `boot` persists it with the first write. */
function seed(kernel: Kernel): void {
  kernel.setRoot(
    new Agent({
      id: "canvas",
      handlerModule: "canvas.ts",
      sentence: "The canvas — the page's root; its views are under it.",
    }),
  );
}

// THE SKIN FIRST, before anything awaits: the shell's flag says the backdrop
// (a plain tab has none: web), and its stock skin is installed so the first
// paint — the refused box too — is already skinned. That is the one skin the
// page shows. A malformed flag is the shell's bug: said in place of the canvas,
// and nothing else runs.
try {
  readShell();
} catch (e) {
  document.body.textContent = (e as Error).message;
  throw e;
}
{
  const boot = bootSkin();
  installSkin(boot.name, boot.skin);
}

// THE BOOT, the library's `boot` composed here with one change: every host call
// goes through `user` (`user_bridge.ts`).
const kernel = new Kernel();
registerRamStore(kernel);
register(kernel);
const inner = new WsBridge(kernel, { origin, controlEndpoint: GATEWAY });
new UserBridge(kernel, inner);

// IS THIS TAB LET IN: `user.forward` to the host kernel's reflect — refused
// when `user`'s grants do not admit the leg. Refused → the reason, LOUD, and
// nothing else runs.
const me = (await kernel.callHost("kernel", { type: "reflect", tree: "none" } as never)) as Record<string, unknown>;
if (me["reason"] === "unauthorized") {
  refused(String(me["error"] ?? "no answer"));
  await new Promise(() => {});
}

const canvases = await listCanvases();
const pick = pickCanvas(canvases, location.search);
if ("missing" in pick || "none" in pick) {
  document.body.textContent = "missing" in pick ? `no canvas ${pick.missing} on this host` : "this host holds no canvas";
  await new Promise(() => {});
}
const here = pick as CanvasEntry;
// known before any record loads: a cell's grants name this canvas's node
setCanvasNode(here.id);
const LOADER = here.loader;
const loader = new ProxyLoader(kernel, LOADER);
kernel.persistenceProvider = LOADER;
// PAGE BUNDLES (`<its records' root>/bundles/*/bundle.yaml`, through its loader)
// load BEFORE the tree, so a persisted instance runs; a refused read fails loud
kernel.readFolders = (name) => loader.readBundles(name);
const folders = await readFolders();
for (const f of folders) loadBundle(kernel, f);
const records = await loader.loadTree();
if (records.length > 0) kernel.load(records);
else seed(kernel);
inner.announce();
loader.start();
// THE HOST STOPS (its shutdown calls `<guid>|canvas` `host_stopping` before it
// drains): the last records flushed, the loader stopped — no write lands mid-drain
let stopping: Promise<void> | null = null;
onHostStopping(() => {
  inner.beginStop(); // the socket's close that follows is the stop: its cut-off calls settle quietly
  return (stopping ??= loader.stop());
});

// THE ROOT'S BOOT MINTS THE GUID (once; it persists with the root record) — the
// canvas's identity as a PEER kernel: the bridge says `hello {guid}` with it, and
// the host's agents address this canvas as `<guid>|canvas`. `boot` hydrates the
// tree but boots no agent, so the page boots its own root.
if (kernel.rootId !== null) await kernel.send([{ target: kernel.rootId, payload: { type: "boot" } }]);

await mountCanvas({
  kernel,
  mount: document.body,
  selfId: kernel.rootId ?? "canvas",
  flush: () => loader.flush(),
  node: here.id,
  canvases,
});

/** Every canvas on the host, at any depth under `canvas_page` (`canvasesOf`). */
async function listCanvases(): Promise<CanvasEntry[]> {
  const all: Array<Record<string, unknown>> = [];
  let after: unknown = null;
  for (;;) {
    const page = (await kernel.callHost("kernel", { type: "list_agents", limit: 200, after } as never)) as {
      agents?: Array<Record<string, unknown>>;
      next?: unknown;
    };
    all.push(...(page?.agents ?? []));
    if (page?.next === null || page?.next === undefined) break;
    after = page.next;
  }
  return canvasesOf(all);
}

/** The page's folders, as its loader reads them — a refusal is LOUD. */
async function readFolders(): Promise<FolderText[]> {
  const r = (await loader.readBundles()) as { bundles?: unknown } | null;
  if (!Array.isArray(r?.bundles)) throw new Error(`page bundles: the loader's read_bundles answered ${JSON.stringify(r)}`);
  return r.bundles as FolderText[];
}

/** The host refused this tab: its reason, in place of the canvas. */
function refused(why: string): void {
  const box = document.createElement("p");
  box.className = "host-refused";
  box.textContent = `The host refused this canvas — ${why}.`;
  box.style.cssText = `position:fixed;inset:0;display:grid;place-content:center;font:14px ${tok("font.family.sans")};color:${tok("color.text.cool")};background:${tok("color.page.bg")}`;
  document.body.replaceChildren(box);
}
