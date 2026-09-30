import { Host } from "../../lib.ts";
import type { Json, Payload } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";

// THE AI SET-UP, AS A CANVAS CELL — everything the terminal's set-up page does,
// with its OWN state: list the backends, write an AI agent's record, choose where
// keys are kept and store one, save / load / forget templates, set or clear the
// default. ONE at a time on a canvas (opening it again moves to it).
//
// It fronts a `setup.tools` agent the canvas spawns under the leg for it — the
// pair model: the view lands first, then the agent, then the bind; ✕ removes
// both. The host sweeps a set-up agent left behind at boot, so after a restart
// the cell shows its backend lost and ⟳ respawns it. The cell persists nothing
// of its own.

type Rec = Record<string, unknown>;
/** One field of a backend's record schema, as the set-up agent hands it. */
interface SchemaField {
  name: string;
  type: string;
  required: boolean;
  enum: string[];
  default: unknown;
  hint: string;
}
interface BackendInfo {
  module: string;
  key_var: string | null;
  fields: SchemaField[];
}
/** A value as an input shows it. */
const textOf = (v: unknown): string =>
  v === null || v === undefined ? "" : Array.isArray(v) ? v.join(", ") : typeof v === "object" ? JSON.stringify(v) : String(v);
interface Template {
  name: string;
  record: Rec;
}

/** Every verb the cell calls on its set-up agent. */
const VERBS = ["backends", "write_ai", "key_mode", "set_key_mode", "store_key", "templates", "save_template", "forget_template", "prefer", "reflect"] as const;

const CSS = `
  .setup-view { margin: 0; background: #0f0e14; color: #e5e1ee; font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; }
  .setup-view { padding: 14px 18px; height: 100%; overflow: auto; box-sizing: border-box; }
  .setup-view h1 { font-size: 16px; letter-spacing: .08em; margin: 0 0 6px; }
  .setup-view p.lead { color: #9a93ad; margin: 0 0 22px; }
  .setup-view h2 { font-size: 12px; letter-spacing: .12em; color: #9a93ad; border-bottom: 1px solid #2a2636; padding-bottom: 6px; margin: 28px 0 12px; }
  .setup-view label { display: grid; grid-template-columns: 110px 1fr; gap: 10px; align-items: center; margin: 8px 0; }
  .setup-view label span { color: #9a93ad; }
  .setup-view input, .setup-view select, .setup-view button { font: inherit; color: inherit; background: #1b1924; border: 1px solid #332e42; border-radius: 6px; padding: 7px 10px; }
  .setup-view input:focus, .setup-view select:focus { outline: 1px solid #b78cff; }
  .setup-view button { cursor: pointer; background: #2a2340; }
  .setup-view button.primary { background: #b78cff; color: #16121f; font-weight: 700; border-color: #b78cff; }
  .setup-view .row { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin: 10px 0; }
  .setup-view .note { color: #7d768f; font-size: 12px; }
  .setup-view .ok { color: #7ee2a8; } .err { color: #ff8a8a; }
  .setup-view .indent { margin-left: 120px; }
  .setup-view .warn { color: #ffd166; margin: 4px 0 0 120px; font-size: 13px; }
  .setup-view .warn:empty { display: none; }
  .setup-view ul.tpl { list-style: none; padding: 0; margin: 0; }
  .setup-view ul.tpl li { display: flex; gap: 8px; align-items: center; padding: 6px 0; border-bottom: 1px dashed #2a2636; }
  .setup-view ul.tpl li .name { font-weight: 700; min-width: 120px; }
  .setup-view ul.tpl li .star { color: #ffd166; }
  .setup-view ul.tpl li .desc { color: #9a93ad; flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
`;

function ensureStyle(): void {
  if (document.getElementById("fantastic-setup-style")) return;
  const s = document.createElement("style");
  s.id = "fantastic-setup-style";
  s.textContent = CSS;
  document.head.appendChild(s);
}

export const setupView: ViewBundle = {
  handles: ["setup_view.ts"],
  fronts: "setup.tools",
  grants: (backend) => ({ [backend]: VERBS }),
  async mount(ctx: ViewContext): Promise<ViewHandle> {
    ensureStyle();
    const root = ctx.mount;
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
    // THE BACKEND'S OWN FIELDS, from its schema: required first (empty —
    // nothing is guessed), then the optional ones at the backend's defaults.
    const requiredBox = el("div");
    const optionalBox = el("div");
    let backends: BackendInfo[] = [];
    const inputs = new Map<string, HTMLInputElement | HTMLSelectElement>();
    const backendOf = (): BackendInfo | undefined => backends.find((b) => b.module === handler.value);
    const renderFields = (): void => {
      inputs.clear();
      const b = backendOf();
      const make = (f: SchemaField): HTMLElement => {
        let input: HTMLInputElement | HTMLSelectElement;
        if (f.type === "enum") {
          const sel = el("select");
          sel.append(el("option", { value: "", textContent: f.required ? "— choose —" : "— not set —" }), ...f.enum.map((o) => el("option", { value: o, textContent: o })));
          sel.value = textOf(f.default);
          input = sel;
        } else {
          input = el("input", { placeholder: f.required ? `${f.type} — required` : `${f.type} — optional`, value: textOf(f.default) });
        }
        inputs.set(f.name, input);
        const note = `${f.hint}${f.default !== null && f.default !== undefined ? ` (backend default ${textOf(f.default)})` : ""}`;
        return field(f.name, input, note);
      };
      requiredBox.replaceChildren(...(b?.fields.filter((f) => f.required).map(make) ?? []));
      const opt = b?.fields.filter((f) => !f.required) ?? [];
      optionalBox.replaceChildren(...(opt.length ? [el("h2", {}, "OPTIONAL — EACH STARTS AT THE BACKEND'S OWN DEFAULT"), ...opt.map(make)] : []));
    };
    handler.onchange = (): void => renderFields();
    // an untouched default stays the BACKEND's to apply — not sent
    const fieldTexts = (): Record<string, string> => {
      const defaults = new Map((backendOf()?.fields ?? []).map((f) => [f.name, textOf(f.default)]));
      return Object.fromEntries([...inputs].map(([k, i]) => [k, i.value.trim()]).filter(([k, v]) => !(defaults.get(k!) !== "" && defaults.get(k!) === v)));
    };
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

    root.append(
      el(
        "div",
        { className: "setup-view" },
        el("h1", {}, "SET UP AN AI AGENT"),
        el("p", { className: "lead" }, "Nothing is filled in for you, and nothing is written until you ask."),
        result,
        el("h2", {}, "THE RECORD"),
        field("id", id),
        field("handler", handler, "a backend this build links"),
        requiredBox,
        field("may call", mayCall),
        el("div", { className: "row" }, write),
        optionalBox,
        el("h2", {}, "THE KEY"),
        field("keys kept", el("div", { className: "row" }, keyMode, applyMode)),
        keyWhere,
        keyWarn,
        field("key", secret, "stored where keys are kept; loaded on the next start"),
        el("div", { className: "row" }, storeKey),
        el("h2", {}, "TEMPLATES — REUSE IT IN ANY PROJECT"),
        field("name", tplName, "saves the backend + every field set — never the key, the id or grants"),
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

    const host = new Host(ctx.kernel, ctx.selfId);
    const call = (verb: string, args: Rec = {}): Promise<Json> => host.call(ctx.backend, { type: verb, ...args } as Payload);

    function missingRecord(): string[] {
      const m: string[] = [];
      if (!id.value.trim()) m.push("id");
      if (!handler.value) m.push("handler");
      for (const f of backendOf()?.fields ?? []) if (f.required && !inputs.get(f.name)?.value.trim()) m.push(f.name);
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
            renderFields();
            for (const [k, i] of inputs) if (k in rec) i.value = textOf(rec[k]);
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
            el("span", { className: "desc" }, `${String(rec["handler_module"] ?? "")} · ${String(rec["model"] ?? "")}`),
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
        fields: fieldTexts(),
        may_call: mayCall.value.split(/\s+/).filter(Boolean),
      });
      const e = errOf(r);
      if (e) return say(false, e);
      say(true, `written to ${String((r as Rec)["written"])} — the backend and its fields stay, to save as a template or write another`);
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
        handler: handler.value,
        fields: fieldTexts(),
      });
      const e = errOf(r);
      say(!e, e ?? `saved as template \`${name}\` at ${String((r as Rec)["path"])} — load it back any time, in any project; "make default" makes it your default for new AI agents`);
      await refreshTemplates();
    };

    // ── open: fill the cell from the set-up agent the canvas spawned for it ──
    try {
      backends = (((await call("backends")) as Rec)["backends"] as BackendInfo[] | undefined) ?? [];
      handler.replaceChildren(
        el("option", { value: "", textContent: backends.length ? `— pick one of ${backends.length} —` : "— none linked —" }),
        ...backends.map((b) => el("option", { value: b.module, textContent: b.module })),
      );
      await refreshMode();
      await refreshTemplates();
    } catch (e) {
      say(false, (e as Error).message);
    }
    return { unmount: (): void => root.replaceChildren() };
  },
};
