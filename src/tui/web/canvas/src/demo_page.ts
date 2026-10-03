// THE DEMO PAGE (`src/tui/demo/web`, packed to one self-contained html) that the
// action popup's `demo` kind places as an html cell. Set once by the bundle
// entry (`bundle.ts`, which imports the packed file as text); a page built
// without it (the unit tests run the sources, not the bundle) has none.

let html: string | null = null;

export function setDemoHtml(h: string): void {
  html = h;
}

/** The packed demo page, or null when this build carries none. */
export function demoHtml(): string | null {
  return html;
}
