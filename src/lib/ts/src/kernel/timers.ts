// Timers are a PLATFORM capability, and the pure kernel compiles without the DOM
// lib (src/kernel/tsconfig.json), so it reads them off globalThis STRUCTURALLY.
// Both browsers and node provide them.

export interface Timers {
  setTimeout: (fn: () => void, ms: number) => unknown;
  clearTimeout: (h: unknown) => void;
}

/** The runtime's timers. NO FALLBACK: a bound that cannot be honoured would turn
 *  "wait at most 5s" into "wait forever", so a runtime without timers fails
 *  LOUDLY, and only for the callers that actually need one. */
export function timers(): Timers {
  const g = globalThis as unknown as Partial<Timers>;
  if (typeof g.setTimeout !== "function" || typeof g.clearTimeout !== "function") {
    throw new Error(
      "kernel: this runtime provides no setTimeout/clearTimeout, so a timed " +
        "operation (a per-item `timeout`, the flush debounce, a stop bound) cannot be honoured",
    );
  }
  return g as Timers;
}

export function sleep(ms: number): Promise<void> {
  const t = timers();
  return new Promise((res) => {
    t.setTimeout(res, ms);
  });
}

/** Seconds since the epoch, as the store's `ts` fields carry them. */
export function nowSeconds(): number {
  return Date.now() / 1000;
}
