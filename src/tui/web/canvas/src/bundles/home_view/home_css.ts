import { tint, tok } from "../skin/css.ts";

// THE HOME'S STYLE. Every colour the Home paints is ONE custom property in the
// first block below (`#home { --home-… }`), and each is a skin token (`tok`,
// `tint`, phase 4 #49): a skin re-colours the Home with no edit here. The
// statuses' colours are `--home-status-<status>` (`colourVar`); they read the
// skin's status hues — running ok, starting and stopping the host side's orange,
// no_canvas info, unverified warn, stopped subtle text, missing error.
//
// The page slide is three hover beats (0.45 s in the stock skins).

export const HOME_STYLE_ID = "fantastic-home-style";

export const HOME_CSS = `
#home {
  --home-bg: ${tok("color.page.bg")};
  --home-text: ${tok("color.text.primary")};
  --home-bright: ${tok("color.text.bright")};
  --home-muted: ${tok("color.text.muted")};
  --home-subtle: ${tok("color.text.subtle")};
  --home-faint: ${tok("color.text.faint")};
  --home-card: ${tok("color.surface.panel")};
  --home-field: ${tok("color.surface.field")};
  --home-button: ${tok("color.surface.button")};
  --home-line: ${tok("color.line.button")};
  --home-line-strong: ${tok("color.line.strong")};
  --home-row-line: ${tok("color.line.faint")};
  --home-hover: ${tint("color.ink", 6)};
  --home-pill: ${tok("color.surface.pill")};
  --home-accent: ${tok("color.accent.base")};
  --home-accent-on: ${tok("color.accent.on")};
  --home-accent-tint: ${tint("color.accent.base", 22)};
  --home-toast: ${tok("color.surface.toast")};
  --home-toast-text: ${tok("color.status.error-text")};
  --home-notice: ${tok("color.status.warn-surface")};
  --home-notice-text: ${tok("color.status.warn-text")};
  --home-veil: ${tint("color.shade", 60)};
  --home-status-running: ${tok("color.status.ok")};
  --home-status-starting: ${tok("color.side.host")};
  --home-status-stopping: ${tok("color.side.host")};
  --home-status-no_canvas: ${tok("color.status.info")};
  --home-status-unverified: ${tok("color.status.warn")};
  --home-status-stopped: ${tok("color.text.subtle")};
  --home-status-missing: ${tok("color.status.error")};
  --home-row-h: 58px;
}
#home { position: fixed; inset: 0; overflow: hidden; background: var(--home-bg); color: var(--home-text); font: 14px/1.45 ${tok("font.family.sans")}; }
#home [hidden] { display: none !important; }
#home button, #home input { font: inherit; color: inherit; }
#home button { cursor: pointer; background: var(--home-button); border: 1px solid var(--home-line); border-radius: ${tok("radius.control")}; padding: 6px 12px; }
#home button:hover { border-color: var(--home-line-strong); }
#home .dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; flex: 0 0 auto; background: var(--dot, var(--home-status-stopped)); }

#home .carousel { position: absolute; inset: 0; transition: transform calc(3 * ${tok("duration.hover")}) ${tok("easing.standard")}; }
#home .page { position: absolute; top: 0; bottom: 0; width: 100%; left: calc(var(--i, 0) * 100%); overflow: hidden; }
#home .frame-slot { position: absolute; inset: 0; }
#home .canvas-frame { position: absolute; inset: 0; width: 100%; height: 100%; border: 0; display: block; background: var(--home-bg); }

#home .page-home { display: grid; place-items: center; overflow: auto; }
#home .home-card { width: min(560px, calc(100% - 48px)); box-sizing: border-box; padding: 32px 32px 24px; margin: 48px 0 96px; background: var(--home-card);
  border: 1px solid var(--home-line); border-radius: ${tok("radius.panel")}; box-shadow: ${tok("shadow.popup")}; }
#home .home-card h1 { margin: 0; font-size: 26px; font-weight: 700; color: var(--home-bright); letter-spacing: -0.3px; }
#home .home-card .lead { margin: 4px 0 20px; color: var(--home-muted); }
#home .open-folder { background: var(--home-accent); border-color: var(--home-accent); color: var(--home-accent-on); font-weight: 650; padding: 9px 18px; }
#home .open-hint { margin: 8px 0 0; color: var(--home-faint); font-size: 12.5px; }
#home .open-path { display: flex; gap: 8px; }
#home .open-path input { flex: 1; min-width: 0; background: var(--home-field); border: 1px solid var(--home-line); border-radius: ${tok("radius.control")}; padding: 7px 11px; }
#home .open-path input:focus { outline: 2px solid var(--home-accent-tint); outline-offset: -1px; }
#home .home-card h2 { margin: 26px 0 6px; font: 600 11px ${tok("font.family.mono")}; letter-spacing: .14em; color: var(--home-subtle); }
#home .home-empty { color: var(--home-faint); padding: 10px 0; }
#home .home-rows { max-height: calc(5 * var(--home-row-h)); overflow-y: auto; }
#home .home-row { display: flex; align-items: center; gap: 12px; min-height: var(--home-row-h); box-sizing: border-box; padding: 8px 10px; cursor: pointer;
  border-bottom: 1px solid var(--home-row-line); border-radius: ${tok("radius.control")}; }
#home .home-row:hover { background: var(--home-hover); }
#home .home-row .who { flex: 1; min-width: 0; display: flex; flex-direction: column; }
#home .home-row .name { font-weight: 650; color: var(--home-bright); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
#home .home-row .path { color: var(--home-subtle); font-size: 12px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
#home .home-row .meta { display: flex; flex-direction: column; align-items: flex-end; flex: 0 0 auto; font-size: 12px; }
#home .status { display: inline-flex; align-items: center; gap: 6px; color: var(--home-muted); white-space: nowrap; }
#home .last-seen { color: var(--home-faint); white-space: nowrap; }
#home .home-row .acts { display: flex; gap: 6px; flex: 0 0 auto; }
#home .home-row .acts button { padding: 4px 10px; font-size: 12.5px; }

#home .state-card { position: absolute; inset: 0; margin: auto; width: min(480px, calc(100% - 48px)); height: fit-content; box-sizing: border-box; padding: 28px;
  background: var(--home-card); border: 1px solid var(--home-line); border-radius: ${tok("radius.panel")}; box-shadow: ${tok("shadow.popup")}; text-align: center; }
#home .state-card .dot { width: 12px; height: 12px; }
#home .state-card h2 { margin: 12px 0 6px; font-size: 19px; color: var(--home-bright); }
#home .state-card p { margin: 0 0 18px; color: var(--home-muted); overflow-wrap: anywhere; }
#home .state-card .acts { display: flex; gap: 8px; justify-content: center; }

#home .controls { position: absolute; top: 12px; right: 12px; display: flex; gap: 6px; padding: 5px; z-index: 2; background: var(--home-pill);
  border: 1px solid var(--home-line); border-radius: ${tok("radius.pill")}; box-shadow: ${tok("shadow.pill")}; }
#home .controls button, #home .controls a { padding: 4px 10px; font-size: 12.5px; border-radius: ${tok("radius.pill")}; }
#home .controls a { color: inherit; text-decoration: none; border: 1px solid var(--home-line); background: var(--home-button); }

#home .nav-pill { position: absolute; left: 50%; bottom: 16px; transform: translateX(-50%); display: flex; gap: 4px; padding: 5px; z-index: 5; max-width: calc(100% - 32px);
  background: var(--home-pill); border: 1px solid var(--home-line); border-radius: ${tok("radius.pill")}; box-shadow: ${tok("shadow.pill")}; }
#home .nav-pill button { position: relative; display: inline-flex; align-items: center; gap: 7px; border-radius: ${tok("radius.pill")}; padding: 5px 12px;
  background: transparent; border-color: transparent; color: var(--home-muted); }
#home .nav-pill button[aria-current="page"] { background: var(--home-accent-tint); color: var(--home-bright); }
#home .nav-page .x { display: none; margin-left: 2px; color: var(--home-faint); }
#home .nav-page:hover .x { display: inline; }
#home .nav-page .x:hover { color: var(--home-bright); }

#home .status-capsule { position: absolute; left: 16px; bottom: 16px; display: flex; align-items: center; gap: 8px; z-index: 4; max-width: calc(50% - 120px);
  padding: 7px 14px; font-size: 12.5px; white-space: nowrap; background: var(--home-pill); border: 1px solid var(--home-line); border-radius: ${tok("radius.pill")}; }
#home .status-capsule > span { overflow: hidden; text-overflow: ellipsis; min-width: 0; }
#home .status-capsule .name { color: var(--home-bright); font-weight: 650; flex-shrink: 1; }
#home .status-capsule .addr { color: var(--home-subtle); font-family: ${tok("font.family.mono")}; flex-shrink: 100; }
#home .status-capsule .pid { color: var(--home-faint); font-family: ${tok("font.family.mono")}; flex-shrink: 10000; }

#home .problem, #home .notice { position: absolute; left: 50%; transform: translateX(-50%); z-index: 6; display: flex; align-items: center; gap: 12px;
  max-width: min(640px, calc(100% - 32px)); box-sizing: border-box; padding: 9px 10px 9px 16px; border-radius: ${tok("radius.card")}; }
#home .problem { top: 14px; background: var(--home-toast); color: var(--home-toast-text); box-shadow: ${tok("shadow.toast")}; }
#home .notice { top: 66px; background: var(--home-notice); color: var(--home-notice-text); }
#home .problem .text, #home .notice .text { overflow-wrap: anywhere; }

#home dialog.log { width: min(760px, calc(100% - 48px)); max-height: 70vh; padding: 0; color: var(--home-text); background: var(--home-card);
  border: 1px solid var(--home-line); border-radius: ${tok("radius.panel")}; }
#home dialog.log::backdrop { background: var(--home-veil); }
#home dialog.log header { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 10px 14px; border-bottom: 1px solid var(--home-line); }
#home dialog.log pre { margin: 0; padding: 12px 14px; overflow: auto; max-height: calc(70vh - 60px); font: 12px/1.5 ${tok("font.family.mono")}; white-space: pre-wrap; }

/* the lock screen: over everything, kept above the nav, the problem and the dialogs */
#home .lock-overlay { position: absolute; inset: 0; z-index: 100; display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 14px;
  background: var(--home-bg); color: var(--home-bright); }
#home .lock-overlay h1 { margin: 0; font-size: 26px; font-weight: 700; letter-spacing: -0.3px; }
#home .lock-form { display: flex; gap: 8px; }
#home .lock-form input { width: 12em; background: var(--home-field); border: 1px solid var(--home-line); border-radius: ${tok("radius.control")}; padding: 8px 12px; text-align: center; letter-spacing: .3em; }
#home .lock-form input:focus { outline: 2px solid var(--home-accent-tint); outline-offset: -1px; }
#home .lock-form input:disabled { opacity: .5; }
#home .lock-note { margin: 0; min-height: 1.4em; color: var(--home-toast-text); }

#home .home-error { position: absolute; inset: 0; display: grid; place-content: center; padding: 24px; text-align: center; color: var(--home-toast-text); }
`;
