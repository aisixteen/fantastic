import { extOf, human } from "./preview_model.ts";
import type { Mode } from "./preview_model.ts";

// THE FILE VIEW'S HEADER, as data: the breadcrumb, the kind chip and the meta
// line under the name. Pure; no DOM.

/** The breadcrumb of a path: its segments, the project root first when the path is empty. */
export function crumbs(path: string | null): string[] {
  if (path === null) return [];
  if (path === "") return ["project"];
  return path.split("/").filter((s) => s !== "");
}

/** The kind chip: what the path is on disk. */
export function kindOf(mode: Mode, agent: boolean): "file" | "folder" | "agent" {
  if (agent) return "agent";
  return mode === "dir" ? "folder" : "file";
}

/** `612 bytes`, then the larger units of `human`. */
export function sizeText(n: number): string {
  if (n < 0) return "unknown size";
  if (n < 1024) return `${n} ${n === 1 ? "byte" : "bytes"}`;
  return human(n);
}

/** The line count of a text. A trailing newline ends the last line, it does not start one. */
export function lineCount(text: string): number {
  if (text === "") return 0;
  const n = text.split("\n").length;
  return text.endsWith("\n") ? n - 1 : n;
}

/** `32 lines · 612 bytes · yaml`: the lines only when the text is in hand, the size and the extension when known. */
export function metaOf(o: { name: string; size?: number; text?: string }): string {
  const parts: string[] = [];
  if (o.text !== undefined) {
    const n = lineCount(o.text);
    parts.push(`${n} ${n === 1 ? "line" : "lines"}`);
  }
  if (o.size !== undefined && o.size >= 0) parts.push(sizeText(o.size));
  const ext = extOf(o.name);
  if (ext !== "") parts.push(ext);
  return parts.join(" · ");
}

/** What the breadcrumb reads of the agent index. */
export interface AgentLookup {
  agentAt(path: string): { id: string; displayName: string; parentId?: string } | undefined;
  folderOf(id: string): string | undefined;
}

/** The agents above (and including) agent `id`, outermost first, as their shown names: its `parent_id` chain as far as the index knows it. */
export function agentChain(id: string, index: AgentLookup): string[] {
  const out: string[] = [];
  const seen = new Set<string>();
  let cur: string | undefined = id;
  while (cur !== undefined && !seen.has(cur)) {
    seen.add(cur);
    const folder = index.folderOf(cur);
    const info = folder === undefined ? undefined : index.agentAt(folder);
    if (info === undefined) {
      if (cur === id) out.push(id);
      break;
    }
    out.push(info.displayName !== "" ? info.displayName : info.id);
    cur = info.parentId;
  }
  return out.reverse();
}

/** The breadcrumb of a path inside an agent's folder: the agent chain, then the path below that folder. Outside any agent folder it is `crumbs`. */
export function trail(path: string | null, index: AgentLookup & { paths(): string[] }): string[] {
  if (path === null || path === "") return crumbs(path);
  let home = "";
  for (const p of index.paths()) {
    if ((path === p || path.startsWith(`${p}/`)) && p.length > home.length) home = p;
  }
  const info = home === "" ? undefined : index.agentAt(home);
  if (info === undefined) return crumbs(path);
  return [...agentChain(info.id, index), ...crumbs(path.slice(home.length))];
}

/** The handler a "no view for X" refusal names: an agent that has no view of its own. */
export function noViewHandler(message: string): string | null {
  const m = /^no view for (.+)$/.exec(message);
  return m === null ? null : m[1]!;
}
