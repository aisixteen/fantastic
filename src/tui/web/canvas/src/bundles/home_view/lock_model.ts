import type { HomeRow } from "./home_model.ts";

// THE LOCK SCREEN'S MODEL — pure: no DOM, no socket. The kiosk's lock is a state
// the Home page obeys: while the `lock` agent (in the system canvas) says
// `locked`, a full-screen overlay covers the page, and only a lock state that
// says `locked: false` takes it away — never a key, a click or the Home's own
// state. `tests/home_pages.test.ts`. It is a UI lock, not a security boundary.

/** The agent's `state` reply, as the page reads it. */
export interface LockState {
  locked: boolean;
  has_pin: boolean;
  auto_lock_min: number;
  failures: number;
  retry_at_ms: number | null;
}

/** `activity` is sent at most this often (the agent throttles to the same). */
export const ACTIVITY_EVERY_MS = 10_000;

/** The `lock` agent's state, or null when `v` is not one (a refusal, a stranger's reply). */
export function decodeLock(v: unknown): LockState | null {
  if (typeof v !== "object" || v === null || Array.isArray(v)) return null;
  const r = v as Record<string, unknown>;
  const num = (k: string): number | null => (typeof r[k] === "number" && Number.isFinite(r[k]) ? (r[k] as number) : null);
  const retry = r["retry_at_ms"];
  if (typeof r["locked"] !== "boolean" || typeof r["has_pin"] !== "boolean") return null;
  if (num("auto_lock_min") === null || num("failures") === null) return null;
  if (retry !== null && retry !== undefined && typeof retry !== "number") return null;
  return {
    locked: r["locked"],
    has_pin: r["has_pin"],
    auto_lock_min: num("auto_lock_min")!,
    failures: num("failures")!,
    retry_at_ms: typeof retry === "number" ? retry : null,
  };
}

/** The Home row that is the system canvas: the running one named `system`, with a canvas to dial. */
export function systemRow(rows: readonly HomeRow[]): HomeRow | undefined {
  return rows.find((r) => r.status === "running" && r.canvas !== null && r.name === "system");
}

/** The socket of a canvas for the calls and the watch: `ws://<host>/user/ws`. */
export function userSocketUrl(canvasUrl: string): string | null {
  try {
    const u = new URL(canvasUrl);
    return `${u.protocol === "https:" ? "wss" : "ws"}://${u.host}/user/ws`;
  } catch {
    return null;
  }
}

/** What the overlay shows. */
export interface Overlay {
  shown: boolean;
  /** the line under the PIN field */
  message: string;
  /** the PIN field takes a try (not while the wait after wrong PINs runs) */
  canTry: boolean;
}

export const NO_OVERLAY: Overlay = { shown: false, message: "", canTry: false };

/** The overlay `lock` asks for at time `now` (ms since the epoch): drawn exactly while locked. */
export function overlayFor(lock: LockState | null, now: number): Overlay {
  if (lock === null || !lock.locked) return NO_OVERLAY;
  if (lock.retry_at_ms !== null && lock.retry_at_ms > now) {
    const s = Math.ceil((lock.retry_at_ms - now) / 1000);
    return { shown: true, message: `Too many wrong PINs — try again in ${s} s`, canTry: false };
  }
  return { shown: true, message: lock.failures > 0 ? "Wrong PIN" : "", canTry: true };
}

/** What a key or click does to the page behind an overlay. */
export interface Input {
  type: "key" | "click";
  key?: string;
  /** it happened in the overlay's own PIN field or button */
  inside: boolean;
}

/** An input while the overlay is up: swallowed unless it is the overlay's own;
 *  nothing it does ever hides the overlay (only `overlayFor` does). */
export function overlayInput(o: Overlay, ev: Input): { swallow: boolean; overlay: Overlay } {
  if (!o.shown) return { swallow: false, overlay: o };
  return { swallow: !ev.inside, overlay: o };
}

/** Is an `activity` due, given the last one sent (or null) and now? */
export function activityDue(last: number | null, now: number): boolean {
  return last === null || now - last >= ACTIVITY_EVERY_MS;
}
