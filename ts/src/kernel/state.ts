// Snapshot shape + validation — mirrors python kernel/_state.py and rust
// state.rs. A flat AgentRecord list; `parent_id` encodes the tree. The kernel
// converts between the live tree and this list (save/load); a loader agent
// turns the list into bytes and back.

import type { Json } from "./json.ts";

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

/** A local state-stream event (added/updated/removed + send/emit/drain). */
export interface StateEvent {
  agent_id: string;
  kind: string;
  [key: string]: unknown;
}

export class SnapshotError extends Error {}

/**
 * Validate a flat record list → the single root id (the record with no
 * `parent_id`). Mirrors python `validate_records` / rust `state.rs`. Throws a
 * `SnapshotError` on a bad version, a missing/duplicate id, zero-or-multiple
 * roots, or a `parent_id` that resolves to no record.
 */
export function validateRecords(
  records: AgentRecord[],
  version: number = CURRENT_VERSION,
): string {
  if (version !== CURRENT_VERSION) {
    throw new SnapshotError(
      `unsupported snapshot version ${version} (expected ${CURRENT_VERSION})`,
    );
  }
  const ids = new Set<string>();
  let rootId: string | null = null;
  for (const r of records) {
    const id = r.id;
    if (typeof id !== "string" || id === "") {
      throw new SnapshotError("record missing a string id");
    }
    if (ids.has(id)) throw new SnapshotError(`duplicate agent id '${id}'`);
    ids.add(id);
    const pid = r.parent_id ?? null;
    if (pid === null) {
      if (rootId !== null) {
        throw new SnapshotError(`multiple roots: '${rootId}' and '${id}'`);
      }
      rootId = id;
    }
  }
  if (rootId === null) {
    throw new SnapshotError(
      records.length === 0
        ? "empty snapshot: no root record"
        : "no root record (every record has a parent_id)",
    );
  }
  for (const r of records) {
    const pid = r.parent_id ?? null;
    if (pid !== null && !ids.has(pid)) {
      throw new SnapshotError(
        `agent '${r.id}' references missing parent '${pid}'`,
      );
    }
  }
  return rootId;
}
