import type { Payload } from "../../lib.ts";
import type { ViewBundle, ViewContext, ViewHandle } from "../view.ts";
import { onMac } from "../canvas/viewer.ts";
import { bindCellToView, fileGrant, readBytes, release, routedBridge, said, sharedHub } from "../files/file_bridge.ts";
import { agentIndex, ensureWalker, filesDeps, onRender, onSelection, selection } from "../files/files_deps.ts";
import { extOf, HIGHLIGHT_MAX, human, PAGE } from "./preview_model.ts";
import { lineDiff } from "./diff.ts";
import { langOf, tokens } from "./highlight.ts";
import { dirOf, parseMarkdown, renderMarkdown } from "./markdown.ts";
import { PreviewController, previews } from "./preview_controller.ts";
import type { PreviewMeta } from "./preview_controller.ts";
import { ensurePreviewStyle } from "./styles.ts";

// THE PREVIEWER, as a cell (#106): a file or the live agent. It follows the file
// tree's selection or is pinned to one path, shows text highlighted (and live from
// disk), Markdown rendered, images and PDF drawn, and edits text through the file
// bridge with the conflict made loud. `preview_controller.ts` holds the state and
// the verbs; this draws them.

export const PREVIEW_VIEW = "preview_view.ts";

const MIME: Record<string, string> = {
  png: "image/png", jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp",
  svg: "image/svg+xml", bmp: "image/bmp", ico: "image/x-icon", avif: "image/avif", pdf: "application/pdf",
};

const KIND: Record<string, string> = { kw: "kw", str: "str", com: "com", num: "num", type: "type", punct: "punct" };

/** Highlighted source as nodes: spans of the token classes, the text exactly as given. */
function highlighted(src: string, name: string, into: HTMLElement): void {
  const toks = tokens(src, langOf(name));
  const frag = document.createDocumentFragment();
  for (const t of toks) {
    const k = KIND[t.k];
    if (k === undefined) {
      frag.appendChild(document.createTextNode(t.t));
    } else {
      const s = document.createElement("span");
      s.className = `tk-${k}`;
      s.textContent = t.t;
      frag.appendChild(s);
    }
  }
  // a final newline keeps the last (empty) line's height, as the textarea over it has
  frag.appendChild(document.createTextNode("\n"));
  into.replaceChildren(frag);
}

const gutterOf = (text: string): string => Array.from({ length: text.split("\n").length }, (_, i) => String(i + 1)).join("\n");

export const previewView: ViewBundle = {
  handles: [PREVIEW_VIEW],
  grants: () => fileGrant(),
  mount(ctx: ViewContext): ViewHandle {
    const d = filesDeps("preview_view");
    ensurePreviewStyle();
    const bridge = routedBridge(ctx.kernel);
    ensureWalker(bridge);
    // a preview opened on a file agent (#150) is that agent's view: its file is served by it
    const fileAgent = (ctx.record ?? {})["view_agent"];
    const pinnedPath = (ctx.record ?? {})["pinned"];
    const mac = onMac();
    const ctl = new PreviewController({
      bridge,
      hub: sharedHub(ctx.kernel),
      selfId: ctx.selfId,
      index: agentIndex,
      persist: (m) => {
        ctx.kernel.updateMeta(ctx.selfId, m as unknown as Payload);
      },
      selection,
    });
    previews.set(ctx.selfId, ctl);
    ctl.restore((ctx.record ?? {}) as Partial<PreviewMeta>);

    const el = document.createElement("div");
    el.className = "preview-view";
    el.innerHTML = `<div class="pv-bar"><span class="pv-title"></span></div><div class="pv-note" role="status" hidden></div><div class="pv-conflict" role="alert" hidden></div><div class="pv-body"></div>`;
    ctx.mount.appendChild(el);
    const bar = el.querySelector(".pv-bar") as HTMLElement;
    const title = el.querySelector(".pv-title") as HTMLElement;
    const note = el.querySelector(".pv-note") as HTMLElement;
    const conflict = el.querySelector(".pv-conflict") as HTMLElement;
    const body = el.querySelector(".pv-body") as HTMLElement;

    let localNote: string | null = null;
    let blobUrls: string[] = [];
    let agentHandle: ViewHandle | null = null;
    let bodyKey = "";
    let input: HTMLTextAreaElement | null = null;
    let rePaint: (() => void) | null = null;

    const button = (parent: HTMLElement, text: string, title_: string, onClick: () => void, cls = ""): HTMLButtonElement => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = `pv-chip ${cls}`.trim();
      b.textContent = text;
      b.title = title_;
      b.addEventListener("click", (e) => {
        e.stopPropagation();
        onClick();
      });
      parent.appendChild(b);
      return b;
    };

    const freeBody = (): void => {
      for (const u of blobUrls) URL.revokeObjectURL(u);
      blobUrls = [];
      agentHandle?.unmount();
      agentHandle = null;
      input = null;
      rePaint = null;
    };
    const blobUrl = (bytes: Uint8Array, mime: string): string => {
      const u = URL.createObjectURL(new Blob([bytes as BlobPart], { type: mime }));
      blobUrls.push(u);
      return u;
    };

    // ─── the bar ──────────────────────────────────────────────────────────
    const drawBar = (): void => {
      const live = ctl.isLive() && !ctl.gone;
      title.textContent = ctl.title();
      title.title = ctl.path ?? "";
      if (live) {
        const s = document.createElement("span");
        s.className = "pv-live";
        s.textContent = " · live";
        title.appendChild(s);
      }
      if (ctl.dirty) {
        const s = document.createElement("span");
        s.className = "pv-unsaved";
        s.textContent = " ● unsaved";
        title.appendChild(s);
      }
      for (const c of [...bar.querySelectorAll(".pv-chip")]) c.remove();
      const agentOnly = ctl.path === null && ctl.agent !== null;
      if (ctl.isAgent && !agentOnly) {
        button(bar, ctl.content.k === "agent" ? "⇄ agent" : "⇄ file", ctl.content.k === "agent" ? "show the agent's files" : "show the live agent", () => void ctl.setFileMode(ctl.fileMode === "agent" ? "file" : "agent").then(say));
      }
      const pinned = "pinned" in ctl.binding;
      button(
        bar,
        "📌 pin",
        pinned ? "pinned: click to follow the tree again" : "pin this preview to what it shows",
        () => {
          if (pinned) void ctl.unpin();
          else if (ctl.path !== null) void ctl.pin(ctl.path);
          else say("nothing to pin: select a file first");
        },
        pinned ? "active" : "",
      );
      if (ctl.mode === "markdown" && ctl.content.k === "text" && !ctl.editing) {
        button(bar, "‹› source", "show the Markdown source", () => ctl.setSource(!ctl.source), ctl.source ? "active" : "");
      }
      const can = ctl.canEdit() || ctl.editing;
      button(
        bar,
        "✎ edit",
        can ? "edit this file" : `not editable: ${ctl.whyNot()}`,
        () => {
          const why = ctl.setEditing(!ctl.editing);
          say(why);
        },
        `${ctl.editing ? "active" : ""} ${can ? "" : "off"}`.trim(),
      ).setAttribute("aria-disabled", String(!can));
      if (ctl.editing) {
        const blocked = ctl.editor.state.k !== "dirty" || ctl.gone;
        const save = button(bar, "💾 save", ctl.gone ? "the file was deleted on disk: saving waits until it is back" : `save (${mac ? "⌘" : "Ctrl"}+S)`, () => void ctl.save(), blocked ? "off" : "");
        save.disabled = blocked;
        button(bar, "↺ revert", "drop the unsaved edits, or undo the last save", () => void ctl.revert());
      }
    };
    const say = (m: string | null): void => {
      localNote = m;
      drawNotes();
    };
    const drawNotes = (): void => {
      const n = ctl.notice ?? ctl.editor.error ?? localNote;
      note.hidden = n === null || n === "";
      note.textContent = n ?? "";
      // a change on disk under unsaved edits: loud, three ways out
      const s = ctl.editor.state;
      conflict.replaceChildren();
      conflict.hidden = true;
      if (ctl.editing && (s.k === "conflict" || s.k === "comparing")) {
        conflict.hidden = false;
        const msg = document.createElement("span");
        msg.className = "msg";
        msg.textContent = "Changed on disk while you have unsaved edits.";
        conflict.appendChild(msg);
        button(conflict, "Reload", "drop my edits, take the disk's", () => void ctl.reload());
        button(conflict, "Keep mine", "overwrite the disk's change with my edits", () => void ctl.keepMine());
        button(conflict, "Compare", "mine and the disk's, side by side", () => ctl.compare());
      } else if (ctl.editing && ctl.gone && ctl.dirty) {
        conflict.hidden = false;
        const msg = document.createElement("span");
        msg.className = "msg";
        msg.textContent = "Deleted on disk while you have unsaved edits. Saving waits until the file is back; revert drops the edits.";
        conflict.appendChild(msg);
      }
    };

    // ─── the body ─────────────────────────────────────────────────────────
    const message = (cls: string, text: string): void => {
      const p = document.createElement("div");
      p.className = cls;
      p.textContent = text;
      body.replaceChildren(p);
    };

    const textBody = (): void => {
      const text = ctl.editor.text();
      const name = ctl.name();
      const big = text.length > HIGHLIGHT_MAX;
      const scroll = document.createElement("div");
      scroll.className = "pv-scroll";
      const gutter = document.createElement("pre");
      gutter.className = "pv-gutter";
      const stack = document.createElement("div");
      stack.className = "pv-stack";
      const code = document.createElement("pre");
      code.className = "pv-code";
      stack.appendChild(code);
      scroll.append(gutter, stack);
      const paint = (src: string): void => {
        gutter.textContent = gutterOf(src);
        if (big) code.textContent = `${src}\n`;
        else highlighted(src, name, code);
      };
      paint(text);
      if (big) say("a large file: shown without highlighting");
      if (ctl.editing) {
        const ta = document.createElement("textarea");
        ta.className = "pv-input";
        ta.value = text;
        ta.spellcheck = false;
        ta.setAttribute("wrap", "off");
        ta.setAttribute("autocapitalize", "off");
        ta.setAttribute("aria-label", `edit ${name}`);
        const lock = (): void => {
          ta.readOnly = ctl.editor.state.k === "saving" || ctl.editor.inConflict();
        };
        lock();
        let frame = 0;
        ta.addEventListener("input", () => {
          ctl.edit(ta.value);
          if (frame === 0) {
            frame = requestAnimationFrame(() => {
              frame = 0;
              paint(ta.value);
            });
          }
          reveal();
        });
        // keep the caret's line in view: the box is as tall as the text, the scroll is its parent's
        const reveal = (): void => {
          const line = ta.value.slice(0, ta.selectionStart).split("\n").length - 1;
          const lh = parseFloat(getComputedStyle(ta).lineHeight) || 18;
          const y = 8 + line * lh;
          if (y < scroll.scrollTop) scroll.scrollTop = y;
          else if (y + lh > scroll.scrollTop + scroll.clientHeight) scroll.scrollTop = y + lh - scroll.clientHeight;
        };
        ta.addEventListener("keydown", (e) => {
          e.stopPropagation(); // the canvas's key guard: the editor's keys are its own
          if ((mac ? e.metaKey : e.ctrlKey) && e.key.toLowerCase() === "s") {
            e.preventDefault();
            void ctl.save();
          } else if (e.key === "Tab" && !e.shiftKey && !ta.readOnly) {
            e.preventDefault();
            ta.setRangeText("  ", ta.selectionStart, ta.selectionEnd, "end");
            ta.dispatchEvent(new Event("input"));
          }
        });
        ta.addEventListener("keyup", reveal);
        ta.addEventListener("click", reveal);
        stack.appendChild(ta);
        input = ta;
        rePaint = lock;
        // the caret lands in the text: a file opened for editing is typed into at once
        queueMicrotask(() => {
          if (document.activeElement !== ta) ta.focus({ preventScroll: true });
        });
      }
      body.replaceChildren(scroll);
    };

    const compareBody = (): void => {
      const s = ctl.editor.state;
      if (s.k !== "comparing") return;
      const d2 = lineDiff(s.theirs, s.mine);
      if ("tooLarge" in d2) {
        message("pv-error", "Too large to compare (over 4000 lines a side). Reload or keep mine.");
        return;
      }
      const wrap = document.createElement("div");
      wrap.className = "pv-diff";
      const disk = document.createElement("div");
      const mine = document.createElement("div");
      for (const [box, h] of [[disk, "on disk"], [mine, "mine"]] as const) {
        const head = document.createElement("h4");
        head.textContent = h;
        box.appendChild(head);
      }
      for (const l of d2) {
        const line = (cls: string, text: string): HTMLElement => {
          const p = document.createElement("pre");
          p.className = cls;
          p.textContent = text === "" ? " " : text;
          return p;
        };
        if (l.op === "same") {
          disk.appendChild(line("", l.text));
          mine.appendChild(line("", l.text));
        } else if (l.op === "del") {
          disk.appendChild(line("del", l.text));
          mine.appendChild(line("blank", ""));
        } else {
          disk.appendChild(line("blank", ""));
          mine.appendChild(line("add", l.text));
        }
      }
      wrap.append(disk, mine);
      body.replaceChildren(wrap);
    };

    const markdownBody = (): void => {
      const ast = parseMarkdown(ctl.editor.text());
      const dir = dirOf(ctl.path ?? "");
      const view = renderMarkdown(ast, document, {
        dir,
        open: (p) => void ctl.pin(p),
        image: (img, p) => {
          void readBytes(bridge, p, 16 * 1024 * 1024).then((r) => {
            if (r.ok) img.src = blobUrl(r.bytes, MIME[extOf(p)] ?? "application/octet-stream");
            else img.alt = `${img.alt} (${r.refusal.error})`;
          });
        },
      });
      body.replaceChildren(view);
    };

    const dirBody = (entries: Array<{ name: string; type: string; size?: number }>): void => {
      const wrap = document.createElement("div");
      wrap.className = "pv-dir";
      const sorted = [...entries].sort((a, b) => (a.type === b.type ? a.name.localeCompare(b.name) : a.type === "dir" ? -1 : 1));
      if (sorted.length === 0) message("pv-empty", "an empty folder");
      for (const e of sorted) {
        const row = document.createElement("div");
        row.className = "e";
        const n = document.createElement("span");
        n.className = "n";
        n.textContent = `${e.type === "dir" ? "📁" : "⋯"} ${e.name}`;
        const s = document.createElement("span");
        s.className = "s";
        s.textContent = e.type === "dir" ? "" : human(e.size ?? -1);
        row.append(n, s);
        wrap.appendChild(row);
      }
      if (sorted.length > 0) body.replaceChildren(wrap);
    };

    const card = (lines: string[]): void => {
      const c = document.createElement("div");
      c.className = "pv-card";
      lines.forEach((l, i) => {
        const p = document.createElement("div");
        if (i === 0) p.className = "big";
        p.textContent = l;
        c.appendChild(p);
      });
      body.replaceChildren(c);
    };

    const drawBody = (): void => {
      freeBody();
      localNote = null;
      const c = ctl.content;
      const s = ctl.editor.state;
      if (c.k === "text" && s.k === "comparing") return compareBody();
      switch (c.k) {
        case "none":
          return message("pv-empty", "Select a file or an agent in the tree.");
        case "error":
          return message("pv-error", c.error);
        case "deleted":
          return card([`${ctl.name()} was deleted on disk`, "it shows again if it comes back"]);
        case "dir":
          return dirBody(c.entries);
        case "text":
          return ctl.mode === "markdown" && !ctl.source && !ctl.editing ? markdownBody() : textBody();
        case "image": {
          const wrap = document.createElement("div");
          wrap.className = "pv-img";
          const img = document.createElement("img");
          img.alt = ctl.name();
          img.src = blobUrl(c.bytes, MIME[extOf(ctl.name())] ?? "image/png"); // svg through <img>: no script runs
          wrap.appendChild(img);
          body.replaceChildren(wrap);
          return;
        }
        case "pdf": {
          const o = document.createElement("object");
          o.className = "pv-pdf";
          o.type = "application/pdf";
          o.data = blobUrl(c.bytes, "application/pdf");
          const fb = document.createElement("div");
          fb.className = "pv-card";
          fb.textContent = `This browser cannot show the PDF inline (${human(c.bytes.length)}).`;
          o.appendChild(fb);
          body.replaceChildren(o);
          return;
        }
        case "binary":
          return card([ctl.name(), `${extOf(ctl.name()) || "no extension"} · ${human(c.size)}`, "binary: no content is shown"]);
        case "large": {
          const pages = Math.max(1, Math.ceil(c.size / PAGE));
          const pager = document.createElement("div");
          pager.className = "pv-pager";
          button(pager, "‹ prev", "the previous 64 KiB", () => void ctl.page(c.page - 1), c.page === 0 ? "off" : "");
          const lbl = document.createElement("span");
          lbl.textContent = `page ${c.page + 1}/${pages} · ${human(c.size)} · not editable (over 1 MiB)`;
          button(pager, "next ›", "the next 64 KiB", () => void ctl.page(c.page + 1), c.page + 1 >= pages ? "off" : "");
          pager.insertBefore(lbl, pager.lastChild);
          const scroll = document.createElement("div");
          scroll.className = "pv-scroll";
          scroll.style.gridTemplateColumns = "1fr";
          const pre = document.createElement("pre");
          pre.className = "pv-code";
          pre.textContent = c.text;
          scroll.appendChild(pre);
          body.replaceChildren(pager, scroll);
          return;
        }
        case "agent": {
          const host = document.createElement("div");
          host.className = "pv-agent";
          body.replaceChildren(host);
          const mine = ctl.agent!;
          void d.mountAgentView(mine, host, `${ctx.selfId}:${mine}`).then(
            (h) => {
              // the preview moved on while it mounted
              if (ctl.content.k !== "agent" || ctl.agent !== mine || !host.isConnected) h.unmount();
              else agentHandle = h;
            },
            (err: Error) => message("pv-error", `agent ${mine}: ${err.message}`),
          );
          return;
        }
      }
    };

    /** What decides whether the body is drawn again: the shown thing, not the typing in it. */
    const keyOf = (): string => {
      const s = ctl.editor.state;
      const c = ctl.content;
      const sub = c.k === "text" ? `${ctl.editor.version()}:${ctl.editor.generation}:${s.k === "comparing" ? "cmp" : "txt"}:${ctl.source}:${ctl.editing}` : c.k === "large" ? `${c.page}` : c.k === "dir" ? c.entries.map((e) => e.name + e.size).join("|") : c.k === "agent" ? c.agent : c.k === "error" ? c.error : "";
      return `${ctl.path}:${ctl.mode}:${c.k}:${sub}`;
    };

    const draw = (): void => {
      drawBar();
      drawNotes();
      const key = keyOf();
      if (key !== bodyKey) {
        const caret = input !== null && document.activeElement === input ? [input.selectionStart, input.selectionEnd] : null;
        bodyKey = key;
        drawBody();
        if (caret !== null && input !== null) {
          const ta: HTMLTextAreaElement = input;
          ta.setSelectionRange(Math.min(caret[0]!, ta.value.length), Math.min(caret[1]!, ta.value.length));
        }
      } else rePaint?.();
    };

    // A pinned preview is its file's view: bound now (a saved `view_agent` serves it again;
    // one saved before views gets the view of its file). Its first read waits for that, and a
    // refusal is said in the cell, which stays open.
    const bound =
      typeof pinnedPath === "string"
        ? bindCellToView(ctx.kernel, "file", pinnedPath, typeof fileAgent === "string" ? fileAgent : undefined, (view) => {
            ctx.kernel.updateMeta(ctx.selfId, { view_agent: view } as unknown as Payload);
          }, undefined, ctx.selfId).then((b) => {
            if (!b.ok) say(`${said(b.refusal)} — this preview is not its file's view`);
          })
        : Promise.resolve();

    ctl.onChange(draw);
    const offRender = onRender(draw);
    const offSel = onSelection(() => {
      if ("follow" in ctl.binding) void ctl.refresh();
    });
    draw();
    void bound.then(() => ctl.refresh());

    return {
      unmount(): void {
        offSel();
        offRender();
        ctl.dispose();
        if (typeof pinnedPath === "string") release(ctx.kernel, pinnedPath, ctx.selfId);
        previews.delete(ctx.selfId);
        freeBody();
        el.remove();
      },
    };
  },
};
