import { mountReadView } from "../read_view.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { cabinetsRows, cabinetsState } from "../agent_models/cabinets_model.ts";

// cabinet MANAGER: its cabinets (task, worker, canvas, status, equipment).
// A cabinet CELL keeps its own status-cell `view` (the record's wins). Read-only.
export const CABINETS_VIEW = "cabinets_view.ts";
export const CABINETS_CALLS = ["cabinets", "watch"] as const;

export const cabinetsView: ViewBundle = {
  handles: [CABINETS_VIEW],
  grants: (backend) => ({ [backend]: [...CABINETS_CALLS] }),
  mount(ctx: ViewContext): Promise<ViewHandle> {
    return mountReadView<unknown>(ctx, {
      name: "cabinets",
      load: (call) => call({ type: "cabinets" }),
      shape: (reply) => cabinetsRows(cabinetsState(reply)),
    });
  },
};
