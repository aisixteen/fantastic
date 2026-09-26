import type { Strategy } from "../context.ts";
import { compact } from "./compact.ts";
import { fold } from "./fold.ts";
import { strip } from "./strip.ts";
import { truncate } from "./truncate.ts";

// The context-overflow strategy registry — port of python `strategies/__init__.py`.
// One strategy per agent (the `context_strategy` field on its record), default
// `compact`. Selection is STATIC config — an unknown name is a config error the
// caller surfaces, never a silent fall-through to the default.

export const DEFAULT_STRATEGY = "compact";

const STRATEGIES: { [name: string]: Strategy } = {
  compact,
  truncate,
  // keeps every turn and drops reply BODIES instead; the cut is its second stage
  strip,
  // leaves MARKS: stretches collapse to appended fold rows; then strip, then cut
  fold,
};

export function getStrategy(name: string): Strategy | null {
  return Object.prototype.hasOwnProperty.call(STRATEGIES, name) ? (STRATEGIES[name] as Strategy) : null;
}

export function strategyNames(): string[] {
  return Object.keys(STRATEGIES);
}
