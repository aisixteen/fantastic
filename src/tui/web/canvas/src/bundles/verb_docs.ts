// HOW TO CALL the page's agents — what the page kernel's `docs` serves. The ts
// kernel answers `docs {of: <agent>}` from the agent's own `verbs`, which a
// record does not carry; so every page agent of a bundle listed here is given
// its bundle's table as it appears (seeded, placed or hydrated): `documentPage`.
import type { Kernel } from "../lib.ts";

export const PAGE_VERB_DOCS: { [handlerModule: string]: { [verb: string]: string } } = {
  "view.ts": {
    place: "args: kind:str (html|gl|terminal|chat) or handler_module:str — OR only backend_id:str (a VIEW of that host agent, drawn as its record says: its `view`, else terminal for an exec agent, chat for an AI, else its reflect card) — x:num, y:num (req), id:str?, width:num?, height:num?, display_name:str?, bytes:str? (the body, verbatim), trusted:bool?, can_call:dict?, callable_by:dict?, backend_id:str? (terminal/chat). A new cell of this view. {placed, kind, ignored?, grants_missing?, hint?}. A PANEL'S JS (an html cell's body) has `fantastic`: `await fantastic.send(id, payload)` → the reply · `fantastic.watch(id, ev => …)` → events (returns off) · `fantastic.emit(id, payload)` · `fantastic.onMessage(p => …)`. Its calls are judged by the CELL's can_call — grant what it calls (`{<id>: [verbs…, watch]}`); a reply's `grants_missing` names any it lacks. On the host they arrive as `user`: a host agent you create for a panel needs `callable_by: {user: [<the verbs the panel calls>, watch]}` in its create_agent (a new agent admits no one); never drop `user` from an existing agent's callable_by. Example — a button that runs a command on a host exec agent `runner` and shows its output live: place {kind: html, x: 700, y: 60, can_call: {runner: [start, watch]}, bytes: <button onclick=\"fantastic.send('runner',{type:'start',command:'ls'}).then(r=>job=r.job_id)\">Run</button><pre id=o></pre><script>let job;fantastic.watch('runner',e=>{if(e.job_id===job&&e.type==='progress')o.textContent+=e.line+'\\n'})</script>}",
    move: "args: id:str (req), x:num?, y:num?, width:num?, height:num?. Moves / resizes a cell. {moved, …}.",
    fit: "args: id:str (req), max_width:num?, max_height:num? (default 1600 × 1200). Sizes an html cell to what it draws: the width its content wants, then its height at that width. {fitted, x, y, width, height, content, overflowed} — overflowed: the content is bigger than the max. A cell keeps its size otherwise (480×320 when place names none): after you place or update an html cell, call fit.",
    update: "args: id:str (req — a cell, or this view), bytes:str? (a new body, verbatim), fields:dict? (display_name|trusted|readme|x|y|width|height|can_call|callable_by; on the view any field). {updated, fields, grants_missing?, hint?}.",
    wire: "args: sender:str, receiver:str, verbs:[str] (req). A grant: merged into each side that is a cell of this view (sender's can_call, receiver's callable_by); e.g. verbs [emit] lets a panel emit to another. Not from/to. {wired}.",
    remove: "args: id:str (req). Removes a cell; refused on a delete_lock (the background). {deleted}.",
    reflect: "No args. This view, its members and its readme.",
  },
  "html_agent.ts": {
    get_html: "No args. {id, html}: this cell's body.",
    set_html: "args: bytes:str (req — the new html, verbatim). Replaces the body and reloads the panel. {ok, bytes, grants_missing?, hint?}. A PANEL'S JS (an html cell's body) has `fantastic`: `await fantastic.send(id, payload)` → the reply · `fantastic.watch(id, ev => …)` → events (returns off) · `fantastic.emit(id, payload)` · `fantastic.onMessage(p => …)`. Its calls are judged by the CELL's can_call — grant what it calls (`{<id>: [verbs…, watch]}`); a reply's `grants_missing` names any it lacks. On the host they arrive as `user`: a host agent you create for a panel needs `callable_by: {user: [<the verbs the panel calls>, watch]}` in its create_agent (a new agent admits no one); never drop `user` from an existing agent's callable_by.",
    reflect: "No args.",
  },
  "gl_agent.ts": {
    get_gl_view: "No args. This GL cell's source and view.",
    set_gl_source: "args: bytes:str (req — the GL source, verbatim). Replaces the scene.",
    reflect: "No args.",
  },
  "background.ts": {
    set_bg: "args: bytes:str (req — GL source, verbatim, non-empty). Sets the view's background scene.",
    default_bg: "No args. Restores the bundled background scene.",
    clear_bg: "No args. Clears the background.",
    get_bg: "No args. The background's source.",
    get_gl_view: "No args. The background's GL view.",
    reflect: "No args.",
  },
};

/** Give every page agent of a listed bundle its docs (once; an agent that
 *  already carries `verbs` keeps them). */
export function documentPage(kernel: Kernel): void {
  for (const a of kernel.agents.values()) {
    if (a.verbs === null && a.handlerModule !== null && PAGE_VERB_DOCS[a.handlerModule] !== undefined) {
      a.verbs = { ...PAGE_VERB_DOCS[a.handlerModule]! };
    }
  }
}
