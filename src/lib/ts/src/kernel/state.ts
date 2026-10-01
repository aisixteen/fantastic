// Snapshot shape + validation — mirrors python kernel/_state.py and rust
// state.rs. A flat AgentRecord list; `parent_id` encodes the tree. The kernel
// converts between the live tree and this list (save/load); a loader agent
// turns the list into bytes and back.

import type { Json } from "./json.ts";
import { pyRepr, pyTruthy } from "./pyjson.ts";
import { guardAgentId } from "./refusal.ts";

/** Wire/disk shape of one agent — id + handler_module + parent_id + meta.
 *  Identical to python `Agent.record` and the on-disk `agent.json`. */
export interface AgentRecord {
  id: string;
  handler_module?: string | null;
  parent_id?: string | null;
  [key: string]: Json | undefined;
}

export const CURRENT_VERSION = 1;

export interface Snapshot {
  version: number;
  records: AgentRecord[];
}

/**
 * A state-stream event: added/updated/removed (+ send/emit/drain). The kernel
 * keeps no conversation, so there is no kernel `row` event: an AI agent
 * announces its own rows to its watchers (`watch` on that agent).
 */
export interface StateEvent {
  agent_id: string;
  kind: string;
  [key: string]: unknown;
}

export class SnapshotError extends Error {}

/** Constructor parameters of python's `Agent.__init__` that a record key would
 *  collide with — not meta, never writable from a document (python
 *  `RESERVED_RECORD_KEYS`). Refused here too, so one snapshot means one thing in
 *  every runtime. */
export const RESERVED_RECORD_KEYS: ReadonlySet<string> = new Set(["root_path", "ctx", "parent"]);

/**
 * Validate a flat record list → the single root id (the record with no
 * `parent_id`). Port of python `validate_records` (rust `state.rs` matches it).
 * Throws a `SnapshotError` on a version above this kernel's, a missing or unsafe
 * id (`guardAgentId` — the same rule as a create: no `|`, no separator, not
 * `kernel`, not all-digit), a RESERVED key, a duplicate id, not exactly one root,
 * a `parent_id` that resolves to no record, or a record UNREACHABLE from the root
 * (a cycle, an orphaned chain). All of it runs BEFORE `load` tears the live tree
 * down, so a bad snapshot never destroys one.
 */
export function validateRecords(
  records: AgentRecord[],
  version: number = CURRENT_VERSION,
): string {
  if (version > CURRENT_VERSION) {
    throw new SnapshotError(
      `snapshot version ${version} exceeds this kernel's max (${CURRENT_VERSION})`,
    );
  }
  const ids = new Set<string>();
  const roots: string[] = [];
  for (const r of records) {
    const rid = r.id as unknown;
    if (!pyTruthy(rid)) throw new SnapshotError("snapshot record missing id");
    const badId = guardAgentId("load", rid);
    if (badId !== null) throw new SnapshotError(`record ${pyRepr(rid)}: ${String(badId["error"])}`);
    const id = rid as string;
    const reserved = Object.keys(r).filter((k) => RESERVED_RECORD_KEYS.has(k)).sort();
    if (reserved.length > 0) {
      throw new SnapshotError(
        `record ${pyRepr(id)} carries reserved key(s) ${pyRepr(reserved)} —` +
          " these are Agent constructor parameters, not meta; the tree" +
          " lives where its document lives and is never named in a record",
      );
    }
    if (ids.has(id)) throw new SnapshotError(`duplicate agent id ${pyRepr(id)} in snapshot`);
    ids.add(id);
    if ((r.parent_id ?? null) === null) roots.push(id);
  }
  if (roots.length === 0) {
    throw new SnapshotError("snapshot has no root (no record with parent_id == null)");
  }
  if (roots.length > 1) {
    throw new SnapshotError(`snapshot has ${roots.length} roots; expected exactly one`);
  }
  for (const r of records) {
    const pid = r.parent_id ?? null;
    if (pid !== null && !ids.has(pid)) {
      throw new SnapshotError(`agent ${pyRepr(r.id)} parent ${pyRepr(pid)} not in snapshot`);
    }
  }
  // REACHABILITY. Every parent resolving is not enough: a CYCLE (a -> b -> a)
  // resolves on both ends and is unreachable from the root, so `load`'s walk
  // would silently drop those records.
  const kids = new Map<string | null, string[]>();
  for (const r of records) {
    const pid = r.parent_id ?? null;
    const list = kids.get(pid) ?? [];
    list.push(r.id);
    kids.set(pid, list);
  }
  const root = roots[0] as string;
  const reached = new Set<string>();
  const stack = [root];
  while (stack.length > 0) {
    const node = stack.pop() as string;
    if (reached.has(node)) continue;
    reached.add(node);
    stack.push(...(kids.get(node) ?? []));
  }
  const orphans = [...ids].filter((i) => !reached.has(i)).sort();
  if (orphans.length > 0) {
    throw new SnapshotError(
      `snapshot has ${orphans.length} record(s) unreachable from the root ` +
        `${pyRepr(root)} (a cycle or an orphaned parent chain): ${pyRepr(orphans)}`,
    );
  }
  return root;
}
