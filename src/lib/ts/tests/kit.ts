// Shared test kit.
//
// `kin` — the explicit grants a fixture needs. The kernel's baseline is
// `{$self: "*"}` and nothing else, so a fixture whose agents talk along the tree
// says so on the record — never through a kernel default.
//
// `as` — act AS an agent. Identity comes ONLY from dispatch: there is no API to
// send as anyone, so a test does what any transport does — the operator calls
// the agent, and the agent's own handler makes the onward sends.

import type { Kernel } from "../src/kernel/kernel.ts";
import type { Handler } from "../src/kernel/agent.ts";
import type { Json, Payload } from "../src/kernel/json.ts";
import { pyRepr } from "../src/kernel/pyjson.ts";
import { UNHANDLED, refusal } from "../src/kernel/refusal.ts";

/** Parent ⇄ child, every verb, on both fields: a record that reaches and accepts
 *  its creator and its direct children. Merged over any `meta` given. */
export function kin(meta: Payload = {}): Payload {
  const grant = (field: string): Payload => ({
    $parent: "*",
    $subtree: "*",
    ...((meta[field] as Payload | undefined) ?? {}),
  });
  return { ...meta, can_call: grant("can_call"), callable_by: grant("callable_by") };
}

/** The verb the operator sends to make an agent run a test's code. */
export const AS_VERB = "__as";

const RUNS = new Map<string, (view: Kernel) => unknown>();
const WRAPPED = new WeakSet<object>();
let seq = 0;

/**
 * Run `fn` INSIDE `agentId`'s own dispatch and answer what it returns: the
 * operator sends the agent one `__as` call, and its handler — wrapped once per
 * agent, every other verb answered exactly as before — runs `fn` with the kernel
 * view the dispatch handed it. What `fn` sends through that view is the agent's,
 * gated as the agent. A throw inside `fn` is rethrown here.
 */
export async function as<T>(k: Kernel, agentId: string, fn: (view: Kernel) => T | Promise<T>): Promise<T> {
  const agent = k.get(agentId);
  if (agent === undefined) throw new Error(`as: no agent '${agentId}' in this kernel`);
  if (!WRAPPED.has(agent)) {
    const own: Handler | null = agent.handler;
    agent.handler = (id, payload, view) => {
      const verb = payload["type"];
      if (verb === AS_VERB) return RUNS.get(String(payload["run"]))!(view) as Json;
      if (own !== null) return own(id, payload, view);
      // the kernel's own answer for an agent with no handler
      if (verb === "boot" || verb === "shutdown" || verb === "") return null;
      return refusal(UNHANDLED, `agent ${pyRepr(id)} has no handler_module; cannot answer verb ${pyRepr(verb)}`, {
        extra: { verb: verb as Json },
      });
    };
    WRAPPED.add(agent);
  }
  const run = `r${++seq}`;
  let thrown: { e: unknown } | null = null;
  let ran = false;
  RUNS.set(run, async (view) => {
    ran = true;
    try {
      return await fn(view);
    } catch (e) {
      thrown = { e };
      throw e;
    }
  });
  try {
    const [out] = await k.send([{ target: agentId, payload: { type: AS_VERB, run } }]);
    if (thrown !== null) throw (thrown as { e: unknown }).e;
    if (!ran) throw new Error(`as: the operator's call to '${agentId}' never ran: ${JSON.stringify(out!.result)}`);
    return out!.result as T;
  } finally {
    RUNS.delete(run);
  }
}

/** Send `items` as `agentId` (through its own dispatch) — its outcomes. */
export function sendAs(k: Kernel, agentId: string, items: Parameters<Kernel["send"]>[0]): ReturnType<Kernel["send"]> {
  return as(k, agentId, (v) => v.send(items));
}
