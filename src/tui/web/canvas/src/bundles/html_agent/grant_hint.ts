// THE GRANT HINT — when a cell's html calls an agent its `can_call` does not
// name, the call will be refused at run time, inside the panel, where the AI
// never sees it. So the write that set the body SAYS which grants are missing
// and how to add them. It only reads the body; it never writes a grant.

/** `fantastic.<send|watch|emit>("<target>", …)` calls in `html`, as
 *  `{target: verbs}` — `send` contributes the `type` of its payload when it
 *  is written inline, `watch` / `emit` themselves. */
export function calledAgents(html: string): Record<string, string[]> {
  const out: Record<string, Set<string>> = {};
  const add = (t: string, v: string): void => { (out[t] ??= new Set()).add(v); };
  const re = /fantastic\.(send|watch|emit)\(\s*(['"`])([^'"`]+)\2\s*(?:,\s*(\{[^}]*\}))?/g;
  for (let m = re.exec(html); m !== null; m = re.exec(html)) {
    const [, how, , target, arg] = m;
    if (how === "send") {
      const t = /type\s*:\s*(['"`])(\w+)\1/.exec(arg ?? "");
      add(target!, t ? t[2]! : "*");
    } else add(target!, how!);
  }
  return Object.fromEntries(Object.entries(out).map(([k, v]) => [k, [...v]]));
}

/** The grants `html` needs that `canCall` lacks, or null when none is missing. */
export function missingGrants(html: string, canCall: unknown): Record<string, string[]> | null {
  const have = (canCall && typeof canCall === "object" ? canCall : {}) as Record<string, unknown>;
  const miss: Record<string, string[]> = {};
  for (const [target, verbs] of Object.entries(calledAgents(html))) {
    const g = have[target];
    if (g === "*") continue;
    const granted = Array.isArray(g) ? (g as string[]) : [];
    if (granted.includes("*")) continue;
    const need = verbs.filter((v) => v === "*" ? granted.length === 0 : !granted.includes(v));
    if (need.length) miss[target] = need;
  }
  return Object.keys(miss).length ? miss : null;
}

/** The reply fields that say so: `{grants_missing, hint}`, or {} when none. */
export function grantHint(cellId: string, html: string, canCall: unknown): Record<string, unknown> {
  const miss = missingGrants(html, canCall);
  if (miss === null) return {};
  const want = Object.fromEntries(Object.entries(miss).map(([t, v]) => [t, v.includes("*") ? "*" : v]));
  return {
    grants_missing: miss,
    hint: `this cell's html calls ${Object.keys(miss).join(", ")} but its can_call does not name ${Object.keys(miss).length > 1 ? "them" : "it"} — those calls will be refused. Grant it from the view: update {id: ${cellId}, fields: {can_call: ${JSON.stringify({ $self: "*", ...want })}}}`,
  };
}
