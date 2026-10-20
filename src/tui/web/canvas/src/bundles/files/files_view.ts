import type { Payload } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { isFocusClick } from "../canvas/viewer.ts";
import { badge, badgeOf, side } from "./agent_index.ts";
import { bindCellToView, fileGrant, routedBridge, said, sharedHub } from "./file_bridge.ts";
import { FilesController, controllers } from "./files_controller.ts";
import { agentIndex, allHidden, ensureWalker, filesDeps, getWalker, onRender, rootOf, select } from "./files_deps.ts";
import type { Rec } from "./files_deps.ts";
import { DRAG_MIME, encodeDrag, rangeOf, toggled } from "./drag.ts";
import type { DragItem } from "./drag.ts";
import { FANTASTIC, inRoot, norm } from "./paths.ts";
import { ensureFilesStyle } from "./styles.ts";

// THE FILE TREE, as a cell (#106): the project's folder, live. It replaces the
// agent tree — every agent is a folder in it. `files_controller.ts` holds the
// state and the verbs; this draws them.

export const FILES_VIEW = "files_view.ts";
/** The cell's old name: a saved `sidebar_view.ts` cell opens as this tree. */
export const LEGACY_SIDEBAR_VIEW = "sidebar_view.ts";

const str = (r: Rec | undefined, k: string): string | undefined => (typeof r?.[k] === "string" && r[k] !== "" ? (r[k] as string) : undefined);

export const filesView: ViewBundle = {
  handles: [FILES_VIEW, LEGACY_SIDEBAR_VIEW],
  grants: () => fileGrant(),
  mount(ctx: ViewContext): ViewHandle {
    const d = filesDeps("files_view");
    ensureFilesStyle();
    const bridge = routedBridge(ctx.kernel);
    ensureWalker(bridge);
    // the folder this tree starts at (#135: a folder dropped on the canvas); the project when none
    const saved = ctx.record ?? {};
    const root = norm(str(saved, "root") ?? "");
    if (!inRoot(root)) throw new Error(`files_view: its root '${root}' is outside the project`);
    // a tree opened on a folder agent (#150) is that agent's view: its folder is served by it
    const folderAgent = str(saved, "view_agent");
    const el = document.createElement("div");
    el.className = "side files-view";
    el.innerHTML =
      `<div class="ft-head"><span class="ft-kernel host"></span><span class="ft-kernel page"></span><span class="ft-new" hidden></span>` +
      `<input class="ft-filter" placeholder="filter loaded" spellcheck="false" aria-label="filter the loaded tree"></div>` +
      `<div class="ft-note" role="status" hidden></div>` +
      `<ul class="ft-tree" tabindex="0" role="tree"></ul>`;
    ctx.mount.appendChild(el);
    const note = el.querySelector(".ft-note") as HTMLElement;
    const list = el.querySelector(".ft-tree") as HTMLElement;
    const filter = el.querySelector(".ft-filter") as HTMLInputElement;
    const hostLabel = el.querySelector(".ft-kernel.host") as HTMLElement;
    const pageLabel = el.querySelector(".ft-kernel.page") as HTMLElement;
    const newBadge = el.querySelector(".ft-new") as HTMLElement;

    const ctl = new FilesController({
      bridge,
      hub: sharedHub(ctx.kernel),
      selfId: ctx.selfId,
      persist: (s) => {
        ctx.kernel.updateMeta(ctx.selfId, s as unknown as Payload);
      },
      index: agentIndex,
      walk: () => void getWalker()?.request(),
      publish: (s) => select(s, ctx.selfId),
      root,
    });
    controllers.set(ctx.selfId, ctl);

    // THE MARKED ROWS (#134, #135): several rows at once, by Shift (a range) or ⌘/Ctrl
    // (a toggle). A drag takes the marked rows, or the one it starts on.
    let marks: string[] = [];
    let anchor: string | null = null;
    /** a row's key: its path, or `agent:<id>` for an agent with no folder on disk */
    const keyOf = (path: string | null, agent: string | undefined): string => path ?? `agent:${agent ?? ""}`;

    // what the page knows of an agent, for its row
    interface Deco {
      id: string;
      side: "page" | "host";
      glyph: string;
      handler: string;
      loaded: boolean;
      elsewhere: boolean;
      cells: string[];
      /** every cell of it is off its view (#133) */
      hidden: boolean;
      viewed: boolean;
      fresh: boolean;
      name: string;
      dn: string;
    }
    let hostRecs = new Map<string, Rec>();
    let pageRecs = new Map<string, Rec>();
    const deco = (id: string, folder?: string): Deco => {
      const info = folder !== undefined ? agentIndex.agentAt(folder) : undefined;
      const page = pageRecs.get(id);
      const host = hostRecs.get(id);
      const rec = page ?? host;
      const handler = str(rec, "handler_module") ?? info?.handler ?? "";
      const sd = page !== undefined ? "page" : host !== undefined ? "host" : side(handler);
      const loaded = rec !== undefined;
      const dn = str(rec, "display_name") ?? info?.displayName ?? "";
      const cells = loaded ? d.cellsFor(id) : [];
      return {
        id,
        side: sd,
        glyph: rec !== undefined ? badge(rec, handler) : info !== undefined ? badgeOf(info) : "◇",
        handler,
        loaded,
        elsewhere: !loaded && sd === "host" && d.onHost(id) && !d.seenHere(id),
        cells,
        hidden: allHidden(cells, d.isHidden),
        viewed: d.viewed() === id,
        fresh: sd === "host" && d.fresh(id),
        name: id,
        dn: dn !== id ? dn : "",
      };
    };

    /** What a plain click used to do: view a page agent, show a host agent's cell, or open one. */
    const activate = (a: Deco): void => {
      if (a.elsewhere || !a.loaded) return;
      if (a.side === "page") return d.view(a.id, false);
      const now = d.cellsFor(a.id);
      if (now.length > 0) d.view(now[0]!, false);
      else if (a.handler !== "") d.open(a.id);
    };

    const row = (path: string | null, depth: number, a: Deco | null, o: { type: "dir" | "file"; open: boolean; name: string; count: string; status: string; err?: string }): HTMLElement => {
      const li = document.createElement("li");
      li.setAttribute("role", "treeitem");
      li.className = "ft-row";
      if (path !== null) {
        li.dataset["path"] = path;
        if (path === FANTASTIC) li.classList.add("fantastic");
        if (path === ctl.model.selected) li.classList.add("sel");
        if (path === ctl.model.cursor) li.classList.add("cur");
        if (o.type === "dir") li.setAttribute("aria-expanded", String(o.open));
      }
      // a drag source (#134, #135): a host agent loaded here, a file, a folder. A page
      // agent (or one on another canvas) is not dragged: the canvas cannot show it.
      const key = keyOf(path, a?.id);
      li.dataset["key"] = key;
      if (marks.includes(key)) li.classList.add("marked");
      if (a !== null) {
        if (a.side === "host" && a.loaded && !a.elsewhere) {
          li.dataset["kind"] = "agent";
          li.draggable = true;
        }
      } else if (path !== null) {
        li.dataset["kind"] = o.type;
        li.draggable = true;
      }
      li.style.paddingLeft = `${10 + depth * 12}px`;
      li.innerHTML = `<span class="tw"></span><span class="gl"></span><span class="nm"></span><span class="ct"></span><span class="st"></span>`;
      (li.children[0] as HTMLElement).textContent = o.type === "dir" && path !== null ? (o.open ? "▾" : "▸") : "";
      (li.children[1] as HTMLElement).textContent = a !== null ? a.glyph : o.type === "dir" ? "📁" : "⋯";
      const nm = li.children[2] as HTMLElement;
      nm.textContent = o.name;
      (li.children[3] as HTMLElement).textContent = o.count;
      (li.children[4] as HTMLElement).textContent = o.status;
      let title = path === null ? o.name : path === "" ? "the project folder" : path;
      if (o.err !== undefined) {
        li.classList.add("err");
        title += ` — ${o.err}`;
      }
      if (a !== null) {
        li.dataset["agent"] = a.id;
        li.classList.add("agent", a.side);
        // hidden (#133): every cell of it is off its view — shown, it is not "has-cell"
        if (a.hidden) li.classList.add("hidden");
        else if (a.cells.length > 0) li.classList.add("has-cell");
        if (a.viewed) li.classList.add("viewed");
        if (a.fresh) li.classList.add("fresh");
        if (a.elsewhere) li.classList.add("elsewhere");
        if (!a.loaded) li.classList.add("unloaded");
        if (a.dn !== "") {
          nm.textContent = a.dn;
          const id = document.createElement("span");
          id.className = "id";
          id.textContent = ` · ${a.id}`;
          nm.appendChild(id);
        }
        const fkey = d.mac ? "⌘" : "Ctrl";
        title =
          `${a.id} · ${a.handler || "?"}` +
          (a.elsewhere
            ? " · on another canvas"
            : !a.loaded
              ? " · not loaded"
              : a.side === "page"
                ? " · double-click: view it"
                : a.hidden
                  ? " · hidden: double-click shows its cell and views it"
                  : a.cells.length > 0
                    ? " · double-click: view its cell"
                    : " · double-click: open it in a cell") +
          (li.draggable ? ` · drag onto the canvas · ${fkey}-click: select more` : "") +
          (path !== null ? ` · ${path}` : "") +
          (o.err !== undefined ? ` — ${o.err}` : "");
        if (!a.elsewhere && a.loaded) {
          const go = document.createElement("span");
          go.className = "go";
          go.textContent = "↗";
          go.title = a.side === "page" ? `view ${a.id}` : a.cells.length > 0 ? `view the cell of ${a.id}` : `open ${a.id} in a cell`;
          li.appendChild(go);
        }
        if (a.side === "host" && a.handler !== "" && !a.elsewhere && a.loaded) {
          const del = document.createElement("span");
          del.className = "del";
          del.textContent = "⌫";
          del.title = `delete ${a.id} (the agent, not just its view)`;
          li.appendChild(del);
        }
      }
      li.title = title;
      return li;
    };

    let lastViewed = "";
    const draw = (): void => {
      hostRecs = new Map(d.hostRecords().map((r) => [String(r["id"]), r]));
      pageRecs = new Map(d.pageAgents().map((r) => [String(r["id"]), r]));
      const rows = ctl.model.rows(filter.value);
      const out: HTMLElement[] = [];
      for (const r of rows) {
        const node = ctl.model.nodes.get(r.path)!;
        const folder = agentIndex.agentAt(r.path);
        const a = folder !== undefined ? deco(folder.id, r.path) : null;
        const cells = a !== null ? a.cells.length : 0;
        const st =
          a === null
            ? node.status === "loading" && r.type === "dir" ? "…" : ""
            : a.viewed ? "viewed" : cells > 0 ? `${cells} cell${cells > 1 ? "s" : ""}` : a.loaded ? "loaded" : a.elsewhere ? "elsewhere" : "not loaded";
        const walkErr = getWalker()?.errors.get(r.path);
        out.push(
          row(r.path, r.depth, a, {
            type: r.type,
            open: r.open,
            name: r.path === "" ? "project" : r.name,
            count: a !== null && a.fresh ? "new" : a !== null && cells > 1 ? String(cells) : "",
            status: node.status === "error" ? "✕" : st,
            ...(node.error !== undefined ? { err: node.error } : walkErr !== undefined ? { err: walkErr } : {}),
          }),
        );
      }
      // agents the page runs that have no folder on disk
      if (root === "" && getWalker()?.done === true && filter.value.trim() === "") {
        const onDisk = new Set(agentIndex.ids());
        const loose = [...hostRecs.keys(), ...pageRecs.keys()].filter((id) => !onDisk.has(id));
        if (loose.length > 0) {
          const g = document.createElement("li");
          g.className = "ft-group";
          g.textContent = "◆ not on disk";
          out.push(g);
          for (const id of [...new Set(loose)].sort()) {
            const a = deco(id);
            out.push(row(null, 1, a, { type: "file", open: false, name: id, count: a.fresh ? "new" : a.cells.length > 1 ? String(a.cells.length) : "", status: a.viewed ? "viewed" : "not on disk" }));
          }
        }
      }
      list.replaceChildren(...out);
      const hosts = d.hostRecords();
      hostLabel.textContent = `◆ host${hosts.length > 0 ? ` · ${rootOf(hosts)}` : ""}`;
      const pages = d.pageAgents();
      pageLabel.textContent = `◆ page${pages.length > 0 ? ` · ${rootOf(pages)}` : ""}`;
      const fresh = d.freshIds();
      newBadge.hidden = fresh.length === 0;
      newBadge.textContent = `${fresh.length} new`;
      // the path to the VIEWED agent is unfolded once per change, so the person can still fold it
      const w = getWalker();
      if (w?.done === true && d.viewed() !== lastViewed) {
        lastViewed = d.viewed();
        const f = agentIndex.folderOf(lastViewed);
        if (f !== undefined) void ctl.reveal(f);
      }
      list.querySelector(".ft-row.cur")?.scrollIntoView?.({ block: "nearest" });
    };
    ctl.onChange(draw);
    const offRender = onRender(draw);

    const rowOf = (e: Event): { li: HTMLElement; path: string | null; agent: string | undefined } | null => {
      const li = (e.target as Element).closest(".ft-row") as HTMLElement | null;
      if (li === null) return null;
      return { li, path: li.dataset["path"] ?? null, agent: li.dataset["agent"] };
    };
    const decoOf = (r: { path: string | null; agent: string | undefined }): Deco | null => {
      if (r.agent === undefined) return null;
      return deco(r.agent, r.path ?? undefined);
    };
    /** the rows as shown, by key (a Shift-click's range runs over them) */
    const visibleKeys = (): string[] => ctl.model.rows(filter.value).map((r) => r.path);
    /** the marks, drawn on the rows already on screen (no redraw: a drag's source must stay in the DOM) */
    const paintMarks = (): void => {
      for (const li of list.querySelectorAll<HTMLElement>(".ft-row")) li.classList.toggle("marked", marks.includes(li.dataset["key"] ?? ""));
    };
    /** a drag source row as the item it drags (`row()` sets the row's kind) */
    const itemOf = (li: HTMLElement): DragItem | null => {
      const kind = li.dataset["kind"];
      if (kind === "agent" && li.dataset["agent"] !== undefined) return { kind: "agent", id: li.dataset["agent"] };
      if ((kind === "file" || kind === "dir") && li.dataset["path"] !== undefined) return { kind, path: li.dataset["path"] };
      return null;
    };

    list.addEventListener("click", (e) => {
      const t = e.target as Element;
      const r = rowOf(e);
      if (r === null) return;
      const a = decoOf(r);
      if (t.closest(".del") !== null && a !== null) return d.deleteAgent(a.id);
      if (t.closest(".go") !== null && a !== null) return activate(a);
      list.focus();
      if (r.path !== null && t.closest(".tw") !== null) {
        void ctl.toggle(r.path);
        return;
      }
      // Shift: the range from the anchor row; ⌘/Ctrl: this row in or out of the marks (#134, #135)
      const key = r.li.dataset["key"] ?? "";
      if (e.shiftKey && anchor !== null) {
        marks = rangeOf(visibleKeys(), anchor, key);
        return paintMarks();
      }
      if (isFocusClick(e, d.mac)) {
        marks = toggled(marks, key);
        anchor = key;
        return paintMarks();
      }
      marks = [key];
      anchor = key;
      paintMarks();
      if (r.path !== null) ctl.select(r.path);
      else if (a !== null) ctl.selectAgent(a.id);
    });
    // a drag takes the marked rows that can be dragged, or the row it starts on (#134, #135)
    list.addEventListener("dragstart", (e) => {
      const r = rowOf(e);
      if (r === null || !r.li.draggable) return e.preventDefault();
      const key = r.li.dataset["key"] ?? "";
      if (!marks.includes(key)) {
        marks = [key];
        anchor = key;
        paintMarks();
      }
      const items: DragItem[] = [];
      for (const li of list.querySelectorAll<HTMLElement>(".ft-row[draggable='true']")) {
        const it = itemOf(li);
        if (it !== null && marks.includes(li.dataset["key"] ?? "")) items.push(it);
      }
      if (items.length === 0 || e.dataTransfer === null) return e.preventDefault();
      e.dataTransfer.setData(DRAG_MIME, encodeDrag(items));
      e.dataTransfer.setData("text/plain", items.map((it) => (it.kind === "agent" ? it.id : it.path)).join("\n"));
      e.dataTransfer.effectAllowed = "copy";
    });
    list.addEventListener("dblclick", (e) => {
      const r = rowOf(e);
      if (r === null) return;
      const a = decoOf(r);
      if (a !== null) return activate(a);
      if (r.path !== null) void ctl.toggle(r.path);
    });

    list.addEventListener("keydown", (e) => {
      e.stopPropagation(); // the canvas's key guard: arrows are the tree's here
      const rows = ctl.model.rows(filter.value);
      const k = ctl.model.key(rows, ctl.model.cursor ?? ctl.model.selected, e.key, e.shiftKey);
      if (!["ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "Home", "End", "Enter"].includes(e.key)) return;
      e.preventDefault();
      ctl.setCursor(k.cursor);
      const fx = k.effect;
      if (fx === undefined) return;
      if (fx.kind === "expand") void ctl.expand(fx.path);
      else if (fx.kind === "collapse") void ctl.collapse(fx.path);
      else if (fx.kind === "select") ctl.select(fx.path);
      else {
        // Shift+Enter: the old plain click on an agent
        ctl.select(fx.path);
        const ag = agentIndex.agentAt(fx.path);
        if (ag !== undefined) activate(deco(ag.id, fx.path));
      }
    });
    // ↓ from the filter goes into the tree; Esc clears it
    filter.addEventListener("keydown", (e) => {
      e.stopPropagation();
      if (e.key === "Escape") {
        filter.value = "";
        draw();
      } else if (e.key === "ArrowDown") {
        e.preventDefault();
        list.focus();
      }
    });
    filter.addEventListener("input", draw);
    newBadge.addEventListener("click", () => {
      const first = d.freshIds()[0];
      const folder = first !== undefined ? agentIndex.folderOf(first) : undefined;
      if (folder !== undefined) void ctl.reveal(folder).then(() => ctl.setCursor(folder));
    });

    // A tree is its folder's view: bound now (a saved `view_agent` serves it again; one saved
    // before views gets the view of its folder). Its first listing waits for that, and a
    // refusal is said on the tree, which stays open.
    const bound = bindCellToView(ctx.kernel, "folder", root, folderAgent, (view) => {
      ctx.kernel.updateMeta(ctx.selfId, { view_agent: view } as unknown as Payload);
    }).then((b) => {
      if (!b.ok) {
        note.hidden = false;
        note.textContent = `${said(b.refusal)} — this tree is not its folder's view`;
      }
    });
    void bound
      .then(() =>
        ctl.start({
          expanded: Array.isArray(saved["expanded"]) ? (saved["expanded"] as string[]) : [],
          ...(typeof saved["selected"] === "string" ? { selected: saved["selected"] } : {}),
        }),
      )
      .then(draw);
    draw();

    return {
      unmount(): void {
        offRender();
        ctl.dispose();
        controllers.delete(ctx.selfId);
        el.remove();
      },
    };
  },
};
