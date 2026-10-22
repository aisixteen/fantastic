import type { Handler, Json, Payload } from "../../lib.ts";
import { str } from "../../lib.ts";
import { isRefusal, said } from "./file_bridge.ts";
import type { Change, FileBridge, Refusal, WatchHub } from "./file_bridge.ts";
import type { AgentIndex } from "./agent_index.ts";
import type { Selection } from "./files_deps.ts";
import { ancestors, inRoot, norm, parentOf, under, wire } from "./paths.ts";
import { TreeModel } from "./tree_model.ts";
import type { Effect, Entry } from "./tree_model.ts";

// THE TREE'S CONTROLLER — everything the file-tree cell does that is not
// drawing: the model, the bridge calls its effects ask for, the watch holds, the
// selection, and the verbs an AI drives it with. No DOM; the cell draws from it.

export interface FilesState {
  expanded: string[];
  selected: string | null;
}

export interface FilesCtlOptions {
  bridge: FileBridge;
  hub: WatchHub;
  selfId: string;
  /** keep the state on the cell's record (a reload restores the tree) */
  persist(s: FilesState): void;
  index: AgentIndex;
  /** the agent folders changed on disk: read them again */
  walk(): void;
  /** the selection moved: the previews follow */
  publish(s: Selection): void;
  /** the folder the tree starts at ("" the project; a folder dropped on the canvas is its own tree (#135)) */
  root?: string;
}

const FANTASTIC_PREFIX = ".fantastic/";

export class FilesController {
  readonly model: TreeModel;
  /** the folder the tree starts at: nothing above it is shown */
  readonly root: string;
  /** refusals of listings, by folder (the row's title; a verb passes it through) */
  readonly errors = new Map<string, Refusal>();
  private listener: (() => void) | null = null;
  private disposed = false;
  /** true while `start` is restoring: the record is not saved until it is done */
  private restoring = false;

  readonly o: FilesCtlOptions;

  constructor(o: FilesCtlOptions) {
    this.o = o;
    this.root = o.root ?? "";
    this.model = new TreeModel(this.root);
  }

  /** The cell draws from here whenever the tree changed. */
  onChange(fn: () => void): void {
    this.listener = fn;
  }

  private changed(): void {
    this.listener?.();
  }

  /** Open the root, then bring back what the record remembered. */
  async start(saved: Partial<FilesState>): Promise<void> {
    this.restoring = true;
    await this.expand(this.root);
    const open = (saved.expanded ?? []).filter((p) => typeof p === "string" && inRoot(p) && under(p, this.root) && p !== this.root);
    for (const p of open.sort((a, b) => a.split("/").length - b.split("/").length)) {
      if (this.disposed) return;
      const parent = parentOf(p);
      if (!this.model.isOpen(parent) || !this.model.nodes.has(p)) continue; // a folder that went away
      await this.expand(p);
    }
    if (typeof saved.selected === "string" && inRoot(saved.selected) && under(saved.selected, this.root) && this.model.nodes.has(saved.selected)) {
      this.model.selected = saved.selected;
      this.model.cursor = saved.selected;
      this.o.publish({ path: saved.selected, agent: this.o.index.agentAt(saved.selected)?.id ?? null });
    }
    this.restoring = false;
    this.persist();
    this.changed();
  }

  // ─── effects ──────────────────────────────────────────────────────────

  private async run(fx: Effect[]): Promise<void> {
    await Promise.all(fx.map((e) => this.exec(e)));
  }

  private async exec(e: Effect): Promise<void> {
    if (this.disposed) return;
    switch (e.kind) {
      case "unwatch":
        this.o.hub.drop(e.path, this.o.selfId);
        return;
      case "watch":
        try {
          await this.o.hub.hold(e.path, this.o.selfId, (c) => this.onEvent(c));
        } catch (err) {
          // not live is not a tree: close the folder and say why on its row
          const why = (err as Error).message;
          await this.run(this.model.collapse(e.path));
          this.persist();
          const n = this.model.nodes.get(e.path);
          if (n !== undefined) {
            n.status = "error";
            n.error = why;
          }
          this.errors.set(e.path, { error: why, reason: "watch_refused" });
          this.changed();
        }
        return;
      case "list": {
        const r = (await this.o.bridge.call({ type: "list", path: wire(e.path) })) as Record<string, unknown>;
        if (this.disposed) return;
        if (isRefusal(r)) {
          this.errors.set(e.path, r);
          this.model.listFailed(e.path, said(r));
        } else {
          this.errors.delete(e.path);
          const files = Array.isArray(r["files"]) ? (r["files"] as Array<Record<string, unknown>>) : [];
          const entries: Entry[] = files.map((f) => ({
            name: String(f["name"]),
            path: norm(String(f["path"])),
            type: f["type"] === "dir" ? "dir" : "file",
            ...(typeof f["size"] === "number" ? { size: f["size"] } : {}),
          }));
          await this.run(this.model.listed(e.path, entries));
        }
        // a listing that no longer has a folder closed it: save that too
        this.persist();
        this.changed();
        return;
      }
    }
  }

  private onEvent(c: Change): void {
    if (this.disposed) return;
    if ((c.path === ".fantastic" || c.path.startsWith(FANTASTIC_PREFIX)) && (c.kind === "added" || c.kind === "deleted")) this.o.walk();
    void this.run(this.model.changed(c)).then(() => {
      this.persist(); // a change can close a folder on its own
      this.changed();
    });
  }

  // ─── what the cell and the verbs do ───────────────────────────────────

  private persist(): void {
    if (this.restoring) return; // `start` saves once, when the restore is done
    this.o.persist({ expanded: [...this.model.expanded].filter((p) => p !== this.root).sort(), selected: this.model.selected });
  }

  async expand(p: string): Promise<void> {
    await this.run(this.model.expand(p));
    this.persist();
    this.changed();
  }

  async collapse(p: string): Promise<void> {
    await this.run(this.model.collapse(p));
    this.persist();
    this.changed();
  }

  async toggle(p: string): Promise<void> {
    if (this.model.isOpen(p)) await this.collapse(p);
    else await this.expand(p);
  }

  /** Open every folder above `p`, waiting for each listing. False when a folder on the way is not there. */
  async reveal(p: string): Promise<boolean> {
    for (const a of ancestors(p)) {
      if (!under(a, this.root)) continue; // above the root: not in this tree
      if (!this.model.nodes.has(a)) return false;
      await this.expand(a);
      if (this.model.nodes.get(a)!.status === "error") return false;
    }
    return this.model.nodes.has(p);
  }

  select(p: string): void {
    this.model.selected = p;
    this.model.cursor = p;
    this.persist();
    this.o.publish({ path: p, agent: this.o.index.agentAt(p)?.id ?? null });
    this.changed();
  }

  /** An agent with no folder on disk is selected by id alone. */
  selectAgent(id: string): void {
    this.model.selected = null;
    this.o.publish({ path: null, agent: id });
    this.persist();
    this.changed();
  }

  setCursor(p: string | null): void {
    this.model.cursor = p;
    this.changed();
  }

  /** The cell goes away: every watch ends. */
  dispose(): void {
    this.disposed = true;
    this.o.hub.dropAll(this.o.selfId);
    this.model.unmount();
    this.listener = null;
  }
}

// ─── THE CELL AS A PAGE AGENT: verbs an AI drives it with ───────────────────

/** The live controllers, by cell id. */
export const controllers = new Map<string, FilesController>();

export const FILES_VERBS = ["select", "expand", "collapse", "reveal"] as const;

export const FILES_VERB_DOCS: Record<string, string> = {
  select: "args: path:str | agent:str (req — a project path, root-relative; or an agent's id). Opens the folders above it, selects it and scrolls to it; every preview that follows the tree shows it. {selected, kind: file|dir, agent?}. Refused: bad_arg, outside_root, not_found.",
  expand: "args: path:str | agent:str (req). Opens a folder (listed and watched live). {expanded, entries}. Refused: bad_arg, outside_root, not_found, not_a_dir, or the bridge's refusal.",
  collapse: "args: path:str | agent:str (req). Closes a folder and every open folder below it (their watches end). {collapsed, was_open}.",
  reveal: "args: path:str | agent:str (req). Opens the folders above it and scrolls to it; the selection stays. {revealed}. Refused: bad_arg, outside_root, not_found.",
  reflect: "No args. This cell and its verbs.",
};

export const FILES_README =
  "A FILE TREE cell: the project's folder, live (or, dropped from the tree, a folder as its own tree: its root is that folder). `.fantastic` is highlighted, and every agent is a folder in it (marked with a badge, its status and its name). Folders list when opened and are watched while open, so a file made, changed or removed on disk shows without a reload. Click selects (the previews that follow the tree show it); Shift-click selects a range, ⌘/Ctrl-click toggles one, and the selected rows drag onto the canvas (agents show their cells, files open as previews, folders as trees). Double-click, Shift+Enter or ↗ opens an agent's cell. VERBS: select {path | agent}, expand, collapse, reveal. Page content: it calls the project's file bridge `files` as its cell grant allows.";

const refusal = (error: string, reason: string, extra: Record<string, unknown> = {}): Json => ({ error, reason, ...extra }) as Json;

/** `path` or `agent` → a project path; or the refusal. */
function target(p: Payload, ctl: FilesController): { path: string } | { refused: Json } {
  const agent = str(p, "agent");
  let path: string;
  if (agent !== "") {
    const folder = ctl.o.index.folderOf(agent);
    if (folder === undefined) return { refused: refusal(`files: no folder on disk for agent '${agent}'`, "not_found", { arg: "agent", hint: "the agent folders are read from .fantastic; an agent with no folder is not on disk" }) };
    path = folder;
  } else {
    if (typeof p["path"] !== "string") return { refused: refusal("files: path (a root-relative project path) or agent (an agent id) is required", "bad_arg", { arg: "path" }) };
    if (!inRoot(p["path"])) return { refused: refusal(`files: '${p["path"]}' is outside the project`, "outside_root", { arg: "path", hint: "paths are relative to the project folder, with no leading / and no .." }) };
    path = norm(p["path"]);
  }
  // a tree rooted at a folder (#135) shows nothing above it
  if (!under(path, ctl.root)) return { refused: refusal(`files: '${path}' is outside this tree (its root is ${ctl.root === "" ? "the project" : ctl.root})`, "outside_root", { arg: agent !== "" ? "agent" : "path" }) };
  return { path };
}

export const filesAgent: Handler = async (id, payload): Promise<Json> => {
  const verb = str(payload, "type");
  if (verb === "") return refusal("files: no verb", "bad_arg", { arg: "type" });
  if (!(FILES_VERBS as readonly string[]).includes(verb)) {
    return refusal(`files: unknown verb '${verb}'`, "unhandled", { verb, hint: `it answers ${FILES_VERBS.join(", ")}` });
  }
  const ctl = controllers.get(id);
  if (ctl === undefined) return refusal("files: the cell is not on screen", "not_mounted", { hint: "the cell is not on screen" });
  const t = target(payload, ctl);
  if ("refused" in t) return t.refused;
  const p = t.path;
  switch (verb) {
    case "collapse": {
      const was = ctl.model.isOpen(p);
      await ctl.collapse(p);
      return { collapsed: p, was_open: was };
    }
    case "expand": {
      if (!(await ctl.reveal(p))) return notFound(p, ctl);
      const n = ctl.model.nodes.get(p);
      if (n === undefined) return notFound(p, ctl);
      if (n.type !== "dir") return refusal(`files: '${p}' is a file, not a folder`, "not_a_dir", { arg: "path" });
      await ctl.expand(p);
      const e = ctl.errors.get(p);
      if (e !== undefined) return e as Json;
      return { expanded: p, entries: ctl.model.nodes.get(p)?.children?.length ?? 0 };
    }
    case "reveal":
    case "select": {
      if (!(await ctl.reveal(p))) return notFound(p, ctl);
      const n = ctl.model.nodes.get(p)!;
      if (verb === "reveal") {
        ctl.setCursor(p);
        return { revealed: p };
      }
      ctl.select(p);
      const agent = ctl.o.index.agentAt(p)?.id;
      return { selected: p, kind: n.type, ...(agent !== undefined ? { agent } : {}) };
    }
  }
  return refusal(`files: unknown verb '${verb}'`, "unhandled", { verb });
};

function notFound(p: string, ctl: FilesController): Json {
  const e = ctl.errors.get(parentOf(p));
  if (e !== undefined) return e as Json;
  return refusal(`files: '${p}' is not in the project`, "not_found", { arg: "path" });
}

export function registerFilesView(kernel: {
  registerBundle(hm: string, h: Handler, verbs?: Record<string, string>): void;
  setBundleReadme(hm: string, readme: string): void;
}): void {
  // sidebar_view.ts is the cell's name before the file tree (#106): saved layouts still hold it, and it opens as the tree
  for (const hm of ["files_view.ts", "sidebar_view.ts"]) {
    kernel.registerBundle(hm, filesAgent, FILES_VERB_DOCS);
    kernel.setBundleReadme(hm, hm === "files_view.ts" ? FILES_README : "legacy name of files_view.ts: it opens as the file tree. " + FILES_README);
  }
}
