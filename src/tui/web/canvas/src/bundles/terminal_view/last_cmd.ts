// A terminal's LAST LAUNCHED COMMAND and its RUN offer (owner 2026-10-12): the
// host saves what the shell last ran on the exec agent (`last_cmd`, emitted as
// `{type: "last_cmd", cmd}`, `kern/term_watch`). The cell shows it; when its
// shell was started anew (after a restart) it offers "run" — gone once any
// command runs. Pure state, the view draws it.

export interface LastCmd {
  /** the last command the shell launched, or null (none yet) */
  cmd: string | null;
  /** the ▶ run button is shown */
  offer: boolean;
}

/** The record's saved command (a non-empty string), else null. */
export function savedCmd(rec: Record<string, unknown> | undefined): string | null {
  const c = rec?.["last_cmd"];
  return typeof c === "string" && c.trim() !== "" ? c.trim() : null;
}

/** The view attached: `fresh` — it started a NEW shell (no live job to join);
 *  `shell` — the cell is a login shell (a coding agent's cell runs its program). */
export function attached(s: LastCmd, fresh: boolean, shell: boolean): LastCmd {
  return { cmd: s.cmd, offer: fresh && shell && s.cmd !== null };
}

/** The shell launched a command: it is the last one, and the offer is gone. */
export function launched(_s: LastCmd, cmd: string): LastCmd {
  return { cmd, offer: false };
}

/** The human pressed run: the line goes to the shell, the offer is gone. */
export function ran(s: LastCmd): LastCmd {
  return { cmd: s.cmd, offer: false };
}

/** The line run types: the command and Enter. */
export const runLine = (cmd: string): string => `${cmd}\r`;

/** A command shortened for the cell's bar, its start kept. */
export function shortCmd(cmd: string, max = 60): string {
  const one = cmd.replace(/\s+/g, " ").trim();
  return one.length <= max ? one : `${one.slice(0, max - 1)}…`;
}
