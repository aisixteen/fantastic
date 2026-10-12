// THE AI SET-UP WIZARD's pure part (owner 2026-10-11): the four providers, their
// starting values, a new agent's auto name, and which stored key fits an
// endpoint — no DOM, so the units test it.

/** The providers page 1 offers, in its order. */
export const PROVIDERS: ReadonlyArray<{ module: string; label: string; endpoint: string }> = [
  { module: "responses_backend.tools", label: "responses", endpoint: "https://api.openai.com/v1/responses" },
  { module: "anthropic_backend.tools", label: "anthropic", endpoint: "https://api.anthropic.com/v1/messages" },
  { module: "ollama_backend.tools", label: "ollama", endpoint: "http://127.0.0.1:11434" },
  { module: "vllm_backend.tools", label: "vllm", endpoint: "http://127.0.0.1:8000/v1" },
];

/** A provider's short name: `vllm_backend.tools` → `vllm`. */
export const providerOf = (module: string): string => module.replace(/_backend\.tools$/, "").replace(/\.tools$/, "");

/** The first free `<provider>-<n>` among the names already taken. */
export function autoName(module: string, taken: Iterable<string>): string {
  const t = new Set(taken);
  const base = providerOf(module);
  for (let n = 1; ; n++) if (!t.has(`${base}-${n}`)) return `${base}-${n}`;
}

/** The `host[:port]` of an endpoint URL, lowercased (`kern/keys.rs` `host_of`). */
export function hostOf(endpoint: string): string {
  const rest = endpoint.includes("://") ? endpoint.split("://")[1]! : endpoint;
  return (rest.split(/[/?#]/)[0] ?? "").split("@").pop()!.toLowerCase();
}

/** The ref a key for `module` at `endpoint` is stored under (`kern/keys.rs` `ref_for`). */
export const refFor = (module: string, endpoint: string): string => `${providerOf(module)}/${hostOf(endpoint)}`;

/** The stored key that fits `module` at `endpoint` — a key is bound to its host. */
export function savedKeyFor(keys: ReadonlyArray<{ ref: string }>, module: string, endpoint: string): string | null {
  const r = refFor(module, endpoint);
  return keys.some((k) => k.ref === r) ? r : null;
}

/** A name a saved set-up may take. */
export const nameOk = (name: string): boolean => /^[A-Za-z0-9_-]+$/.test(name);
