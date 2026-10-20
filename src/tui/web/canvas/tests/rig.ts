import { Agent, Kernel } from "../src/lib.ts";
import type { Payload } from "../src/lib.ts";
import { WatchHub } from "../src/bundles/files/file_bridge.ts";
import { AgentIndex } from "../src/bundles/files/agent_index.ts";
import { FilesController, controllers, registerFilesView } from "../src/bundles/files/files_controller.ts";
import type { Selection } from "../src/bundles/files/files_deps.ts";
import { PreviewController, previews, registerPreviewView } from "../src/bundles/preview/preview_controller.ts";
import { FakeBridge } from "./fake_bridge.ts";

export type Dict = Record<string, unknown>;

// A KERNEL WITH THE TREE AND THE PREVIEWER registered, over an in-memory file
// bridge: the tests drive them through `k.send`, as an AI does.

export interface Rig {
  k: Kernel;
  bridge: FakeBridge;
  hub: WatchHub;
  index: AgentIndex;
  published: Selection[];
  saved: Dict[];
  mountTree(id: string): Promise<FilesController>;
  mountPreview(id: string, meta?: Dict): Promise<PreviewController>;
  /** the tree selects: every following preview refreshes, as the bus tells them */
  select(s: Selection | null): Promise<void>;
  call(target: string, p: Dict): Promise<Dict>;
}

export function rig(): Rig {
  const bridge = new FakeBridge();
  const hub = new WatchHub(bridge, bridge.subscribe);
  const index = new AgentIndex();
  const k = new Kernel();
  registerFilesView(k);
  registerPreviewView(k);
  k.registerBundle("canvas.ts", () => null);
  k.setRoot(new Agent({ id: "canvas", handlerModule: "canvas.ts" }));
  const published: Selection[] = [];
  const saved: Dict[] = [];
  let sel: Selection | null = null;
  const mountTree = async (id: string): Promise<FilesController> => {
    k.register(new Agent({ id, parentId: "canvas", handlerModule: "files_view.ts" }));
    const ctl = new FilesController({
      bridge,
      hub,
      selfId: id,
      persist: (s) => saved.push({ ...s }),
      index,
      walk: () => {},
      publish: (s) => published.push(s),
    });
    controllers.set(id, ctl);
    await ctl.start({});
    return ctl;
  };
  const mountPreview = async (id: string, meta: Dict = {}): Promise<PreviewController> => {
    k.register(new Agent({ id, parentId: "canvas", handlerModule: "preview_view.ts" }));
    const ctl = new PreviewController({ bridge, hub, selfId: id, index, persist: (m) => saved.push({ ...m }), selection: () => sel });
    ctl.restore(meta);
    previews.set(id, ctl);
    await ctl.refresh();
    return ctl;
  };
  const select = async (s: Selection | null): Promise<void> => {
    sel = s;
    for (const c of previews.values()) if ("follow" in c.binding) await c.refresh();
  };
  const call = async (target: string, p: Dict): Promise<Dict> => ((await k.send([{ target, payload: p as Payload }]))[0]!.result ?? {}) as Dict;
  return { k, bridge, hub, index, published, saved, mountTree, mountPreview, select, call };
}

export function project(b: FakeBridge): void {
  b.put("docs/a.md", "# Hello\n");
  b.put("src/one.txt", "one");
  b.put("src/deep/x.ts", "export {};\n");
  b.put("top.txt", "t");
  b.put(".fantastic/agents/user/agents/files/agent.yaml", "id: files\nhandler_module: file_bridge.tools\n");
}
