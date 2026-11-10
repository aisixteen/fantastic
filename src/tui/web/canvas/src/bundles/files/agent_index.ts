import { under } from "./paths.ts";

// WHICH FOLDERS ARE AGENTS. An agent lives on disk as a folder holding an
// `agent.yaml`; its id is that file's top-level `id:` — NOT the folder's name
// (layout v2 nests `canvas_root` at `.fantastic/web/agents/canvas/`). The index
// maps folder ↔ id from what the tree read. Pure.

export interface AgentInfo {
  id: string;
  handler: string;
  displayName: string;
}

const unquote = (v: string): string => v.trim().replace(/^(["'])(.*)\1$/, "$2");

/** The identity fields from the head of an agent.yaml: top-level keys only (an
 *  indented `id:` belongs to a nested map). `id` null when the head names none. */
export function agentIdOf(lines: string[]): { id: string | null; handler: string | null; displayName: string | null } {
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

/** The badge for an agent, by its handler name: ✦ an AI (a backend or `ai_chat`),
 *  ⌨ exec, ▣ a store, ◻ a page (`.ts`) agent, ◇ anything else. */
export function badge(handler: string): string {
  if (
    handler === "anthropic_backend.tools" ||
    handler === "ollama_backend.tools" ||
    handler === "responses_backend.tools" ||
    handler === "vllm_backend.tools" ||
    handler === "foundation_models_backend.tools" ||
    handler === "ai_chat.tools"
  )
    return "✦";
  if (handler === "exec_bridge.tools") return "⌨";
  if (
    handler === "ram_store.tools" ||
    handler === "sqlite_store.tools" ||
    handler === "credential_store.tools" ||
    handler === "user_home_store.tools" ||
    handler === "user_store.tools"
  )
    return "▣";
  if (handler.endsWith(".ts")) return "◻";
  return "◇";
}
