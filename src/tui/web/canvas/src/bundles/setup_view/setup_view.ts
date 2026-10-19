import { Host } from "../../lib.ts";
import type { Json, Payload } from "../../lib.ts";
import { tint, tok } from "../skin/css.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { PROVIDERS, autoName, nameOk, providerOf, savedKeyFor } from "./wizard.ts";

// THE AI SET-UP WIZARD, AS A CANVAS CELL (owner 2026-10-11) — two pages:
//   1. a provider: a SAVED one (its agent is made at once) or a NEW one of the
//      four (responses, anthropic, ollama, vllm);
//   2. the new one's details: a name (auto), its URL (filled), its model, its key
//      (a stored key for that host, a new one pasted, or none) and the backend's
//      own optional fields — then "create & open chat" saves it as a saved set-up
//      (the first becomes the default) and opens the agent's chat.
// Every page has SKIP: an AI cell with no AI yet, which says how to set it up.
// THE SETTINGS of an AI agent are page 2 for that agent (`edit_ai` on the
// record): its fields, changed live on its next turn; `‹ other set-ups` is page 1
// for its cell — another saved set-up, or a new provider, rebinds that cell.
//
// It fronts a `setup.tools` agent the canvas spawns in its view's backend folder
// for it (the pair model); the host sweeps one left behind at boot, so after a
// restart the cell shows its backend lost and ⟳ respawns it.

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
  /** Whether its record takes a key (`true` = required); null = none. */
  key: boolean | null;
  fields: SchemaField[];
}
interface Saved {
  name: string;
  record: Rec;
}
/** A value as an input shows it. */
const textOf = (v: unknown): string =>
  v === null || v === undefined ? "" : Array.isArray(v) ? v.join(", ") : typeof v === "object" ? JSON.stringify(v) : String(v);

/** Every verb the cell calls on its set-up agent. */
const VERBS = ["backends", "keys", "ai_settings", "configure_ai", "key_mode", "set_key_mode", "store_key", "saved", "save", "prefer", "reflect"] as const;
/** Shown on their own rows (page 2's top), never under "advanced". */
const MAIN = new Set(["endpoint", "model"]);

const CSS = `
  .wiz { height: 100%; box-sizing: border-box; overflow: auto; padding: 22px 24px 18px; background: ${tok("color.surface.panel")}; color: ${tok("color.text.primary")};
    font: 14px/1.5 ${tok("font.family.sans")}; display: flex; flex-direction: column; }
  .wiz .top { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; }
  .wiz h1 { margin: 0; font-size: 19px; font-weight: 650; letter-spacing: -0.2px; }
  .wiz .step { color: ${tok("color.text.subtle")}; font: 600 11px ${tok("font.family.mono")}; letter-spacing: .12em; }
  .wiz .lead { color: ${tok("color.text.muted")}; margin: 8px 0 18px; }
  .wiz .tip { margin: -6px 0 18px; padding: 10px 14px; border-radius: ${tok("radius.panel")}; background: ${tint("color.accent.base", 12)};
    box-shadow: inset 0 0 0 1px ${tint("color.accent.base", 40)}; color: ${tok("color.text.primary")}; }
  .wiz .tip b { color: ${tok("color.accent.soft")}; }
  .wiz .tip code { font: 600 12.5px ${tok("font.family.mono")}; color: ${tok("color.accent.text")}; padding: 1px 6px; border-radius: ${tok("radius.key")}; background: ${tint("color.accent.base", 20)}; }
  .wiz h2 { margin: 18px 0 8px; color: ${tok("color.text.subtle")}; font: 600 11px ${tok("font.family.mono")}; letter-spacing: .14em; }
  .wiz .saved { display: flex; flex-direction: column; gap: 6px; }
  .wiz .saved-row { display: flex; gap: 6px; }
  .wiz .saved-row > button:first-child { flex: 1; min-width: 0; }
  .wiz .saved button { display: flex; align-items: baseline; gap: 10px; text-align: left; width: 100%; padding: 10px 12px; }
  .wiz .saved button.edit { width: auto; flex: 0 0 auto; align-items: center; color: ${tok("color.text.muted")}; }
  .wiz .saved .name { font-weight: 650; }
  .wiz .saved .star { color: ${tok("color.status.warn")}; font-size: 12px; }
  .wiz .saved .desc { color: ${tok("color.text.subtle")}; font-size: 12.5px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .wiz .none { color: ${tok("color.text.faint")}; font-size: 13px; }
  .wiz .tiles { display: grid; grid-template-columns: repeat(auto-fill, minmax(118px, 1fr)); gap: 8px; }
  .wiz .tiles button { padding: 16px 10px; font-weight: 650; font-size: 14px; }
  .wiz .tiles button:disabled { opacity: .4; cursor: default; }
  .wiz .tiles button small { display: block; font-weight: 400; color: ${tok("color.text.subtle")}; font-size: 11.5px; margin-top: 2px; }
  .wiz label { display: grid; grid-template-columns: 92px 1fr; gap: 12px; align-items: start; margin: 10px 0; }
  .wiz label > span { color: ${tok("color.text.muted")}; padding-top: 8px; font-size: 13px; }
  .wiz .hint { color: ${tok("color.text.faint")}; font-size: 12px; margin-top: 3px; }
  .wiz input, .wiz select, .wiz button { font: inherit; color: inherit; background: ${tok("color.surface.field")}; border: 1px solid ${tok("color.line.button")}; border-radius: ${tok("radius.card")}; padding: 8px 11px; }
  .wiz input, .wiz select { width: 100%; box-sizing: border-box; }
  .wiz input:focus, .wiz select:focus { outline: 2px solid ${tint("color.accent.base", 55)}; outline-offset: -1px; }
  .wiz button { cursor: pointer; background: ${tok("color.surface.button")}; }
  .wiz button:hover:not(:disabled) { border-color: ${tok("color.accent.base")}; }
  .wiz button.primary { background: ${tok("color.accent.base")}; border-color: ${tok("color.accent.base")}; color: ${tok("color.accent.on")}; font-weight: 700; }
  .wiz .row { display: flex; gap: 8px; align-items: center; }
  .wiz .grow { flex: 1; }
  .wiz details { margin-top: 12px; }
  .wiz summary { cursor: pointer; color: ${tok("color.text.muted")}; font-size: 13px; }
  .wiz .foot { margin-top: auto; padding-top: 18px; display: flex; gap: 8px; align-items: center; }
  .wiz .say { min-height: 20px; font-size: 13px; margin-top: 10px; }
  .wiz .ok { color: ${tok("color.status.ok")}; } .wiz .err { color: ${tok("color.status.error")}; }
  .wiz .warn { color: ${tok("color.status.warn")}; font-size: 12px; margin-top: 4px; }
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
    const field = (label: string, input: HTMLElement, hint = ""): HTMLElement =>
      el("label", {}, el("span", {}, label), el("div", {}, input, hint ? el("div", { className: "hint" }, hint) : ""));
    const host = new Host(ctx.kernel, ctx.selfId);
    const call = (verb: string, args: Rec = {}): Promise<Json> => host.call(ctx.backend, { type: verb, ...args } as Payload);
    const errOf = (r: unknown): string | null => {
      const o = r as Rec | null;
      if (o === null || typeof o !== "object") return "no answer";
      return typeof o["error"] === "string" ? `${o["error"]}${typeof o["hint"] === "string" && o["hint"] ? ` — ${o["hint"]}` : ""}` : null;
    };
    const rec = ctx.record ?? {};
    const targetCell = typeof rec["target_cell"] === "string" ? (rec["target_cell"] as string) : undefined;
    const editAi = typeof rec["edit_ai"] === "string" ? (rec["edit_ai"] as string) : undefined;

    const wiz = el("div", { className: "wiz" });
    root.replaceChildren(wiz);
    const sayEl = el("div", { className: "say" });
    const say = (ok: boolean, text: string): void => sayEl.replaceChildren(el("span", { className: ok ? "ok" : "err" }, `${ok ? "✓" : "✕"} ${text}`));
    const button = (text: string, onClick: () => void, cls = ""): HTMLButtonElement => {
      const b = el("button", { textContent: text, className: cls });
      b.onclick = onClick;
      return b;
    };
    const skip = (): HTMLButtonElement =>
      button("skip", () => {
        if (targetCell !== undefined || editAi !== undefined) ctx.close();
        else ctx.actions?.skipAi(ctx.selfId);
      });

    let backends: BackendInfo[] = [];
    let saved: Saved[] = [];
    let preferred: string | null = null;
    let keys: Array<{ ref: string; host: string }> = [];
    let keyMode: string | null = null;
    const loadAll = async (): Promise<string | null> => {
      const [b, t, k] = await Promise.all([call("backends"), call("saved"), call("keys")]);
      const e = errOf(b) ?? errOf(t) ?? errOf(k);
      if (e) return e;
      backends = ((b as Rec)["backends"] as BackendInfo[] | undefined) ?? [];
      saved = ((t as Rec)["saved"] as Saved[] | undefined) ?? [];
      preferred = ((t as Rec)["preferred"] as string | null) ?? null;
      keys = ((k as Rec)["keys"] as Array<{ ref: string; host: string }> | undefined) ?? [];
      keyMode = ((k as Rec)["mode"] as string | null) ?? null;
      return null;
    };

    // ── page 1: a provider ──
    const page1 = (): void => {
      const ai = new Set(backends.map((b) => b.module));
      const aiSaved = saved.filter((t) => ai.has(String(t.record["handler_module"] ?? "")));
      wiz.replaceChildren(
        el("div", { className: "top" }, el("h1", {}, "New AI agent"), el("span", { className: "step" }, "1 / 2")),
        el("p", { className: "lead" }, "You can set everything up manually, or let your coding agent do it."),
        el(
          "div",
          { className: "tip" },
          el("b", {}, "Tip: "),
          "double-click the canvas, start your favorite coding agent, ask it to ",
          el("code", {}, "lookaround"),
          ", then ask it to set up an AI agent.",
        ),
        el("h2", {}, "SAVED"),
        aiSaved.length === 0
          ? el("div", { className: "none" }, "Nothing saved yet — pick a provider below.")
          : el(
              "div",
              { className: "saved" },
              ...aiSaved.map((t) => {
                const b = el(
                  "button",
                  {},
                  el("span", { className: "name" }, t.name),
                  el("span", { className: "star" }, t.name === preferred ? "★ default" : ""),
                  el("span", { className: "desc" }, `${providerOf(String(t.record["handler_module"]))} · ${textOf(t.record["model"])} · ${textOf(t.record["endpoint"])}`),
                );
                b.onclick = (): void => void ctx.actions?.spawnAi(t.name, ctx.selfId, targetCell);
                // EDIT a saved provider (owner 2026-10-12): its fields, saved back under its name
                const edit = button("edit", () => page2(String(t.record["handler_module"]), { name: t.name, fields: t.record }, true), "edit");
                edit.title = "edit this saved provider — agents made from it later use the new values";
                return el("div", { className: "saved-row" }, b, edit);
              }),
            ),
        el("h2", {}, "NEW"),
        el(
          "div",
          { className: "tiles" },
          ...PROVIDERS.map((p) => {
            const b = el("button", {}, p.label, el("small", {}, ai.has(p.module) ? "" : "not in this build"));
            b.disabled = !ai.has(p.module);
            b.onclick = (): void => page2(p.module);
            return b;
          }),
        ),
        sayEl,
        el("div", { className: "foot" }, el("span", { className: "grow" }), skip()),
      );
    };

    // ── page 2: the details (a new provider, or an agent's settings) ──
    const page2 = (module: string, current?: { name: string; fields: Rec }, editSaved = false): void => {
      const b = backends.find((x) => x.module === module);
      if (b === undefined) return say(false, `this build links no ${module}`);
      const provider = PROVIDERS.find((p) => p.module === module);
      const editing = current !== undefined;
      const name = el("input", { value: current?.name ?? autoName(module, saved.map((t) => t.name)) });
      name.disabled = editing;
      const schemaOf = (n: string): SchemaField | undefined => b.fields.find((f) => f.name === n);
      const start = (n: string): string => textOf(current?.fields[n] ?? (n === "endpoint" ? provider?.endpoint : schemaOf(n)?.default));
      const url = el("input", { value: start("endpoint") });
      const model = el("input", { value: start("model"), placeholder: schemaOf("model")?.required ? "required" : "the backend's default" });
      // the key: a stored one for this host, a new one pasted, or none
      const source = el("select");
      const keyBox = el("div");
      const secret = el("input", { type: "password", placeholder: "paste the API key" });
      const keep = el("select");
      keep.append(el("option", { value: "secure", textContent: "the keychain (secure)" }), el("option", { value: "plain", textContent: "a 0600 text file (plain)" }));
      const keyNeeded = b.key !== null;
      // the key source follows the URL (a saved key for its host, when there is
      // one) until the user picks one
      let picked = false;
      const fillSources = (): void => {
        const fit = savedKeyFor(keys, module, url.value.trim());
        const was = picked ? source.value : "";
        source.replaceChildren(
          ...(fit !== null ? [el("option", { value: "saved", textContent: `saved key · ${fit}` })] : []),
          el("option", { value: "new", textContent: "paste a new key" }),
          el("option", { value: "none", textContent: "no key" }),
        );
        source.value = [...source.options].some((o) => o.value === was) ? was : fit !== null ? "saved" : b.key === true ? "new" : "none";
        renderKey();
      };
      const renderKey = (): void => {
        keyBox.replaceChildren(
          ...(source.value === "new"
            ? [
                field("value", secret, "bound to the URL's host; never shown again"),
                ...(keyMode === null ? [field("keep it in", keep, "chosen once, for every key")] : []),
              ]
            : []),
        );
      };
      url.oninput = (): void => fillSources();
      source.onchange = (): void => {
        picked = true;
        renderKey();
      };
      // the backend's optional fields, at its own defaults
      const extra = new Map<string, HTMLInputElement | HTMLSelectElement>();
      const own = b.fields.filter((f) => !MAIN.has(f.name) && f.type !== "credentials" && f.name !== "memory_store_id");
      // the backend's REQUIRED fields on the page (nothing guessed), the rest under "advanced"
      const required = own.filter((f) => f.required);
      const optional = own.filter((f) => !f.required);
      const input = (f: SchemaField): HTMLElement => {
          let input: HTMLInputElement | HTMLSelectElement;
          if (f.type === "enum") {
            const sel = el("select");
            sel.append(el("option", { value: "", textContent: f.required ? "— choose —" : "— the default —" }), ...f.enum.map((o) => el("option", { value: o, textContent: o })));
            sel.value = textOf(current?.fields[f.name] ?? f.default);
            input = sel;
          } else input = el("input", { value: textOf(current?.fields[f.name] ?? f.default), placeholder: f.required ? `${f.type} — required` : f.type });
          extra.set(f.name, input);
          return field(f.name.replaceAll("_", " "), input, f.hint);
      };
      const requiredEls = required.map(input);
      const advanced = el("details", {}, el("summary", {}, "advanced — each starts at the backend's own default"), ...optional.map(input));
      // an untouched default stays the backend's to apply — not sent
      const fieldTexts = (): Record<string, string> => {
        const out: Record<string, string> = { endpoint: url.value.trim() };
        if (model.value.trim()) out["model"] = model.value.trim();
        for (const [k, i] of extra) {
          const v = i.value.trim();
          const d = textOf(schemaOf(k)?.default);
          if (v !== "" && v !== d) out[k] = v;
        }
        return out;
      };
      const storeKey = async (): Promise<string | null> => {
        if (source.value !== "new") return null;
        if (!secret.value) return "paste the key, or pick “no key”";
        if (keyMode === null) {
          const m = await call("set_key_mode", { mode: keep.value });
          const e = errOf(m);
          if (e) return e;
          keyMode = keep.value;
        }
        const r = await call("store_key", { module, endpoint: url.value.trim(), secret: secret.value });
        secret.value = ""; // never kept on the page
        return errOf(r);
      };
      const missing = (): string[] => {
        const m: string[] = [];
        if (!editing && !nameOk(name.value.trim())) m.push("a name (letters, digits, - and _)");
        if (!url.value.trim()) m.push("the URL");
        else if (!/^https?:\/\//i.test(url.value.trim())) m.push("a URL starting with http:// or https://");
        if (schemaOf("model")?.required && !model.value.trim()) m.push("the model");
        for (const f of required) if (!extra.get(f.name)?.value.trim()) m.push(f.name.replaceAll("_", " "));
        if (keyNeeded && b.key === true && source.value === "none") m.push("a key");
        return m;
      };
      const go = button(editSaved ? "save" : editing ? "save" : "create & open chat", () => void submit(), "primary");
      const submit = async (): Promise<void> => {
        const m = missing();
        if (m.length > 0) return say(false, `missing: ${m.join(", ")}`);
        go.disabled = true;
        try {
          const ke = await storeKey();
          if (ke) return say(false, ke);
          if (editSaved) {
            const r = await call("save", { name: current!.name, handler: module, fields: fieldTexts() });
            const e = errOf(r);
            if (e) return say(false, e);
            const le = await loadAll();
            if (le) return say(false, le);
            page1();
            say(true, `saved \`${current!.name}\` — agents made from it from now on use these values`);
            return;
          }
          if (editing) {
            const r = await call("configure_ai", { id: editAi, fields: fieldTexts() });
            const e = errOf(r);
            if (e) return say(false, e);
            say(true, "saved — live on its next turn");
            setTimeout(() => ctx.close(), 900);
            return;
          }
          const t = name.value.trim();
          const r = await call("save", { name: t, handler: module, fields: fieldTexts() });
          const e = errOf(r);
          if (e) return say(false, e);
          if (preferred === null) await call("prefer", { name: t });
          await ctx.actions?.spawnAi(t, ctx.selfId, targetCell);
        } finally {
          go.disabled = false;
        }
      };
      fillSources();
      wiz.replaceChildren(
        el(
          "div",
          { className: "top" },
          el("h1", {}, editSaved ? `Edit saved · ${current!.name}` : editing ? `Settings · ${current!.name}` : `New AI agent · ${provider?.label ?? providerOf(module)}`),
          el("span", { className: "step" }, editing ? providerOf(module) : "2 / 2"),
        ),
        el("p", { className: "lead" }, editSaved ? "Agents made from it from now on use these values; agents already running keep theirs (change those in their ⚙ settings)." : editing ? "Changes are live on the agent's next turn." : "Check the values, then create it — you can change them later in its ⚙ settings."),
        field("name", name, editing ? "" : "a name for reuse — it appears under SAVED"),
        field("URL", url, provider ? "filled for you — change it for your own server" : ""),
        field("model", model, schemaOf("model")?.hint ?? ""),
        ...requiredEls,
        ...(keyNeeded ? [field("key", source), keyBox] : []),
        ...(optional.length ? [advanced] : []),
        sayEl,
        // an agent's settings: `‹ other set-ups` is page 1 FOR this cell — a saved
        // set-up or a new provider rebinds it (owner 2026-10-12: the set-up is the
        // AI agent's option, not a menu tile)
        el("div", { className: "foot" }, button(editing && !editSaved ? "‹ other set-ups" : "‹ back", () => page1()), el("span", { className: "grow" }), ...(editSaved ? [] : [skip()]), go),
      );
    };

    // ── open ──
    try {
      const e = await loadAll();
      if (e) {
        wiz.replaceChildren(el("h1", {}, "New AI agent"), sayEl);
        say(false, e);
      } else if (editAi !== undefined) {
        const r = (await call("ai_settings", { id: editAi })) as Rec;
        const e2 = errOf(r);
        if (e2) {
          wiz.replaceChildren(el("h1", {}, "Settings"), sayEl);
          say(false, e2);
        } else page2(String(r["handler"]), { name: textOf(r["saved"]) || editAi, fields: (r["fields"] as Rec) ?? {} });
      } else page1();
    } catch (err) {
      wiz.replaceChildren(el("h1", {}, "New AI agent"), sayEl);
      say(false, (err as Error).message);
    }
    return { unmount: (): void => root.replaceChildren() };
  },
};
