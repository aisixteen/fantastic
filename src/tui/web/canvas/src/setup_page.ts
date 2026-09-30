import { Kernel, WsBridge } from "./lib.ts";
import type { Json, Payload } from "./lib.ts";

// THE BROWSER'S AI SET-UP PAGE (`setup.html`) — everything the terminal's
// set-up page does, with its OWN state (it does not drive the terminal):
// list the backends, write an AI agent's record, store a backend's key in the
// keychain, save / load / forget templates, set or clear the default.
//
// It speaks through the canvas's socket leg (`canvas_ws`, dialed as the canvas
// does, by the loader's id) — no new leg. The host side is a `setup` agent the
// page creates as a CHILD OF THE LEG when it opens and deletes when it closes
// (the host also sweeps any left behind at boot): nothing about set-up is live
// while this page is closed. The page holds no tree and persists nothing.

const LOADER = "web_loader";
const origin = (location.protocol === "https:" ? "wss://" : "ws://") + location.host;

type Rec = Record<string, unknown>;
interface Template {
  name: string;
  record: Rec;
}

const kernel = new Kernel();
new WsBridge(kernel, { origin, controlEndpoint: LOADER });

const host = (target: string, payload: Payload): Promise<Json> => kernel.callHost(target, payload);

async function legOf(): Promise<string> {
  const reply = (await kernel.send([{ target: LOADER, payload: { type: "load_tree" } }]))[0]!.result as { records?: Rec[] };
  const leg = (reply.records ?? []).find((r) => r["id"] === LOADER)?.["parent_id"];
  if (typeof leg !== "string" || leg === "") throw new Error(`the loader ${LOADER} names no parent leg`);
  return leg;
}

// ── DOM ────────────────────────────────────────────────────────────────
const css = `
  :root { color-scheme: dark; }
  body { margin: 0; background: #0f0e14; color: #e5e1ee; font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
  main { max-width: 760px; margin: 40px auto; padding: 0 20px; }
  h1 { font-size: 16px; letter-spacing: .08em; margin: 0 0 6px; }
  p.lead { color: #9a93ad; margin: 0 0 22px; }
  h2 { font-size: 12px; letter-spacing: .12em; color: #9a93ad; border-bottom: 1px solid #2a2636; padding-bottom: 6px; margin: 28px 0 12px; }
  label { display: grid; grid-template-columns: 110px 1fr; gap: 10px; align-items: center; margin: 8px 0; }
  label span { color: #9a93ad; }
  input, select, button { font: inherit; color: inherit; background: #1b1924; border: 1px solid #332e42; border-radius: 6px; padding: 7px 10px; }
  input:focus, select:focus { outline: 1px solid #b78cff; }
  button { cursor: pointer; background: #2a2340; }
  button.primary { background: #b78cff; color: #16121f; font-weight: 700; border-color: #b78cff; }
  .row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin: 10px 0; }
  .note { color: #7d768f; font-size: 12px; }
  .ok { color: #7ee2a8; } .err { color: #ff8a8a; }
  .indent { margin-left: 120px; }
  .warn { color: #ffd166; margin: 4px 0 0 120px; font-size: 13px; }
  .warn:empty { display: none; }
  ul.tpl { list-style: none; padding: 0; margin: 0; }
  ul.tpl li { display: flex; gap: 8px; align-items: center; padding: 6px 0; border-bottom: 1px dashed #2a2636; }
  ul.tpl li .name { font-weight: 700; min-width: 120px; }
  ul.tpl li .star { color: #ffd166; }
  ul.tpl li .desc { color: #9a93ad; flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
`;
document.title = "Aisixteen Fantastic · AI set-up";
const style = document.createElement("style");
style.textContent = css;
document.head.appendChild(style);

const el = <K extends keyof HTMLElementTagNameMap>(tag: K, props: Partial<HTMLElementTagNameMap[K]> = {}, ...kids: (Node | string)[]): HTMLElementTagNameMap[K] => {
  const e = document.createElement(tag);
  Object.assign(e, props);
  for (const k of kids) e.append(k);
  return e;
};
const field = (label: string, input: HTMLElement, note = ""): HTMLElement =>
  el("label", {}, el("span", {}, label), el("div", {}, input, note ? el("div", { className: "note" }, note) : ""));

const id = el("input", { placeholder: "a new agent id" });
const handler = el("select");
const model = el("input", { placeholder: "as the backend names it" });
const endpoint = el("input", { placeholder: "the URL it serves on — required" });
const mayCall = el("input", { placeholder: "agent ids, space-separated — optional" });
const write = el("button", { className: "primary", textContent: "WRITE THE RECORD" });
const keyMode = el("select");
keyMode.append(
  el("option", { value: "", textContent: "— choose —" }),
  el("option", { value: "secure", textContent: "secure — one sealed file, its master key in the keychain" }),
  el("option", { value: "plain", textContent: "plain — one 0600 text file per key" }),
);
const applyMode = el("button", { textContent: "apply" });
const keyWhere = el("div", { className: "note indent" });
const keyWarn = el("div", { className: "warn" });
const secret = el("input", { type: "password", placeholder: "the selected backend's API key" });
const storeKey = el("button", { textContent: "store the key" });
const tplName = el("input", { placeholder: "a name you choose — letters, digits, - _" });
const saveTpl = el("button", { textContent: "SAVE AS TEMPLATE" });
const list = el("ul", { className: "tpl" });
const where = el("div", { className: "note" });
const result = el("div", { className: "row" });

document.body.append(
  el(
    "main",
    {},
    el("h1", {}, "SET UP AN AI AGENT"),
    el("p", { className: "lead" }, "Nothing is filled in for you, and nothing is written until you ask."),
    result,
    el("h2", {}, "THE RECORD"),
    field("id", id),
    field("handler", handler, "a backend this build links"),
    field("model", model),
    field("endpoint", endpoint),
    field("may call", mayCall),
    el("div", { className: "row" }, write),
    el("h2", {}, "THE KEY"),
    field("keys kept", el("div", { className: "row" }, keyMode, applyMode)),
    keyWhere,
    keyWarn,
    field("key", secret, "stored where keys are kept; loaded on the next start"),
    el("div", { className: "row" }, storeKey),
    el("h2", {}, "TEMPLATES — REUSE IT IN ANY PROJECT"),
    field("name", tplName, "saves handler + model + endpoint — never the key, the id or grants"),
    el("div", { className: "row" }, saveTpl),
    list,
    where,
  ),
);

function say(ok: boolean, text: string): void {
  result.replaceChildren(el("div", { className: ok ? "ok" : "err" }, `${ok ? "✓" : "✕"} ${text}`));
}
const errOf = (r: unknown): string | null => {
  const o = r as Rec | null;
  return o && typeof o["error"] === "string" ? `${o["error"]}${typeof o["hint"] === "string" ? ` — ${o["hint"]}` : ""}` : null;
};

// ── the host-side `setup` agent: exists only while this page is open ──
let setupId: string | null = null;
async function call(verb: string, args: Rec = {}): Promise<Json> {
  if (setupId === null) throw new Error("the set-up agent is not up");
  return host(setupId, { type: verb, ...args } as Payload);
}

function missingRecord(): string[] {
  const m: string[] = [];
  if (!id.value.trim()) m.push("id");
  if (!handler.value) m.push("handler");
  if (!model.value.trim()) m.push("model");
  if (!endpoint.value.trim()) m.push("endpoint");
  return m;
}

async function refreshTemplates(): Promise<void> {
  const r = (await call("templates")) as Rec;
  const e = errOf(r);
  if (e) {
    list.replaceChildren(el("li", { className: "err" }, e));
    return;
  }
  const ts = (r["templates"] as Template[] | undefined) ?? [];
  const preferred = r["preferred"] as string | null;
  where.textContent = `in ${String(r["dir"] ?? "")}`;
  if (ts.length === 0) {
    list.replaceChildren(el("li", { className: "note" }, "— none saved yet —"));
    return;
  }
  list.replaceChildren(
    ...ts.map((t) => {
      const rec = t.record;
      const isDefault = preferred === t.name;
      const load = el("button", { textContent: "load" });
      load.onclick = (): void => {
        const hm = String(rec["handler_module"] ?? "");
        if (![...handler.options].some((o) => o.value === hm)) {
          say(false, `template \`${t.name}\` needs ${hm}, which this build does not link`);
          return;
        }
        handler.value = hm;
        model.value = String(rec["model"] ?? "");
        endpoint.value = String(rec["endpoint"] ?? "");
        say(true, `loaded template \`${t.name}\` — give it an id, then WRITE THE RECORD`);
      };
      const def = el("button", { textContent: isDefault ? "stop being the default" : "make default" });
      def.onclick = async (): Promise<void> => {
        const r2 = await call("prefer", { name: isDefault ? null : t.name });
        const e2 = errOf(r2);
        say(!e2, e2 ?? (isDefault ? "no default AI template — new AI agents start from nothing" : `\`${t.name}\` is your default for new AI agents`));
        await refreshTemplates();
      };
      const forget = el("button", { textContent: "forget" });
      forget.onclick = async (): Promise<void> => {
        const r2 = await call("forget_template", { name: t.name });
        const e2 = errOf(r2);
        say(!e2, e2 ?? `forgot template \`${t.name}\``);
        await refreshTemplates();
      };
      return el(
        "li",
        {},
        el("span", { className: "name" }, t.name),
        el("span", { className: "star" }, isDefault ? "★ default" : ""),
        el("span", { className: "desc" }, `${String(rec["handler_module"] ?? "")} · ${String(rec["model"] ?? "")} · ${String(rec["endpoint"] ?? "")}`),
        load,
        def,
        forget,
      );
    }),
  );
}

write.onclick = async (): Promise<void> => {
  const m = missingRecord();
  if (m.length > 0) return say(false, `missing: ${m.join(", ")}`);
  const r = await call("write_ai", {
    id: id.value.trim(),
    handler: handler.value,
    model: model.value.trim(),
    endpoint: endpoint.value.trim(),
    may_call: mayCall.value.split(/\s+/).filter(Boolean),
  });
  const e = errOf(r);
  if (e) return say(false, e);
  say(true, `written to ${String((r as Rec)["written"])}`);
  id.value = "";
  mayCall.value = "";
};

let chosenMode: string | null = null;
function showMode(mode: string | null, where: string): void {
  chosenMode = mode;
  keyMode.value = mode ?? "";
  keyWhere.textContent = where;
  keyWarn.textContent = mode === "plain" ? "⚠ not secured: keys are plain text — anything running as you can read them" : "";
}
async function refreshMode(): Promise<void> {
  const r = (await call("key_mode")) as Rec;
  const e = errOf(r);
  if (e) return showMode(null, e);
  showMode((r["mode"] as string | null) ?? null, String(r["where"] ?? ""));
}
applyMode.onclick = async (): Promise<void> => {
  if (!keyMode.value) return say(false, "choose plain or secure");
  const r = (await call("set_key_mode", { mode: keyMode.value })) as Rec;
  const e = errOf(r);
  say(!e, e ?? `keys are kept ${String(r["where"] ?? "")}`);
  await refreshMode();
};

storeKey.onclick = async (): Promise<void> => {
  if (chosenMode === null) return say(false, "choose where keys are kept first");
  if (!handler.value) return say(false, "pick the handler whose key this is");
  if (!secret.value) return say(false, "missing: key");
  const r = await call("store_key", { module: handler.value, secret: secret.value });
  secret.value = ""; // never kept on the page
  const e = errOf(r);
  say(!e, e ?? `key stored in ${String((r as Rec)["stored"])} — loaded on the next start`);
};

saveTpl.onclick = async (): Promise<void> => {
  const miss: string[] = [];
  if (!tplName.value.trim()) miss.push("template name");
  miss.push(...missingRecord().filter((f) => f !== "id"));
  if (miss.length > 0) return say(false, `missing: ${miss.join(", ")}`);
  const name = tplName.value.trim();
  const r = await call("save_template", {
    name,
    record: { handler_module: handler.value, model: model.value.trim(), endpoint: endpoint.value.trim() },
  });
  const e = errOf(r);
  say(!e, e ?? `saved as template \`${name}\` at ${String((r as Rec)["path"])} — load it back any time, in any project; "make default" makes it your default for new AI agents`);
  await refreshTemplates();
};

// ── open: spawn the host-side agent, fill the page; close: delete it ──
try {
  const leg = await legOf();
  const created = (await host("kernel", { type: "create_agent", parent_id: leg, handler_module: "setup.tools" } as unknown as Payload)) as Rec;
  if (typeof created["id"] !== "string") throw new Error(`could not start set-up: ${JSON.stringify(created)}`);
  setupId = created["id"];
  addEventListener("pagehide", () => {
    if (setupId !== null) void host("kernel", { type: "delete_agent", id: setupId } as unknown as Payload);
  });
  const b = ((await call("backends")) as Rec)["backends"] as string[] | undefined;
  handler.replaceChildren(el("option", { value: "", textContent: b && b.length ? `— pick one of ${b.length} —` : "— none linked —" }), ...(b ?? []).map((h) => el("option", { value: h, textContent: h })));
  await refreshMode();
  await refreshTemplates();
} catch (e) {
  say(false, (e as Error).message);
}
