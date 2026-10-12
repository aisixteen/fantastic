// A terminal's LAST LAUNCHED COMMAND, and NOTHING STARTS ON ITS OWN (owner
// 2026-10-12). The host saves what the shell last ran on the exec agent
// (`last_cmd`, emitted as `{type: "last_cmd", cmd}`, `kern/term_watch`); the cell
// shows it. A cell's program — its login shell, or claude / codex / any program
// it was placed with — starts when the human places the cell; after that, with no
// live job (a restart, a reload after it exited) it waits STOPPED, an overlay
// saying so, until the human presses ▶ (TOS: no program is launched for them).
// Pure state, the view draws it.

export interface TermState {
  /** the last command the shell launched, or null (none yet) */
  cmd: string | null;
  /** no live job, waiting for ▶ */
  stopped: boolean;
}

/** The record's saved command (a non-empty string), else null. */
export function savedCmd(rec: Record<string, unknown> | undefined): string | null {
  const c = rec?.["last_cmd"];
  return typeof c === "string" && c.trim() !== "" ? c.trim() : null;
}

/** The view attached. `live` — its job runs (joined); `started` — the cell's
 *  program was started before (the cell records it). `start`: whether to start it
 *  now — only the first time, the placement being the human's act. */
export function attach(s: TermState, live: boolean, started: boolean): { state: TermState; start: boolean } {
  if (live) return { state: { cmd: s.cmd, stopped: false }, start: false };
  if (!started) return { state: { cmd: s.cmd, stopped: false }, start: true };
  return { state: { cmd: s.cmd, stopped: true }, start: false };
}

/** The human pressed ▶ (start, or run the last command): it runs again. */
export function pressed(s: TermState): TermState {
  return { cmd: s.cmd, stopped: false };
}

/** The shell launched a command: it is the last one. */
export function launched(s: TermState, cmd: string): TermState {
  return { cmd, stopped: s.stopped };
}

/** The line ▶ run types: the command and Enter. */
export const runLine = (cmd: string): string => `${cmd}\r`;

/** What the cell runs, named for the overlay: the program's own name, else "the shell". */
export function programName(argv: readonly string[]): string {
  const p = argv[0];
  return p === undefined ? "the shell" : p.split("/").pop() || p;
}

/** The stopped overlay's words. */
export function stoppedText(argv: readonly string[]): { title: string; note: string } {
  return {
    title: `${programName(argv)} is not running`,
    note: "Nothing starts on its own after a restart — press ▶ to start it.",
  };
}

/** A command shortened for the cell's bar, its start kept. */
export function shortCmd(cmd: string, max = 60): string {
  const one = cmd.replace(/\s+/g, " ").trim();
  return one.length <= max ? one : `${one.slice(0, max - 1)}…`;
}
