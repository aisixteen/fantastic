import { mountReadView } from "../read_view.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { schedulerRows, schedulerState } from "../agent_models/scheduler_model.ts";

// scheduler: the jobs, their next run, target, verb and last result. Read-only.
export const SCHEDULER_VIEW = "scheduler_view.ts";
export const SCHEDULER_CALLS = ["list", "history", "watch"] as const;

export const schedulerView: ViewBundle = {
  handles: [SCHEDULER_VIEW],
  grants: (backend) => ({ [backend]: [...SCHEDULER_CALLS] }),
  mount(ctx: ViewContext): Promise<ViewHandle> {
    return mountReadView<{ list: unknown; history: unknown }>(ctx, {
      name: "scheduler",
      async load(call) {
        const [list, history] = await Promise.all([call({ type: "list" }), call({ type: "history", limit: 20 })]);
        return { list, history };
      },
      shape: (d) => schedulerRows(schedulerState(d.list, d.history), Date.now() / 1000),
    });
  },
};
