import { mountReadView } from "../read_view.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { asDict, asList, str } from "../agent_models/rows.ts";
import { resourcesRows, resourcesState } from "../agent_models/resources_model.ts";
import type { ResourcesReplies } from "../agent_models/resources_model.ts";

// agentic_resources: roles, the fleet, counts and limits. Read-only.
export const RESOURCES_VIEW = "resources_view.ts";
export const RESOURCES_CALLS = ["roles", "fleet", "role", "get_count", "watch"] as const;

export const resourcesView: ViewBundle = {
  handles: [RESOURCES_VIEW],
  grants: (backend) => ({ [backend]: [...RESOURCES_CALLS] }),
  mount(ctx: ViewContext): Promise<ViewHandle> {
    return mountReadView<ResourcesReplies>(ctx, {
      name: "resources",
      async load(call) {
        const [roles, fleet, total] = await Promise.all([call({ type: "roles" }), call({ type: "fleet" }), call({ type: "get_count" })]);
        const names = new Set<string>(asList(asDict(roles)?.["roles"]).map((r) => str(r, "name")));
        for (const k of Object.keys(asDict(asDict(fleet)?.["roles"]) ?? {})) names.add(k);
        const cards: Record<string, unknown> = {};
        const counts: Record<string, unknown> = {};
        await Promise.all([...names].filter((n) => n !== "").map(async (n) => {
          [cards[n], counts[n]] = await Promise.all([call({ type: "role", name: n }), call({ type: "get_count", role: n })]);
        }));
        return { roles, fleet, total, cards, counts };
      },
      shape: (r) => resourcesRows(resourcesState(r)),
    });
  },
};
