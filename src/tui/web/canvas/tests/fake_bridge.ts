import { createHash } from "node:crypto";
import type { FileBridge, Subscribe } from "../src/bundles/files/file_bridge.ts";
import { norm, parentOf } from "../src/bundles/files/paths.ts";

// AN IN-MEMORY FILE BRIDGE for the tree and previewer tests: the verbs the page
// calls (list, read, read_stream, write with if_match, undo, watch_path,
// unwatch_path), answering as the real bridge does — refusals included. Holds
// are ONE set, as the real bridge's are for the one caller (`user`): an
// `unwatch_path` ends the hold whoever else wanted it, which is why the page
// counts holders itself.

type Dict = Record<string, unknown>;

export const versionOf = (text: string): string => createHash("sha256").update(text).digest("hex").slice(0, 16);

export class FakeBridge implements FileBridge {
  readonly files = new Map<string, string>();
  readonly dirs = new Set<string>([""]);
  readonly binary = new Set<string>();
  /** the paths held, as the bridge names them (`.` for the root) */
  readonly watched = new Set<string>();
  readonly calls: Dict[] = [];
  private readonly slots = new Map<string, string | null>();
  private listener: ((ev: Dict) => void) | null = null;

  readonly subscribe: Subscribe = (fn) => {
    this.listener = fn;
    return () => {
      this.listener = null;
    };
  };

  get subscribed(): boolean {
    return this.listener !== null;
  }

  // ─── the disk, as another process changes it ───────────────────────────

  put(path: string, text: string): void {
    const p = norm(path);
    this.mkdirs(parentOf(p), true);
    const had = this.files.has(p);
    this.files.set(p, text);
    this.emit(p, had ? "modified" : "added");
  }

  mkdir(path: string): void {
    const p = norm(path);
    this.mkdirs(p, true);
  }

  remove(path: string): void {
    const p = norm(path);
    for (const f of [...this.files.keys()]) if (f === p || f.startsWith(`${p}/`)) this.files.delete(f);
    for (const d of [...this.dirs]) if (d !== "" && (d === p || d.startsWith(`${p}/`))) this.dirs.delete(d);
    this.emit(p, "deleted");
  }

  /** The folders down to `p`; each new one is reported to whoever watches its parent. */
  private mkdirs(p: string, report: boolean): void {
    const made: string[] = [];
    for (let d = p; d !== "" && !this.dirs.has(d); d = parentOf(d)) made.unshift(d);
    for (const d of made) {
      this.dirs.add(d);
      if (report) this.emit(d, "added");
    }
  }

  /** The bridge reports a path to whoever watches it or its folder. */
  emit(path: string, kind: string): void {
    const hold = (x: string): string => (x === "" ? "." : x);
    if (!this.watched.has(hold(path)) && !this.watched.has(hold(parentOf(path)))) return;
    this.listener?.({ type: "changed", path, kind });
  }

  // ─── the verbs ──────────────────────────────────────────────────────────

  async call(p: Dict): Promise<Dict> {
    this.calls.push(p);
    const verb = String(p["type"]);
    const path = norm(String(p["path"] ?? ""));
    const refuse = (error: string, reason: string, extra: Dict = {}): Dict => ({ error, reason, ...extra });
    switch (verb) {
      case "list": {
        if (this.files.has(path)) return refuse(`list: path must be a directory; '${path}' is not one`, "bad_arg", { arg: "path" });
        if (!this.dirs.has(path)) return refuse(`path '${path}' does not exist`, "not_found", { path });
        const out: Dict[] = [];
        for (const d of this.dirs) if (d !== "" && parentOf(d) === path) out.push({ name: d.split("/").pop(), path: d, type: "dir" });
        for (const [f, text] of this.files) if (parentOf(f) === path) out.push({ name: f.split("/").pop(), path: f, type: "file", size: new TextEncoder().encode(text).length });
        return { path: p["path"], files: out };
      }
      case "read": {
        const text = this.files.get(path);
        if (text === undefined) return refuse(`file '${path}' not found`, "not_found", { path });
        if (this.binary.has(path)) return refuse("read: path must be a text file (this one is binary)", "bad_arg", { arg: "path" });
        const all = text.split("\n");
        if (all[all.length - 1] === "") all.pop();
        const off = Number(p["offset"] ?? 0);
        const limit = Number(p["limit"] ?? 200);
        const page = all.slice(off, off + limit);
        return { path, lines: page.map((t, i) => ({ n: off + i + 1, text: t })), total: all.length, eof: off + limit >= all.length, version: versionOf(text) };
      }
      case "read_stream": {
        const text = this.files.get(path);
        if (text === undefined) return refuse(`file '${path}' not found`, "not_found", { path });
        const bytes = new TextEncoder().encode(text);
        const off = Number(p["offset"] ?? 0);
        const len = Number(p["length"] ?? 65536);
        const chunk = bytes.slice(off, off + len);
        return { path, offset: off, bytes: chunk, next_offset: off + chunk.length, eof: off + chunk.length >= bytes.length, size: bytes.length };
      }
      case "write": {
        const have = this.files.get(path);
        if (p["if_match"] !== undefined && p["if_match"] !== null) {
          const actual = have === undefined ? null : versionOf(have);
          if (actual !== p["if_match"]) return refuse("version mismatch — the document changed since you read it", "version_mismatch", { path, expected: p["if_match"], actual });
        }
        const text = typeof p["bytes"] === "string" ? p["bytes"] : new TextDecoder().decode(p["bytes"] as Uint8Array);
        this.slots.set(path, have ?? null);
        this.put(path, text);
        return { path, size: new TextEncoder().encode(text).length, version: versionOf(text) };
      }
      case "undo": {
        if (!this.slots.has(path)) return refuse(`undo: nothing to undo for '${path}'`, "not_found", { path });
        const slot = this.slots.get(path)!;
        const now = this.files.get(path) ?? null;
        this.slots.set(path, now);
        if (slot === null) {
          this.remove(path);
          return { undone: true, path, deleted: true };
        }
        this.put(path, slot);
        return { undone: true, path, version: versionOf(slot) };
      }
      case "watch_path": {
        if (!this.dirs.has(path) && !this.dirs.has(parentOf(path))) return refuse(`watch_path: no directory '${parentOf(path)}' to watch '${path}' in`, "not_found", { path });
        this.watched.add(path === "" ? "." : path);
        return { watching: path === "" ? "." : path, paths: [...this.watched].sort() };
      }
      case "unwatch_path": {
        this.watched.delete(path === "" ? "." : path);
        return { watching: false, paths: [...this.watched].sort() };
      }
    }
    return refuse(`unknown verb '${verb}'`, "unhandled", { verb });
  }
}

/** Let the hub's microtask and the controllers' async chains settle. */
export const settle = async (): Promise<void> => {
  for (let i = 0; i < 8; i++) await new Promise<void>((r) => setImmediate(r));
};
