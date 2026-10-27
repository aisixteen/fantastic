import { mountReadView } from "../read_view.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { tasksRows, tasksState } from "../agent_models/tasks_model.ts";

// task_registry: the task tree; a click on a task expands its history. Read-only.
export const TASKS_VIEW = "tasks_view.ts";
export const TASKS_CALLS = ["list", "watch"] as const;

export const tasksView: ViewBundle = {
  handles: [TASKS_VIEW],
  grants: (backend) => ({ [backend]: [...TASKS_CALLS] }),
  mount(ctx: ViewContext): Promise<ViewHandle> {
    const expanded = new Set<string>();
    return mountReadView<unknown>(ctx, {
      name: "tasks",
      load: (call) => call({ type: "list" }),
      shape: (reply) => tasksRows(tasksState(reply), expanded),
      onRow(id, redraw) {
        if (!expanded.delete(id)) expanded.add(id);
        redraw();
      },
    });
  },
};
