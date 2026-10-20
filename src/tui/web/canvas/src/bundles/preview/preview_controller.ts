import type { Handler, Json } from "../../lib.ts";
import { str } from "../../lib.ts";
import { bytesOf, isRefusal, readAll, readBytes, said } from "../files/file_bridge.ts";
import type { Change, FileBridge, Refusal, WatchHub } from "../files/file_bridge.ts";
import type { AgentIndex } from "../files/agent_index.ts";
import type { Selection } from "../files/files_deps.ts";
import { inRoot, nameOf, norm, parentOf, wire } from "../files/paths.ts";
import type { Entry } from "../files/tree_model.ts";
import { EditorModel } from "./editor_model.ts";
import type { EditorEffect } from "./editor_model.ts";
import { IMAGE_MAX, PAGE, PDF_MAX, agentTarget, modeFor, pageText, refine, targetOf, why } from "./preview_model.ts";
import type { Binding, Mode } from "./preview_model.ts";

// THE PREVIEWER'S CONTROLLER — what the preview cell does that is not drawing:
// what it is bound to, the file it loads and holds live, the editor and its
// saves, and the verbs an AI drives it with. No DOM; the cell draws from it.

export type Content =
  | { k: "none" }
  | { k: "dir"; entries: Entry[] }
  | { k: "text" }
  | { k: "image" | "pdf"; bytes: Uint8Array }
  | { k: "large"; size: number; page: number; text: string }
  | { k: "binary"; size: number }
  | { k: "error"; error: string }
  | { k: "deleted" }
  | { k: "agent"; agent: string };

export interface PreviewMeta {
  follow: boolean;
  pinned: string | null;
  mode: "file" | "agent";
  path: string | null;
}

export interface PreviewCtlOptions {
  bridge: FileBridge;
  hub: WatchHub;
  selfId: string;
  index: AgentIndex;
  /** keep the binding on the cell's record */
  persist(m: PreviewMeta): void;
  /** the tree's selection now */
  selection(): Selection | null;
}

export class PreviewController {
  readonly o: PreviewCtlOptions;
  binding: Binding = { follow: true };
  fileMode: "file" | "agent" = "file";
  /** the file or folder shown (null: an agent with no folder, or nothing) */
  path: string | null = null;
  agent: string | null = null;
  mode: Mode = "text";
  size = 0;
  content: Content = { k: "none" };
  editing = false;
  readonly editor = new EditorModel();
  /** the file was removed on disk (it is held, so it shows again if it comes back) */
  gone = false;
  /** a one-line note for the person: unsaved edits kept, a watch refused, … */
  notice: string | null = null;
  /** markdown shown as its source */
  source = false;
  /** the folder of this path is an agent (the ⇄ chip applies) */
  isAgent = false;
  private held: string | null = null;
  private seq = 0;
  /** counts the pages asked for: only the latest one's reply is applied */
  private pageAsk = 0;
  /** counts the text reads of a followed file: only the latest one is applied */
  private reads = 0;
  private listener: (() => void) | null = null;
  private disposed = false;
  /** the last bridge refusal of a save, undo or read, for the verbs to pass on */
  lastRefusal: Refusal | null = null;

  constructor(o: PreviewCtlOptions) {
    this.o = o;
  }

  onChange(fn: () => void): void {
    this.listener = fn;
  }

  private changed(): void {
    if (!this.disposed) this.listener?.();
  }

  restore(meta: Partial<PreviewMeta>): void {
    if (typeof meta.pinned === "string" && inRoot(meta.pinned)) this.binding = { pinned: norm(meta.pinned) };
    else if (meta.follow === false && typeof meta.path === "string" && inRoot(meta.path)) this.binding = { pinned: norm(meta.path) };
    if (meta.mode === "agent") this.fileMode = "agent";
  }

  private persist(): void {
    this.o.persist({ follow: "follow" in this.binding, pinned: "pinned" in this.binding ? this.binding.pinned : null, mode: this.fileMode, path: this.path });
  }

  /** The `title` line: the path and the mode. */
  title(): string {
    if (this.content.k === "agent") return `${this.agent ?? ""} · agent`;
    if (this.path === null) return "nothing selected";
    return `${this.path === "" ? "project" : this.path} · ${this.mode}`;
  }

  /** The shown path is held on the bridge: changes on disk reach this preview. */
  isLive(): boolean {
    return this.held !== null;
  }

  get dirty(): boolean {
    return this.content.k === "text" && this.editor.isDirty();
  }

  /** Why this cannot be edited ("" when it can). */
  whyNot(): string {
    if (this.content.k === "agent") return why(this.mode, true);
    if (this.path === null) return "nothing is shown";
    if (this.gone) return "the file was deleted on disk";
    return why(this.mode);
  }

  canEdit(): boolean {
    return this.whyNot() === "";
  }

  // ─── what to show ─────────────────────────────────────────────────────

  private target(): { path: string | null; agent: string | null; agentMode: boolean; isAgent: boolean } {
    const sel = this.o.selection();
    const base = targetOf(this.binding, sel?.path ?? null);
    if (base === null) {
      const only = "pinned" in this.binding ? null : (sel?.agent ?? null);
      return only !== null ? { path: null, agent: only, agentMode: true, isAgent: true } : { path: null, agent: null, agentMode: false, isAgent: false };
    }
    const t = agentTarget(base, this.fileMode, this.o.index);
    const isAgent = t.agent !== null;
    return { path: t.path, agent: t.agent, agentMode: isAgent && this.fileMode === "agent", isAgent };
  }

  /** Bind to what the binding says now, and load it. */
  async refresh(): Promise<void> {
    const my = ++this.seq;
    const t = this.target();
    if (this.dirty) {
      // unsaved edits are never dropped by a tree click or a re-bind
      if (t.path !== this.path) this.notice = `unsaved edits in ${this.path} — save or revert before showing ${t.path ?? t.agent ?? "another file"}`;
      this.changed();
      return;
    }
    this.notice = null;
    this.isAgent = t.isAgent;
    if (t.agentMode && t.agent !== null) {
      this.releaseHold();
      this.path = t.path;
      this.agent = t.agent;
      this.editing = false;
      this.gone = false;
      this.content = { k: "agent", agent: t.agent };
      this.persist();
      this.changed();
      return;
    }
    this.agent = t.agent;
    if (t.path === null) {
      this.releaseHold();
      this.path = null;
      this.editing = false;
      this.gone = false;
      this.content = { k: "none" };
      this.persist();
      this.changed();
      return;
    }
    if (t.path !== this.path) {
      this.editing = false;
      this.source = false;
      this.gone = false;
    }
    this.path = t.path;
    this.persist();
    await this.load(my);
  }

  /** The entry for `path` from its folder's listing. */
  private async stat(path: string): Promise<{ entry: Entry } | { refusal: Refusal }> {
    if (path === "") return { entry: { name: "", path: "", type: "dir" } };
    const r = (await this.o.bridge.call({ type: "list", path: wire(parentOf(path)) })) as Record<string, unknown>;
    if (isRefusal(r)) return { refusal: r };
    const files = Array.isArray(r["files"]) ? (r["files"] as Array<Record<string, unknown>>) : [];
    const f = files.find((x) => norm(String(x["path"])) === path);
    if (f === undefined) return { refusal: { error: `'${path}' is not in the project`, reason: "not_found" } };
    return {
      entry: { name: String(f["name"]), path, type: f["type"] === "dir" ? "dir" : "file", ...(typeof f["size"] === "number" ? { size: f["size"] } : {}) },
    };
  }

  private async load(my: number): Promise<void> {
    const path = this.path!;
    const st = await this.stat(path);
    if (my !== this.seq) return;
    if ("refusal" in st) {
      this.lastRefusal = st.refusal;
      this.mode = "error";
      this.content = { k: "error", error: said(st.refusal) };
      this.gone = st.refusal["reason"] === "not_found";
      await this.holdOn(path, my); // a file that is not there yet shows when it appears
      this.changed();
      return;
    }
    const e = st.entry;
    this.size = e.size ?? 0;
    this.mode = modeFor({ type: e.type, name: e.name, ...(e.size !== undefined ? { size: e.size } : {}) });
    this.gone = false;
    await this.holdOn(path, my);
    if (my !== this.seq) return;
    await this.content_(path, my);
    if (my !== this.seq) return;
    this.changed();
  }

  /** Follow `path` live; a refusal is said, not hidden. */
  private async holdOn(path: string, my: number): Promise<void> {
    // a look still in flight when the cell is disposed takes no hold: nothing would ever let it go
    if (this.disposed) return;
    if (this.held !== path) {
      this.releaseHold();
      this.held = path;
      try {
        await this.o.hub.hold(path === "" ? "." : path, this.o.selfId, (c) => this.onEvent(c));
      } catch (err) {
        if (my === this.seq) this.notice = `not live: ${(err as Error).message}`;
        this.held = null;
      }
    }
  }

  private releaseHold(): void {
    if (this.held !== null) this.o.hub.drop(this.held, this.o.selfId);
    this.held = null;
  }

  private async content_(path: string, my: number): Promise<void> {
    const mode = this.mode;
    switch (mode) {
      case "dir": {
        const r = (await this.o.bridge.call({ type: "list", path: wire(path) })) as Record<string, unknown>;
        if (my !== this.seq) return;
        if (isRefusal(r)) {
          this.mode = "error";
          this.content = { k: "error", error: said(r) };
          return;
        }
        const files = Array.isArray(r["files"]) ? (r["files"] as Array<Record<string, unknown>>) : [];
        this.content = {
          k: "dir",
          entries: files.map((f) => ({ name: String(f["name"]), path: norm(String(f["path"])), type: f["type"] === "dir" ? "dir" : "file", ...(typeof f["size"] === "number" ? { size: f["size"] } : {}) })),
        };
        return;
      }
      case "text":
      case "markdown": {
        const r = await readAll(this.o.bridge, path);
        if (my !== this.seq) return;
        if (!r.ok) {
          const refined = refine(mode, r.refusal);
          this.mode = refined.mode;
          this.content = refined.mode === "binary" ? { k: "binary", size: this.size } : { k: "error", error: said(r.refusal) };
          return;
        }
        this.editor.load(r.text, r.version);
        this.content = { k: "text" };
        return;
      }
      case "image":
      case "pdf": {
        const r = await readBytes(this.o.bridge, path, mode === "image" ? IMAGE_MAX : PDF_MAX);
        if (my !== this.seq) return;
        this.content = r.ok ? { k: mode, bytes: r.bytes } : { k: "error", error: said(r.refusal) };
        return;
      }
      case "large":
        await this.page(0, my);
        return;
      case "binary":
        this.content = { k: "binary", size: this.size };
        return;
      case "error":
        return;
    }
  }

  /** One `PAGE` of a large file (the pager). */
  async page(n: number, my = this.seq): Promise<void> {
    if (this.path === null) return;
    const pages = Math.max(1, Math.ceil(this.size / PAGE));
    const at = Math.max(0, Math.min(pages - 1, n));
    // the latest page asked for decides: a reply that comes back after a newer page was asked for is dropped
    const mine = ++this.pageAsk;
    const r = (await this.o.bridge.call({ type: "read_stream", path: wire(this.path), offset: at * PAGE, length: PAGE + 3 })) as Record<string, unknown>;
    if (my !== this.seq || mine !== this.pageAsk) return;
    if (isRefusal(r)) this.content = { k: "error", error: said(r) };
    else {
      // a file over the text limit with an unlisted extension may still be binary: sniffed here, so it gets the size card, not garbage
      const raw = bytesOf(r["bytes"]);
      const text = raw === null ? null : pageText(raw, at === 0);
      if (text === null) {
        this.mode = "binary";
        this.content = { k: "binary", size: this.size };
      } else this.content = { k: "large", size: this.size, page: at, text };
    }
    this.changed();
  }

  /** The bridge reported `c` under a path this preview holds. */
  private onEvent(c: Change): void {
    if (this.disposed || this.path === null) return;
    if (this.mode === "dir" && (c.path === this.path || parentOf(c.path) === this.path)) {
      void this.refresh();
      return;
    }
    if (c.path !== this.path) return;
    if (c.kind === "deleted") {
      this.gone = true;
      if (!this.dirty) this.content = { k: "deleted" };
      this.changed();
      return;
    }
    this.gone = false;
    if (this.content.k === "text") void this.followed();
    else void this.refresh();
  }

  /** A followed text file changed on disk. Unless edits are open, look at what it is
   *  now first — a file that grew past the text limit or turned binary is shown as
   *  that, not read as text — then read it. */
  private async followed(): Promise<void> {
    const path = this.path;
    if (path === null) return;
    if (!this.dirty) {
      const my = this.seq;
      const st = await this.stat(path);
      if (my !== this.seq || this.disposed || this.path !== path) return;
      if ("refusal" in st) {
        // gone between the event and the look; the deleted event says so
        this.lastRefusal = st.refusal;
        return;
      }
      const e = st.entry;
      const mode = modeFor({ type: e.type, name: e.name, ...(e.size !== undefined ? { size: e.size } : {}) });
      if (mode !== this.mode && !(this.mode === "binary" && mode === "text")) {
        await this.refresh();
        return;
      }
    }
    await this.exec(this.editor.diskChanged());
  }

  // ─── the editor's effects ─────────────────────────────────────────────

  /** Run effects in order; an undo the bridge refuses ends the run. */
  async exec(fx: EditorEffect[]): Promise<void> {
    const path = this.path;
    if (path === null) return;
    for (const e of fx) {
      if (e.kind === "write") {
        const r = (await this.o.bridge.call({ type: "write", path: wire(path), bytes: e.bytes, if_match: e.ifMatch })) as Record<string, unknown>;
        if (isRefusal(r)) {
          this.lastRefusal = r;
          await this.exec(this.editor.saveRefused(r));
        } else {
          this.lastRefusal = null;
          this.editor.saved(String(r["version"]));
        }
      } else if (e.kind === "undo") {
        const r = (await this.o.bridge.call({ type: "undo", path: wire(path) })) as Record<string, unknown>;
        if (isRefusal(r)) {
          this.lastRefusal = r;
          this.editor.error = said(r);
          break;
        }
        this.lastRefusal = null;
      } else {
        const mine = ++this.reads;
        const r = await readAll(this.o.bridge, path);
        // a newer read, or another file, decides what is shown
        if (mine !== this.reads || this.path !== path) continue;
        if (r.ok) {
          this.gone = false;
          this.editor.reread(r);
        } else if (r.refusal["reason"] === "not_found") {
          this.gone = true;
          if (!this.editor.isDirty()) this.content = { k: "deleted" };
          this.editor.readFailed("the file was deleted on disk; saving waits until it is back");
        } else if (!this.editor.isDirty() && r.refusal["reason"] === "bad_arg" && r.refusal["arg"] === "path") {
          await this.refresh(); // it turned binary: shown as that
        } else {
          this.lastRefusal = r.refusal;
          this.notice = said(r.refusal);
          this.editor.readFailed(said(r.refusal));
        }
      }
    }
    this.changed();
  }

  // ─── what the cell does ───────────────────────────────────────────────

  edit(text: string): void {
    this.editor.edit(text);
    this.changed();
  }

  /** Turn editing on or off. Off with unsaved edits is refused (the person saves or reverts). */
  setEditing(on: boolean): string | null {
    if (on) {
      if (!this.canEdit()) return this.whyNot();
      this.editing = true;
    } else {
      if (this.dirty) return "unsaved edits: save or revert first";
      this.editing = false;
    }
    this.changed();
    return null;
  }

  async save(): Promise<void> {
    if (this.gone) {
      // an absent file never matches the version the edits are based on
      this.notice = "the file was deleted on disk: saving waits until it is back";
      this.changed();
    } else if (this.editor.state.k === "dirty") await this.exec(this.editor.save());
    else this.changed();
  }

  async revert(): Promise<void> {
    const r = this.editor.revert();
    if (r.ok) await this.exec(r.effects);
    this.changed();
  }

  async reload(): Promise<void> {
    this.editor.reload();
    this.changed();
  }

  async keepMine(): Promise<void> {
    await this.exec(this.editor.keepMine());
  }

  compare(): void {
    this.editor.compare();
    this.changed();
  }

  setSource(on: boolean): void {
    this.source = on;
    this.changed();
  }

  async pin(path: string): Promise<void> {
    this.binding = { pinned: norm(path) };
    await this.refresh();
  }

  async unpin(): Promise<void> {
    this.binding = { follow: true };
    await this.refresh();
  }

  /** Switch an agent folder between its files and the live agent. Refused (the reason) with unsaved edits: the refresh would not show it. */
  async setFileMode(m: "file" | "agent"): Promise<string | null> {
    if (this.dirty) return "unsaved edits: save or revert first";
    this.fileMode = m;
    await this.refresh();
    return null;
  }

  dispose(): void {
    this.disposed = true;
    this.releaseHold();
    this.o.hub.dropAll(this.o.selfId);
    this.listener = null;
  }

  /** The name of the file shown. */
  name(): string {
    return this.path === null ? "" : nameOf(this.path);
  }
}

// ─── THE CELL AS A PAGE AGENT ───────────────────────────────────────────────

/** The live controllers, by cell id. */
export const previews = new Map<string, PreviewController>();

export const PREVIEW_VERBS = ["open", "pin", "unpin", "mode", "edit", "save", "revert"] as const;

export const PREVIEW_VERB_DOCS: Record<string, string> = {
  open: "args: path:str (req — a project path). Pins this preview to the file and shows it (an AI's open means 'show this'). {opened, mode}. Refused: bad_arg, outside_root, not_found, unsaved_edits.",
  pin: "args: path:str? (default: what is shown). Binds this preview to one path; the tree's selection no longer moves it. {pinned}. Refused: nothing_shown.",
  unpin: "No args. Follows the tree's selection again. {following: true}.",
  mode: "args: mode:file|agent (req). For an agent's folder: its files (agent.yaml) or the live agent. {mode}. Refused: bad_arg, not_an_agent.",
  edit: "args: path:str? (open it first). Starts editing the text file shown. {editing, version}. Refused: nothing_shown, not_editable (with why).",
  save: "No args. Writes the edits, against the version they are based on. {saved, version}. Refused: not_editing; conflict ({expected, actual}) when the file changed on disk under the edits — the person resolves it (reload, keep mine, compare); the bridge's refusal otherwise.",
  revert: "No args. Drops the unsaved edits ({reverted: edits}), or undoes the last save through the bridge's undo ({reverted: save}); the undo slot is RAM-only on the host. {reverted, version}. Refused: nothing_to_revert, or the bridge's refusal.",
  reflect: "No args. This cell and its verbs.",
};

export const PREVIEW_README =
  "A PREVIEW cell: shows a file or the live agent. It follows the file tree's selection (`follow`) or is pinned to one path; several can coexist. Text is shown highlighted and followed live from disk; Markdown is rendered; images and PDF are drawn; a binary or large file shows its size (large ones page). An agent's folder shows its agent.yaml, or — in agent mode — the agent itself. Text files can be edited and saved (⌘S / Ctrl-S) against the version read; a change on disk under unsaved edits is a loud conflict (reload, keep mine, compare), never merged. VERBS: open {path}, pin, unpin, mode {file|agent}, edit, save, revert. Page content: it calls the project's file bridge `files` as its cell grant allows.";

const refusal = (error: string, reason: string, extra: Record<string, unknown> = {}): Json => ({ error, reason, ...extra }) as Json;

export const previewAgent: Handler = async (id, payload): Promise<Json> => {
  const verb = str(payload, "type");
  if (!(PREVIEW_VERBS as readonly string[]).includes(verb)) {
    return refusal(`preview: unknown verb '${verb}'`, "unhandled", { verb, hint: `it answers ${PREVIEW_VERBS.join(", ")}` });
  }
  const ctl = previews.get(id);
  if (ctl === undefined) return refusal("preview: the cell is not on screen", "not_mounted", { hint: "the cell is not on screen" });
  const nothing = (): Json => refusal("preview: nothing is shown", "nothing_shown", { hint: "open {path} or select something in the tree" });
  const asRefusal = (r: Refusal | null, fallback: string): Json => (r !== null ? (r as Json) : refusal(fallback, "refused"));
  const pathArg = (): { path: string } | { refused: Json } => {
    const p = payload["path"];
    if (typeof p !== "string" || p === "") return { refused: refusal("preview: path (a root-relative project path) is required", "bad_arg", { arg: "path" }) };
    if (!inRoot(p)) return { refused: refusal(`preview: '${p}' is outside the project`, "outside_root", { arg: "path", hint: "paths are relative to the project folder, with no leading / and no .." }) };
    return { path: norm(p) };
  };
  switch (verb) {
    case "open": {
      const a = pathArg();
      if ("refused" in a) return a.refused;
      if (ctl.dirty && a.path !== ctl.path) return refusal(`preview: unsaved edits in ${ctl.path}`, "unsaved_edits", { hint: "save or revert first" });
      await ctl.pin(a.path);
      if (ctl.mode === "error" && ctl.gone) return asRefusal(ctl.lastRefusal, `preview: '${a.path}' is not in the project`);
      return { opened: a.path, mode: ctl.mode };
    }
    case "pin": {
      let p = ctl.path;
      if (typeof payload["path"] === "string") {
        const a = pathArg();
        if ("refused" in a) return a.refused;
        p = a.path;
      }
      if (p === null) return nothing();
      await ctl.pin(p);
      return { pinned: p };
    }
    case "unpin":
      await ctl.unpin();
      return { following: true };
    case "mode": {
      const m = str(payload, "mode");
      if (m !== "file" && m !== "agent") return refusal("preview: mode must be file or agent", "bad_arg", { arg: "mode" });
      if (!ctl.isAgent) return refusal("preview: what is shown is not an agent's folder", "not_an_agent", { hint: "select an agent folder first" });
      const unsaved = await ctl.setFileMode(m);
      if (unsaved !== null) return refusal(`preview: unsaved edits in ${ctl.path}`, "unsaved_edits", { hint: "save or revert first" });
      return { mode: m };
    }
    case "edit": {
      if (typeof payload["path"] === "string") {
        const a = pathArg();
        if ("refused" in a) return a.refused;
        if (a.path !== ctl.path) {
          if (ctl.dirty) return refusal(`preview: unsaved edits in ${ctl.path}`, "unsaved_edits", { hint: "save or revert first" });
          await ctl.pin(a.path);
        }
      }
      if (ctl.path === null && ctl.content.k !== "agent") return nothing();
      const why = ctl.setEditing(true);
      if (why !== null) return refusal(`preview: not editable — ${why}`, "not_editable", { why });
      return { editing: ctl.path, version: ctl.editor.version() };
    }
    case "save": {
      if (!ctl.editing) return refusal("preview: not editing", "not_editing", { hint: "edit first" });
      if (ctl.gone) return refusal("preview: the file was deleted on disk", "file_deleted", { hint: "saving waits until the file is back; revert drops the edits" });
      const s = ctl.editor.state;
      if (s.k === "conflict" || s.k === "comparing") return refusal("preview: the file changed on disk under your edits", "conflict", { expected: s.baseVersion, actual: s.theirsVersion, hint: "reload, keep mine or compare in the cell" });
      await ctl.save();
      const after = ctl.editor.state;
      if (after.k === "conflict" || after.k === "comparing") return refusal("preview: the file changed on disk under your edits", "conflict", { expected: after.baseVersion, actual: after.theirsVersion, hint: "reload, keep mine or compare in the cell" });
      if (ctl.editor.error !== null) return asRefusal(ctl.lastRefusal, ctl.editor.error);
      return { saved: ctl.path, version: ctl.editor.version() };
    }
    case "revert": {
      const r = ctl.editor.revert();
      if (!r.ok) return refusal("preview: nothing to revert", "nothing_to_revert", { hint: "revert drops unsaved edits, or undoes the last save" });
      ctl.lastRefusal = null;
      await ctl.exec(r.effects);
      if (ctl.lastRefusal !== null) return ctl.lastRefusal as Json;
      return { reverted: r.what, version: ctl.editor.version() };
    }
  }
  return refusal(`preview: unknown verb '${verb}'`, "unhandled", { verb });
};

export function registerPreviewView(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  kernel.registerBundle("preview_view.ts", previewAgent, PREVIEW_VERB_DOCS);
  kernel.setBundleReadme("preview_view.ts", PREVIEW_README);
}
