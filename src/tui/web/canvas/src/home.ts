import { Agent, Kernel, ProxyLoader, WsBridge, registerRamStore } from "./lib.ts";
import { GATEWAY, UserBridge } from "./user_bridge.ts";
import { homeView } from "./bundles/home_view/home_view.ts";
import { PROTOCOL, VERSION } from "./bundles/home_view/home_model.ts";
import { readShell } from "./bundles/shell.ts";
import { bootSkin, installSkin } from "./bundles/skin/apply.ts";
import { tok } from "./bundles/skin/css.ts";

// THE HOME PAGE (`home.html`, `<body data-page="home">`): the screen of the host's
// `home` agent. It boots as the canvas page does (`main.ts`) — the page kernel, its
// bridge through `user`, its own records through the host's page loader — and
// mounts one view, `homeView`, onto the agent `home`.
//
// Its records are the HOME PAGE's own (`<home>/app/web/agents/home_page/`, through
// `web_loader`): its root `home_page`, and what later phases put under it. The
// open pages and the selection are never page records: they are the `home`
// agent's state, so every tab shows the same ones and a restart keeps them.
//
// The page is no peer the host calls: it never announces itself.

const LOADER = "web_loader";
const HOME = "home";
const origin = (location.protocol === "https:" ? "wss://" : "ws://") + location.host;

/** Said LOUD, in place of the Home, and the page goes no further. */
async function stop(why: string): Promise<never> {
  console.error(`[home] ${why}`);
  const box = document.createElement("p");
  box.className = "home-refused";
  box.textContent = why;
  box.style.cssText = `position:fixed;inset:0;margin:0;padding:24px;display:grid;place-content:center;text-align:center;font:14px ${tok("font.family.sans")};color:${tok("color.text.cool")};background:${tok("color.page.bg")}`;
  document.body.replaceChildren(box);
  return new Promise<never>(() => {});
}

// THE SHELL'S FLAG first: a malformed one is the shell's bug, said in place of the Home.
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

// THE BOOT, as the canvas page's: every host call goes through `user`.
const kernel = new Kernel();
registerRamStore(kernel);
kernel.registerBundle("home_view.ts", () => null);
const inner = new WsBridge(kernel, { origin, controlEndpoint: GATEWAY });
new UserBridge(kernel, inner);

// IS THIS TAB LET IN
const me = (await kernel.callHost("kernel", { type: "reflect", tree: "none" } as never)) as Record<string, unknown> | null;
if (me?.["reason"] === "unauthorized") await stop(`The host refused this page — ${String(me["error"] ?? "no answer")}.`);

// DOES THIS HOST SERVE A HOME, in the protocol this page speaks
const home = (await kernel.callHost(HOME, { type: "reflect" } as never)) as Record<string, unknown> | null;
if (home === null || typeof home !== "object") await stop(`This host serves no home: its reflect answered ${JSON.stringify(home)}.`);
if (typeof home!["error"] === "string") await stop(`This host serves no home: ${home!["error"]}.`);
if (home!["protocol"] !== PROTOCOL || home!["version"] !== VERSION) {
  await stop(`This home speaks ${String(home!["protocol"])} v${String(home!["version"])}; this page speaks ${PROTOCOL} v${VERSION}.`);
}

// THE PAGE'S RECORDS, through the home's page loader. Its refusal is loud: the
// loader's own `loadTree` reads a refusal as an empty tree.
const read = (await kernel.send([{ target: LOADER, payload: { type: "load_tree" } }]))[0]!.result as Record<string, unknown> | null;
if (read !== null && typeof read === "object" && typeof read["error"] === "string") {
  await stop(`The home's page loader refused this page's records — ${read["error"]}.`);
}
const loader = new ProxyLoader(kernel, LOADER);
kernel.persistenceProvider = LOADER;
const records = await loader.loadTree();
if (records.length > 0) kernel.load(records);
else kernel.setRoot(new Agent({ id: "home_page", handlerModule: "home_view.ts", sentence: "The Home page: the view of the host's home agent." }));
loader.start();
// the root's boot mints the page kernel's guid, once; it persists with the root record
if (kernel.rootId !== null) await kernel.send([{ target: kernel.rootId, payload: { type: "boot" } }]);

await homeView.mount({ kernel, mount: document.body, selfId: kernel.rootId!, backend: HOME, close: () => {} });
