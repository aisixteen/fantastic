import { under } from "./paths.ts";

// WHICH FOLDERS ARE AGENTS. An agent lives on disk as a folder holding an
// `agent.yaml`; its id is that file's top-level `id:` — NOT the folder's name
// (layout v2 nests `canvas_root` at `.fantastic/web/agents/canvas/`). The index
// maps folder ↔ id from what the tree read. Pure.

export interface AgentInfo {
  id: string;
  handler: string;
  displayName: string;
  /** the yaml names a parent (the project's root agent has none) */
  hasParent: boolean;
  /** the yaml names a memory store (an AI agent) */
  hasMemory: boolean;
}

const unquote = (v: string): string => v.trim().replace(/^(["'])(.*)\1$/, "$2");

/** The identity fields from the head of an agent.yaml: top-level keys only (an
 *  indented `id:` belongs to a nested map). `id` null when the head names none. */
export function agentIdOf(lines: string[]): { id: string | null; handler: string | null; displayName: string | null; hasParent: boolean; hasMemory: boolean } {
  const top = (key: string): string | null => {
    for (const l of lines) {
      const m = new RegExp(`^${key}:\\s*(\\S.*)?$`).exec(l);
      if (m !== null) {
        const v = unquote(m[1] ?? "");
        return v === "" || v === "null" || v === "~" ? null : v;
      }
    }
    return null;
  };
  return {
    id: top("id"),
    handler: top("handler_module"),
    displayName: top("display_name"),
    hasParent: top("parent_id") !== null,
    hasMemory: top("memory_store_id") !== null,
  };
}

export class AgentIndex {
  private readonly byPath = new Map<string, AgentInfo>();
  private readonly byId = new Map<string, string>();

  set(path: string, info: AgentInfo): void {
    this.byPath.set(path, info);
    this.byId.set(info.id, path);
  }

  agentAt(path: string): AgentInfo | undefined {
    return this.byPath.get(path);
  }

  folderOf(id: string): string | undefined {
    return this.byId.get(id);
  }

  /** Forget the folder `prefix` and everything below it. */
  drop(prefix: string): void {
    for (const [p, info] of [...this.byPath]) {
      if (!under(p, prefix)) continue;
      this.byPath.delete(p);
      if (this.byId.get(info.id) === p) this.byId.delete(info.id);
    }
  }

  clear(): void {
    this.byPath.clear();
    this.byId.clear();
  }

  paths(): string[] {
    return [...this.byPath.keys()];
  }

  ids(): string[] {
    return [...this.byId.keys()];
  }
}

/** The side an agent runs on: a `.ts` handler is a page agent, anything else the host's. */
export const side = (handler: string): "page" | "host" => (handler.endsWith(".ts") ? "page" : "host");

/** The badge for an agent, by what its record says it is. An AI agent is a
 *  NON-ROOT agent naming a memory store (the root names the project's). `rec`
 *  is the live record when the page has one, else what the yaml said. */
export function badge(rec: Record<string, unknown>, handler: string): string {
  if (typeof rec["memory_store_id"] === "string" && typeof rec["parent_id"] === "string") return "✦";
  if (handler.startsWith("exec_bridge")) return "⌨";
  if (handler.includes("store")) return "▣";
  if (handler.endsWith(".ts")) return "◻";
  return "◇";
}

/** `badge` for a folder known only by its yaml. */
export function badgeOf(info: AgentInfo): string {
  return badge({ ...(info.hasMemory ? { memory_store_id: "x" } : {}), ...(info.hasParent ? { parent_id: "x" } : {}) }, info.handler);
}
