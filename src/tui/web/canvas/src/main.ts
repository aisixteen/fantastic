import { Agent, Kernel, ProxyLoader, WsBridge, registerRamStore } from "./lib.ts";
import { GATEWAY, UserBridge, pageVerbs, readToken, writeToken, writesPage } from "./user_bridge.ts";
import { registerGlAgent } from "./bundles/gl_agent/gl_agent.ts";
import { registerHtmlAgent } from "./bundles/html_agent/html_agent.ts";
import { mountCanvas } from "./bundles/canvas/canvas.ts";
import { registerCanvas } from "./bundles/canvas/canvas_agent.ts";
import { registerBackground } from "./bundles/canvas/background_agent.ts";

// The canvas page. The frontend kernel boots as a PEER of the host: it dials the host's `web_loader` (by id) over the bridge (a host-side
// `kernel_state` rooted at `.fantastic/web/`, which the operator created — no
// automation), hydrates its OWN agent tree from there (`load_tree`), and
// persists every local change back (`proxy_loader`). The canvas renders the
// LOCAL member tree; host backends are weak peers referenced by id. One shared
// frontend tree, addressed by the loader's id — no per-session id.

const LOADER = "web_loader"; // the host-side web/kernel_state, by id
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
  registerCanvas(kernel); // the compositor root and its control plane
  registerBackground(kernel); // its singleton, delete-locked background (set_bg/clear_bg/get_bg)
  // View bundles that front a host backend are real frontend agents too — a
  // no-op handler makes their records load-able; the ViewBundle renders them.
  kernel.registerBundle("terminal_view.ts", () => null);
  kernel.registerBundle("ai_view.ts", () => null);
  kernel.registerBundle("reflect_view.ts", () => null);
  kernel.registerBundle("setup_view.ts", () => null);
  kernel.registerBundle("stream_view.ts", () => null);

  // Each frontend bundle declares its role via reflect (readme=true): the view
  // CLIENTS name the host CAPABILITY + verb surface they front (an LLM weaves the
  // pairing from this + the host's capability readme — no hardcoded pairing);
  // content agents say what they hold. Direction is frontend→host: the JS knows
  // it fronts a host capability; the host stays ignorant of the frontend.
  kernel.setBundleReadme(
    "html_agent.ts",
    "Frontend HTML content agent. Holds a mutable `html` body in its record, rendered in a sandboxed frame; the injected connector relays send/emit/watch/onMessage to the kernel. Content, not a host client.",
  );
  kernel.setBundleReadme(
    "gl_agent.ts",
    "Frontend WebGL content agent. Renders its record's `gl_source` shader and reacts to events it watches by id. Content, not a host client.",
  );
  kernel.setBundleReadme(
    "terminal_view.ts",
    "HTML/xterm CLIENT for a host PTY. Fronts any agent answering the PTY verb surface (boot/write/ack/resize/interrupt/stop) and emitting output/exited, bound by `backend_id`: watches the backend's output, renders it, sends keystrokes via write.",
  );
  kernel.setBundleReadme(
    "ai_view.ts",
    "HTML chat CLIENT for a host LLM backend. Fronts any agent answering send/history/interrupt/status, bound by `backend_id`: renders streamed token/done events, sends user turns via send.",
  );
}

/** Fresh (the host namespace holds no records): seed the canvas root — the
 *  operator's first conscious agent. `boot` persists it with the first write. */
function seed(kernel: Kernel): void {
  kernel.setRoot(
    new Agent({
      id: "canvas",
      handlerModule: "canvas.ts",
      sentence: "Canvas compositor — renders the frontend's own member tree.",
    }),
  );
}

// THE BOOT, the library's `boot` composed here with one change: every host call
// goes through the USERS GATEWAY as this tab's user (`user_bridge.ts`).
const kernel = new Kernel();
registerRamStore(kernel);
register(kernel);
const inner = new WsBridge(kernel, { origin, controlEndpoint: GATEWAY });
const tab = crypto.randomUUID().replace(/-/g, "");
// the token is read ONCE, at load: a token typed in another tab takes effect here
// on this tab's next load, never mid-session
const token = readToken();
const bridge = new UserBridge(kernel, inner, { loader: LOADER, token: () => token, tab });

// WHO THIS TAB IS: the gateway says which user its token is (no token: the
// host's tokenless default). Refused → ask for a token, and nothing else runs.
const me = (await kernel.callHost(GATEWAY, { type: "reflect" })) as Record<string, unknown>;
if (typeof me["as"] !== "string") {
  const why = (me["as_error"] as Record<string, unknown> | undefined)?.["error"] ?? me["error"] ?? "no answer";
  askToken(String(why));
  await new Promise(() => {}); // the form reloads the page
}

const loader = new ProxyLoader(kernel, LOADER);
kernel.persistenceProvider = LOADER;
const records = await loader.loadTree();
if (records.length > 0) kernel.load(records);
else seed(kernel);
inner.announce();
loader.start();
bridge.start();

// THE ROOT'S BOOT MINTS THE GUID (once; it persists with the root record) — the
// canvas's identity as a PEER kernel: the bridge says `hello {guid}` with it, and
// the host's agents address this canvas as `<guid>|canvas`. `boot` hydrates the
// tree but boots no agent, so the page boots its own root.
if (kernel.rootId !== null) await kernel.send([{ target: kernel.rootId, payload: { type: "boot" } }]);

/** The socket LEG this page speaks through, in the host's own words: the
 *  parent of the loader's anchor (its own record, which `load_tree` returns
 *  first). Every backend the canvas spawns is the leg's child. */
async function legOf(): Promise<string | undefined> {
  const reply = (await kernel.send([{ target: LOADER, payload: { type: "load_tree" } }]))[0]!.result as {
    records?: Array<Record<string, unknown>>;
  };
  const anchor = (reply.records ?? []).find((r) => r["id"] === LOADER);
  const leg = anchor?.["parent_id"];
  return typeof leg === "string" && leg !== "" ? leg : undefined;
}

/** Whether this tab's user may write page agent `id` (its page grants). */
const canCall = (me["can_call"] ?? {}) as Record<string, unknown>;
function writable(id: string): boolean {
  const guid = kernel.guid ?? "";
  const up: string[] = [];
  let cur = kernel.get(id)?.parentId ?? null;
  while (cur !== null && !up.includes(cur)) {
    up.push(cur);
    cur = kernel.get(cur)?.parentId ?? null;
  }
  return writesPage(pageVerbs(canCall, guid, id, up));
}

await mountCanvas({
  kernel,
  mount: document.body,
  selfId: kernel.rootId ?? "canvas",
  leg: await legOf(),
  flush: () => loader.flush(),
  writable,
});

/** The token form: shown when the host refuses this tab. The token goes to
 *  browser storage only, then the page reloads. */
function askToken(why: string): void {
  const box = document.createElement("form");
  box.className = "token-ask";
  box.innerHTML = `<p class="why"></p><input type="password" autocomplete="off" placeholder="token"><button>open</button>`;
  (box.querySelector(".why") as HTMLElement).textContent = `This canvas wants a token — ${why}.`;
  box.style.cssText = "position:fixed;inset:0;display:grid;place-content:center;gap:10px;font:14px system-ui;color:#cde;background:#0b0b10";
  box.addEventListener("submit", (e) => {
    e.preventDefault();
    writeToken((box.querySelector("input") as HTMLInputElement).value.trim());
    location.reload();
  });
  document.body.replaceChildren(box);
  (box.querySelector("input") as HTMLInputElement).focus();
}
