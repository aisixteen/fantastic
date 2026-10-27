import { mountReadView } from "../read_view.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { hitsState, storeRows, storeState } from "../agent_models/store_model.ts";
import type { Hits } from "../agent_models/store_model.ts";

// ram_store / sqlite_store: owners, row and fold counts, and a search box over
// the viewer's own text index. Counts only: no row body is read into the model.
export const STORE_VIEW = "store_view.ts";
export const STORE_CALLS = ["dump", "search", "watch"] as const;

export const storeView: ViewBundle = {
  handles: [STORE_VIEW],
  grants: (backend) => ({ [backend]: [...STORE_CALLS] }),
  mount(ctx: ViewContext): Promise<ViewHandle> {
    let hits: Hits | null = null;
    return mountReadView<unknown>(ctx, {
      name: "store",
      load: (call) => call({ type: "dump" }),
      shape: (reply) => storeRows(storeState(reply), hits),
      controls(box, { call, redraw }) {
        const input = document.createElement("input");
        input.setAttribute("placeholder", "search your own rows");
        input.setAttribute("name", "query");
        const go = document.createElement("button");
        go.textContent = "search";
        const run = async (): Promise<void> => {
          const query = (input as unknown as { value: string }).value.trim();
          if (query === "") return;
          hits = hitsState(query, await call({ type: "search", query }));
          redraw();
        };
        // typing is typing: no canvas shortcut may take a key
        input.addEventListener("keydown", (e) => {
          (e as unknown as { stopPropagation?: () => void }).stopPropagation?.();
          if ((e as unknown as { key?: string }).key === "Enter") void run();
        });
        go.addEventListener("click", () => void run());
        box.appendChild(input);
        box.appendChild(go);
      },
    });
  },
};
