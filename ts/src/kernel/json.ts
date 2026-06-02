// The JSON value lattice — the only thing that crosses the wire or an
// agent boundary. Mirrors what the python/rust/swift kernels marshal.

export type Json =
  | null
  | boolean
  | number
  | string
  | Json[]
  | { [key: string]: Json };

/** A message sent to an agent. `type` is the verb; the rest are args. */
export type Payload = { [key: string]: Json };

/** Read a string field from a payload, or a fallback. */
export function str(payload: Payload, key: string, fallback = ""): string {
  const v = payload[key];
  return typeof v === "string" ? v : fallback;
}
