import { isRefusal, said } from "./file_bridge.ts";
import type { FileBridge } from "./file_bridge.ts";
import { agentIdOf } from "./agent_index.ts";
import type { AgentIndex, AgentInfo } from "./agent_index.ts";
import { FANTASTIC, IGNORED, join, wire } from "./paths.ts";

// READING THE AGENT FOLDERS. An agent is a folder with an `agent.yaml`; its id is
// that file's top-level `id:`. The walk lists `.fantastic` breadth-first and reads
// the head of each `agent.yaml` it meets. Runs one at a time; a request that
// lands while it runs makes it run once more.

const MAX_DEPTH = 12;
const HEAD_LINES = 60;

export class AgentWalker {
  /** folders whose `agent.yaml` could not be read → the refusal, for the row's title */
  readonly errors = new Map<string, string>();
  done = false;
  private running: Promise<void> | null = null;
  private again = false;

  private readonly bridge: FileBridge;
  private readonly index: AgentIndex;
  private readonly after: () => void;

  constructor(bridge: FileBridge, index: AgentIndex, after: () => void) {
    this.bridge = bridge;
    this.index = index;
    this.after = after;
  }

  request(): Promise<void> {
    if (this.running !== null) {
      this.again = true;
      return this.running;
    }
    this.running = (async (): Promise<void> => {
      try {
        do {
          this.again = false;
          try {
            await this.walk();
          } catch (err) {
            // a bridge call that throws: said on the `.fantastic` row, and the next request walks again
            this.errors.set(FANTASTIC, `the agent folders could not be read: ${(err as Error).message}`);
          }
          this.done = true;
          this.after();
        } while (this.again);
      } finally {
        this.running = null;
      }
    })();
    return this.running;
  }

  private async walk(): Promise<void> {
    const found = new Map<string, AgentInfo>();
    const errors = new Map<string, string>();
    let level = [FANTASTIC];
    for (let depth = 0; level.length > 0 && depth <= MAX_DEPTH; depth++) {
      const next: string[] = [];
      for (const dir of level) {
        const r = (await this.bridge.call({ type: "list", path: wire(dir) })) as Record<string, unknown>;
        if (isRefusal(r)) {
          // no `.fantastic` yet is no error; a bridge that does not expose it (its allow list) is said
          if (r["reason"] !== "not_found" || /allow/i.test(String(r["hint"] ?? ""))) errors.set(dir, said(r));
          continue;
        }
        const files = Array.isArray(r["files"]) ? (r["files"] as Array<Record<string, unknown>>) : [];
        for (const f of files) {
          const name = String(f["name"]);
          if (f["type"] === "dir") {
            if (!IGNORED.includes(name)) next.push(join(dir, name));
          } else if (name === "agent.yaml") {
            const info = await this.read(dir, errors);
            if (info !== null) found.set(dir, info);
          }
        }
      }
      level = next;
    }
    this.index.clear();
    for (const [p, info] of found) this.index.set(p, info);
    this.errors.clear();
    for (const [p, e] of errors) this.errors.set(p, e);
  }

  private async read(dir: string, errors: Map<string, string>): Promise<AgentInfo | null> {
    const path = join(dir, "agent.yaml");
    const r = (await this.bridge.call({ type: "read", path, limit: HEAD_LINES })) as Record<string, unknown>;
    if (isRefusal(r)) {
      errors.set(dir, `${path}: ${said(r)}`);
      return null;
    }
    const lines = Array.isArray(r["lines"]) ? (r["lines"] as Array<{ text?: unknown }>).map((l) => String(l.text ?? "")) : [];
    const y = agentIdOf(lines);
    if (y.id === null) {
      errors.set(dir, `${path} names no id`);
      return null;
    }
    return { id: y.id, handler: y.handler ?? "", displayName: y.displayName ?? "", hasParent: y.hasParent, hasMemory: y.hasMemory };
  }
}
