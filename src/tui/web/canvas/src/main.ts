import { registerBackgrounds } from "./bundles/backgrounds/backgrounds_agent.ts";
import { Agent, Host, Kernel, ProxyLoader, WsBridge, loadBundle, registerRamStore } from "./lib.ts";
import type { FolderText } from "./lib.ts";
import { GATEWAY, UserBridge } from "./user_bridge.ts";
import { registerGlAgent } from "./bundles/gl_agent/gl_agent.ts";
import { registerHtmlAgent } from "./bundles/html_agent/html_agent.ts";
import { registerAgentView } from "./bundles/agent_view/agent_view.ts";
import { mountCanvas } from "./bundles/canvas/canvas.ts";
import { setCanvasNode } from "./bundles/canvas/host_views.ts";
import { registerCanvas } from "./bundles/canvas/canvas_agent.ts";
import { registerBackground } from "./bundles/canvas/background_agent.ts";
import { registerSelector } from "./bundles/canvas/selector_agent.ts";
import { registerLayout } from "./bundles/canvas/layout.ts";

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
const CANVAS_NODE = "canvas.tools";
const LOADER_HM = "kernel_state.tools";
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
  registerLayout(kernel); // a view's layout per theme (canvas | mobile): where its cells sit
  registerBackgrounds(kernel); // the backgrounds manager: stock + the user's own, random, every minute
  // View bundles that front a host backend are real frontend agents too — a
  // no-op handler makes their records load-able; the ViewBundle renders them.
  kernel.registerBundle("terminal_view.ts", () => null);
  kernel.registerBundle("ai_view.ts", () => null);
  kernel.registerBundle("reflect_view.ts", () => null);
  kernel.registerBundle("setup_view.ts", () => null);
  kernel.registerBundle("stream_view.ts", () => null);
  kernel.registerBundle("sidebar_view.ts", () => null); // the chats + agent tree cell
  kernel.registerBundle("canvases_view.ts", () => null); // the canvas-list cell
  kernel.registerBundle("mcp_view.ts", () => null); // an MCP server agent's view
  kernel.registerBundle("mcp_add_view.ts", () => null); // the add-an-MCP-server form

  // Each frontend bundle declares its role via reflect (readme=true): the view
  // CLIENTS name the host CAPABILITY + verb surface they front (an LLM weaves the
  // pairing from this + the host's capability readme — no hardcoded pairing);
  // content agents say what they hold. Direction is frontend→host: the JS knows
  // it fronts a host capability; the host stays ignorant of the frontend.
  kernel.setBundleReadme(
    "html_agent.ts",
    "An html cell: its record's `html` body, rendered in a sandboxed frame (unsandboxed when trusted). Verbs: get_html, set_html {bytes}. A PANEL'S JS (an html cell's body) has `fantastic`: `await fantastic.send(id, payload)` → the reply · `fantastic.watch(id, ev => …)` → events (returns off) · `fantastic.emit(id, payload)` · `fantastic.onMessage(p => …)`. Its calls are judged by the CELL's can_call — grant what it calls (`{<id>: [verbs…, watch]}`); a reply's `grants_missing` names any it lacks. On the host they arrive as `user`: a host agent you create for a panel needs `callable_by: {user: [<the verbs the panel calls>, watch]}` in its create_agent (a new agent admits no one); never drop `user` from an existing agent's callable_by. Place, move, grant and remove cells through the view (`view_root`).",
  );
  kernel.setBundleReadme(
    "agent_view.ts",
    "A HOST agent's own view: the cell shows `backend_id`, drawn as that agent's record declares (`view: {kind: html, bytes | bytes_file, calls}`). The cell holds no content — the body is the host agent's; its grant onto the agent is the record's `calls`. Show a host agent: place {backend_id, x, y} on the view (no kind).",
  );
  kernel.setBundleReadme(
    "sidebar_view.ts",
    "A sidebar cell: the agent tree of both kernels. Page content, no backend; as many as you like.",
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
  kernel.setBundleReadme(
    "ai_view.ts",
    "A chat cell: the human's view of a host AI (`backend_id`). It sends the human's turns (send), draws the AI's streamed parts, and stops a turn (stop).",
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
const asked = new URLSearchParams(location.search).get("canvas");
const here = asked !== null ? canvases.find((c) => c.id === asked) : (canvases.find((c) => c.main) ?? canvases[0]);
if (here === undefined) {
  document.body.textContent = asked !== null ? `no canvas ${asked} on this host` : "this host holds no canvas";
  await new Promise(() => {});
}
// known before any record loads: a cell's grants name this canvas's node
setCanvasNode(here!.id);
const LOADER = here!.loader;
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
// LIVE PAGE BUNDLES, ON THE DISK'S EVENTS (owner 2026-10-08: no idle loops):
// the host's `bundles_watch` bridge reports `changed {path}` under this page's
// own `bundles/` (watched, and each folder in it — not recursive); a burst is
// taken together, then each touched folder's `reload_bundle` restarts its
// live instances and their panels draw again. Nothing runs while nothing changes.
const BRIDGE = "bundles_watch";
const dir = `${here!.root}/bundles`;
const watchPath = async (path: string): Promise<void> => {
  const r = (await kernel.callHost(BRIDGE, { type: "watch_path", path } as never)) as Record<string, unknown> | null;
  if (typeof r?.["error"] === "string") console.error(`[canvas] page bundles: watch ${path}: ${r["error"]}`);
};
const touched = new Set<string>();
let settle: ReturnType<typeof setTimeout> | null = null;
const reloadTouched = async (): Promise<void> => {
  settle = null;
  const names = [...touched];
  touched.clear();
  for (const name of names) {
    const r = (await kernel.send([{ target: "kernel", payload: { type: "reload_bundle", name } as never }]))[0]?.result as Record<string, unknown> | undefined;
    if (typeof r?.["error"] === "string") {
      console.error(`[canvas] page bundle ${name}: ${r["error"]}`);
      continue;
    }
    // its instances restarted on the new load: their panels draw it again
    for (const id of Array.isArray(r?.["instances"]) ? (r!["instances"] as unknown[]) : []) {
      if (typeof id === "string") kernel.emit(id, { type: "reload_html" } as never);
    }
  }
};
new Host(kernel, "canvas").on(BRIDGE, "changed", (ev) => {
  const path = typeof ev["path"] === "string" ? ev["path"].replace(/^\.\//, "") : "";
  if (!path.startsWith(`${dir}/`)) return; // another canvas's, or the host's
  const rest = path.slice(dir.length + 1).split("/");
  const name = rest[0];
  if (!name) return;
  if (rest.length === 1 && ev["kind"] === "added") void watchPath(`${dir}/${name}`); // a new folder: its files too
  touched.add(name);
  if (settle === null) settle = setTimeout(() => void reloadTouched(), 150);
});
// ARMED BEFORE the tree loads: an edit made once the page shows is never missed
await Promise.all([watchPath(dir), ...folders.map((f) => watchPath(`${dir}/${f.name}`))]);
inner.announce();
loader.start();

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
  node: here!.id,
  canvases,
});

/** Every canvas on the host: `canvas.tools` children of `canvas_page`, each with
 *  its page loader (its `kernel_state` child). */
async function listCanvases(): Promise<Array<{ id: string; loader: string; root: string; main: boolean; name: string }>> {
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
  const out: Array<{ id: string; loader: string; root: string; main: boolean; name: string }> = [];
  for (const n of all) {
    if (n["handler_module"] !== CANVAS_NODE || n["parent_id"] !== "canvas_page" || typeof n["id"] !== "string") continue;
    const l = all.find((a) => a["parent_id"] === n["id"] && a["handler_module"] === LOADER_HM);
    if (typeof l?.["id"] !== "string") continue;
    const name = typeof n["display_name"] === "string" && n["display_name"] !== "" ? n["display_name"] : n["id"].replace(/^canvas_/, "");
    out.push({ id: n["id"], loader: l["id"], root: typeof l["root"] === "string" ? l["root"] : ".fantastic/web", main: n["main"] === true, name });
  }
  return out.sort((a, b) => Number(b.main) - Number(a.main) || (a.id < b.id ? -1 : 1));
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
  box.style.cssText = "position:fixed;inset:0;display:grid;place-content:center;font:14px system-ui;color:#cde;background:#0b0b10";
  document.body.replaceChildren(box);
}
