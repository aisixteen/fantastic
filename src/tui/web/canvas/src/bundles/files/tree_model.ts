import { ignored, norm, parentOf, under } from "./paths.ts";

// THE TREE'S STATE and its transitions — pure, no DOM, no bridge. Every
// transition returns EFFECTS (`list`, `watch`, `unwatch`) for the controller to
// run; the model never calls anything. One invariant holds after each of them:
// the watched paths are exactly the expanded paths.

export type Effect = { kind: "list" | "watch" | "unwatch"; path: string };

export interface Entry {
  name: string;
  path: string;
  type: "dir" | "file";
  size?: number;
}

export interface TreeNode {
  path: string;
  name: string;
  type: "dir" | "file";
  size?: number;
  status: "unloaded" | "loading" | "loaded" | "error";
  /** the refusal, when a list failed */
  error?: string;
  /** child paths, once listed */
  children?: string[];
  /** a `changed` landed while a list was in flight: list again when it returns */
  stale: boolean;
}

export interface Row {
  path: string;
  depth: number;
  type: "dir" | "file";
  open: boolean;
  name: string;
}

export type KeyEffect = { kind: "expand" | "collapse" | "select" | "open"; path: string };

export class TreeModel {
  readonly nodes = new Map<string, TreeNode>();
  readonly expanded = new Set<string>();
  selected: string | null = null;
  cursor: string | null = null;
  showIgnored = false;

  constructor() {
    this.nodes.set("", { path: "", name: "", type: "dir", status: "unloaded", stale: false });
  }

  isOpen(p: string): boolean {
    return this.expanded.has(p);
  }

  /** Open folder `p`: list it and watch it. A no-op when open, or not a known folder. */
  expand(p: string): Effect[] {
    const n = this.nodes.get(p);
    if (n === undefined || n.type !== "dir" || this.expanded.has(p)) return [];
    this.expanded.add(p);
    n.status = "loading";
    delete n.error;
    return [
      { kind: "list", path: p },
      { kind: "watch", path: p },
    ];
  }

  /** Close folder `p`, and every open folder below it (their watches end, their openness is forgotten). */
  collapse(p: string): Effect[] {
    const out: Effect[] = [];
    for (const e of [...this.expanded]) {
      if (under(e, p)) {
        this.expanded.delete(e);
        out.push({ kind: "unwatch", path: e });
      }
    }
    return out;
  }

  /** `entries` of `p` came back. */
  listed(p: string, entries: Entry[]): Effect[] {
    const n = this.nodes.get(p);
    if (n === undefined || !this.expanded.has(p)) return []; // closed, or gone, while it was listing
    const out: Effect[] = [];
    const now = new Set(entries.map((e) => norm(e.path)));
    for (const old of n.children ?? []) {
      if (now.has(old)) continue;
      out.push(...this.collapse(old));
      this.forget(old);
    }
    n.children = entries.map((e) => {
      const path = norm(e.path);
      const was = this.nodes.get(path);
      if (was !== undefined && was.type === e.type) {
        if (e.size !== undefined) was.size = e.size;
      } else {
        // new, or a file became a folder (or back)
        if (was !== undefined) {
          out.push(...this.collapse(path));
          this.forget(path);
        }
        this.nodes.set(path, { path, name: e.name, type: e.type, ...(e.size !== undefined ? { size: e.size } : {}), status: "unloaded", stale: false });
      }
      return path;
    });
    delete n.error;
    if (n.stale) {
      n.stale = false;
      n.status = "loading";
      out.push({ kind: "list", path: p });
    } else {
      n.status = "loaded";
    }
    return out;
  }

  /** The listing of `p` was refused: its row says so. */
  listFailed(p: string, refusal: string): Effect[] {
    const n = this.nodes.get(p);
    if (n === undefined) return [];
    n.status = "error";
    n.error = refusal;
    n.stale = false;
    return [];
  }

  /** A `changed {path, kind}` event from the bridge. */
  changed(ev: { path: string; kind: string }): Effect[] {
    const p = norm(ev.path);
    const out: Effect[] = [];
    if (ev.kind === "modified" && this.nodes.get(p)?.type === "file") return out; // the previews follow it
    if (ev.kind === "deleted" && this.expanded.has(p)) out.push(...this.collapse(p));
    const parent = parentOf(p);
    if (p !== "" && this.expanded.has(parent)) {
      const n = this.nodes.get(parent)!;
      if (n.status === "loading") n.stale = true;
      else {
        n.status = "loading";
        out.push({ kind: "list", path: parent });
      }
    }
    return out;
  }

  /** The cell goes away: every watch ends. */
  unmount(): Effect[] {
    const out = [...this.expanded].map((path): Effect => ({ kind: "unwatch", path }));
    this.expanded.clear();
    return out;
  }

  /** Drop a node and everything below it (its watches are the caller's: see `collapse`). */
  private forget(p: string): void {
    for (const k of [...this.nodes.keys()]) if (k !== "" && under(k, p)) this.nodes.delete(k);
  }

  /** Whether a name shows: an ignored folder hides unless asked for. */
  private shown(n: TreeNode): boolean {
    return this.showIgnored || !ignored(n.name);
  }

  private kids(n: TreeNode): TreeNode[] {
    return (n.children ?? [])
      .map((c) => this.nodes.get(c))
      .filter((c): c is TreeNode => c !== undefined && this.shown(c))
      .sort((a, b) => (a.type === b.type ? a.name.localeCompare(b.name) : a.type === "dir" ? -1 : 1));
  }

  /** The visible rows, top to bottom. With a `filter`, the loaded nodes whose name has it, and their folders. */
  rows(filter = ""): Row[] {
    const f = filter.trim().toLowerCase();
    const out: Row[] = [];
    const matches = (n: TreeNode): boolean => n.name.toLowerCase().includes(f) || this.kids(n).some(matches);
    const walk = (n: TreeNode, depth: number): void => {
      const open = f !== "" ? n.type === "dir" && this.kids(n).some(matches) : this.expanded.has(n.path);
      out.push({ path: n.path, depth, type: n.type, open: n.type === "dir" && open, name: n.name });
      if (n.type === "dir" && open) for (const c of this.kids(n)) if (f === "" || matches(c)) walk(c, depth + 1);
    };
    walk(this.nodes.get("")!, 0);
    return out;
  }

  /** A key in the tree: the cursor's new place, and what to do there. */
  key(rows: Row[], cursor: string | null, key: string, shift = false): { cursor: string | null; effect?: KeyEffect } {
    if (rows.length === 0) return { cursor };
    let i = rows.findIndex((r) => r.path === cursor);
    if (i < 0) i = 0;
    const row = rows[i]!;
    const at = (j: number): { cursor: string } => ({ cursor: rows[Math.max(0, Math.min(rows.length - 1, j))]!.path });
    switch (key) {
      case "ArrowDown":
        return at(i + 1);
      case "ArrowUp":
        return at(i - 1);
      case "Home":
        return at(0);
      case "End":
        return at(rows.length - 1);
      case "ArrowRight":
        if (row.type !== "dir") return { cursor: row.path };
        if (!row.open) return { cursor: row.path, effect: { kind: "expand", path: row.path } };
        return rows[i + 1] !== undefined && rows[i + 1]!.depth === row.depth + 1 ? at(i + 1) : { cursor: row.path };
      case "ArrowLeft": {
        if (row.type === "dir" && row.open) return { cursor: row.path, effect: { kind: "collapse", path: row.path } };
        const up = rows.find((r) => r.path === parentOf(row.path) && r.path !== row.path);
        return { cursor: up !== undefined ? up.path : row.path };
      }
      case "Enter":
        if (shift) return { cursor: row.path, effect: { kind: "open", path: row.path } };
        if (row.type === "file") return { cursor: row.path, effect: { kind: "select", path: row.path } };
        return { cursor: row.path, effect: { kind: row.open ? "collapse" : "expand", path: row.path } };
      default:
        return { cursor: row.path };
    }
  }
}
