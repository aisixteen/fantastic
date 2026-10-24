import type { Payload } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { bindCellToView, fileGrant, release, said } from "./file_bridge.ts";
import { mountTreePanel } from "./tree_panel.ts";
import { inRoot, norm } from "./paths.ts";

// THE FILE TREE, as a cell (#106): the project's folder, live. It replaces the
// agent tree — every agent is a folder in it. The drawing, the marks and the drag
// are `tree_panel.ts` (shared with the canvas's own sidebar, #148); this is the
// cell's part: the folder it is a view of, and its saved state.

export const FILES_VIEW = "files_view.ts";
/** The cell's old name: a saved `sidebar_view.ts` cell opens as this tree. */
export const LEGACY_SIDEBAR_VIEW = "sidebar_view.ts";

const str = (r: Record<string, unknown> | undefined, k: string): string | undefined => (typeof r?.[k] === "string" && r[k] !== "" ? (r[k] as string) : undefined);

export const filesView: ViewBundle = {
  handles: [FILES_VIEW, LEGACY_SIDEBAR_VIEW],
  grants: () => fileGrant(),
  mount(ctx: ViewContext): ViewHandle {
    // the folder this tree starts at (#135: a folder dropped on the canvas); the project when none
    const saved = ctx.record ?? {};
    const root = norm(str(saved, "root") ?? "");
    if (!inRoot(root)) throw new Error(`files_view: its root '${root}' is outside the project`);
    // a tree opened on a folder agent (#150) is that agent's view: its folder is served by it
    const folderAgent = str(saved, "view_agent");
    const panel = mountTreePanel({
      kernel: ctx.kernel,
      selfId: ctx.selfId,
      root,
      mount: ctx.mount,
      persist: (s) => ctx.kernel.updateMeta(ctx.selfId, s as unknown as Payload),
    });

    // A tree is its folder's view: bound now (a saved `view_agent` serves it again; one saved
    // before views gets the view of its folder). Its first listing waits for that, and a
    // refusal is said on the tree, which stays open.
    const bound = bindCellToView(ctx.kernel, "folder", root, folderAgent, (view) => {
      ctx.kernel.updateMeta(ctx.selfId, { view_agent: view } as unknown as Payload);
    }, ctx.selfId).then((b) => {
      if (!b.ok) panel.say(`${said(b.refusal)} — this tree is not its folder's view`);
    });
    void bound.then(() =>
      panel.start({
        expanded: Array.isArray(saved["expanded"]) ? (saved["expanded"] as string[]) : [],
        ...(typeof saved["selected"] === "string" ? { selected: saved["selected"] } : {}),
      }),
    );

    return {
      unmount(): void {
        panel.dispose();
        release(ctx.kernel, root, ctx.selfId);
      },
    };
  },
};
