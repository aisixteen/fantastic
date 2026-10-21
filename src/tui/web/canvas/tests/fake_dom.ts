// A SMALL DOM for the tree panel's drag test (#148): elements with classes, attributes,
// text, children and listeners, and a `dispatch` that reaches them. Enough for a tree
// panel to draw its rows and for a `dragstart` to reach its handler; nothing is laid out
// or painted. `innerHTML` takes the flat markup the panels write (tags, classes,
// attributes, text), and selectors are compound ones (`.ft-row.cur`, `[draggable='true']`).

type Listener = (e: FakeEvent) => void;

export interface FakeEvent {
  type: string;
  target: FakeEl | null;
  metaKey: boolean;
  ctrlKey: boolean;
  shiftKey: boolean;
  relatedTarget: null;
  dataTransfer: FakeTransfer | null;
  preventDefault(): void;
}

/** A drag's transfer: what is set on it is what the drop reads back. */
export class FakeTransfer {
  readonly data = new Map<string, string>();
  types: string[] = [];
  effectAllowed = "";
  dropEffect = "";
  setData(type: string, value: string): void {
    this.data.set(type, value);
    if (!this.types.includes(type)) this.types.push(type);
  }
  getData(type: string): string {
    return this.data.get(type) ?? "";
  }
}

const VOID = new Set(["input", "br", "img", "hr"]);

export class FakeEl {
  readonly tagName: string;
  parent: FakeEl | null = null;
  children: FakeEl[] = [];
  readonly attrs = new Map<string, string>();
  private readonly listeners = new Map<string, Listener[]>();
  private text = "";
  hidden = false;
  value = "";
  title = "";
  readonly style: Record<string, string> = {};
  /** the `data-*` attributes, by their names without `data-` */
  readonly dataset: Record<string, string | undefined> = new Proxy({} as Record<string, string | undefined>, {
    get: (_t, k) => (typeof k === "string" ? this.attrs.get(`data-${k}`) : undefined),
    set: (_t, k, v) => (this.attrs.set(`data-${String(k)}`, String(v)), true),
  });

  constructor(tag: string) {
    this.tagName = tag.toUpperCase();
  }

  get className(): string {
    return this.attrs.get("class") ?? "";
  }
  set className(v: string) {
    this.attrs.set("class", v);
  }

  get draggable(): boolean {
    return this.attrs.get("draggable") === "true";
  }
  set draggable(v: boolean) {
    this.attrs.set("draggable", String(v));
  }

  get id(): string {
    return this.attrs.get("id") ?? "";
  }
  set id(v: string) {
    this.attrs.set("id", v);
  }

  get textContent(): string {
    return this.children.length > 0 ? this.children.map((c) => c.textContent).join("") : this.text;
  }
  set textContent(v: string) {
    this.setChildren([]);
    this.text = v;
  }

  set innerHTML(html: string) {
    this.setChildren(parseMarkup(html));
    this.text = "";
  }

  classList = {
    add: (...cs: string[]): void => this.setClasses([...this.classes(), ...cs]),
    remove: (...cs: string[]): void => this.setClasses(this.classes().filter((c) => !cs.includes(c))),
    toggle: (c: string, force?: boolean): boolean => {
      const on = force ?? !this.classes().includes(c);
      if (on) this.classList.add(c);
      else this.classList.remove(c);
      return on;
    },
    contains: (c: string): boolean => this.classes().includes(c),
  };

  private classes(): string[] {
    return this.className.split(/\s+/).filter((c) => c !== "");
  }
  private setClasses(cs: string[]): void {
    this.className = [...new Set(cs)].join(" ");
  }

  setAttribute(k: string, v: string): void {
    this.attrs.set(k, String(v));
  }
  getAttribute(k: string): string | null {
    return this.attrs.get(k) ?? null;
  }

  private setChildren(nodes: FakeEl[]): void {
    for (const c of this.children) c.parent = null;
    this.children = [];
    for (const n of nodes) this.appendChild(n);
  }

  appendChild(c: FakeEl): FakeEl {
    c.remove();
    this.children.push(c);
    c.parent = this;
    return c;
  }

  replaceChildren(...nodes: FakeEl[]): void {
    this.setChildren(nodes);
  }

  remove(): void {
    if (this.parent === null) return;
    this.parent.children = this.parent.children.filter((c) => c !== this);
    this.parent = null;
  }

  contains(node: FakeEl | null): boolean {
    for (let n = node; n !== null; n = n.parent) if (n === this) return true;
    return false;
  }

  focus(): void {}

  addEventListener(type: string, fn: Listener): void {
    this.listeners.set(type, [...(this.listeners.get(type) ?? []), fn]);
  }

  /** The event reaches this element's listeners; its target is this element. */
  dispatch(type: string, init: Partial<FakeEvent> = {}): FakeEvent {
    const e: FakeEvent = {
      type,
      target: this,
      metaKey: false,
      ctrlKey: false,
      shiftKey: false,
      relatedTarget: null,
      dataTransfer: null,
      preventDefault: () => {},
      ...init,
    };
    for (const fn of this.listeners.get(type) ?? []) fn(e);
    return e;
  }

  querySelectorAll<T extends FakeEl = FakeEl>(sel: string): T[] {
    const out: T[] = [];
    const walk = (n: FakeEl): void => {
      for (const c of n.children) {
        if (matches(c, sel)) out.push(c as T);
        walk(c);
      }
    };
    walk(this);
    return out;
  }

  querySelector<T extends FakeEl = FakeEl>(sel: string): T | null {
    return this.querySelectorAll<T>(sel)[0] ?? null;
  }

  closest(sel: string): FakeEl | null {
    for (let n: FakeEl | null = this; n !== null; n = n.parent) if (matches(n, sel)) return n;
    return null;
  }
}

/** A compound selector: a tag, `.class`, `#id` and `[attr]` or `[attr='value']`, all of which must hold. */
function matches(el: FakeEl, sel: string): boolean {
  const re = /\.([\w-]+)|#([\w-]+)|\[([\w-]+)(?:='([^']*)')?\]|^([a-zA-Z][\w-]*)/g;
  let m: RegExpExecArray | null;
  let any = false;
  while ((m = re.exec(sel)) !== null) {
    any = true;
    if (m[1] !== undefined && !el.classList.contains(m[1])) return false;
    if (m[2] !== undefined && el.id !== m[2]) return false;
    if (m[3] !== undefined) {
      const v = el.getAttribute(m[3]);
      if (v === null || (m[4] !== undefined && v !== m[4])) return false;
    }
    if (m[5] !== undefined && el.tagName !== m[5].toUpperCase()) return false;
  }
  if (!any) throw new Error(`fake_dom: an unsupported selector '${sel}'`);
  return true;
}

/** The flat markup the panels write: tags, attributes and text, with no nesting of text. */
function parseMarkup(html: string): FakeEl[] {
  const root: FakeEl[] = [];
  const stack: FakeEl[] = [];
  const tok = /<(\/?)([a-zA-Z][\w-]*)((?:\s+[^\s=>\/]+(?:="[^"]*"|='[^']*'|=[^\s>]+)?)*)\s*(\/?)>|([^<]+)/g;
  let m: RegExpExecArray | null;
  while ((m = tok.exec(html)) !== null) {
    const parent = stack[stack.length - 1];
    if (m[5] !== undefined) {
      if (parent !== undefined && m[5].trim() !== "") parent.textContent = m[5];
      continue;
    }
    if (m[1] === "/") {
      stack.pop();
      continue;
    }
    const el = new FakeEl(m[2]!);
    for (const a of (m[3] ?? "").matchAll(/([^\s=]+)(?:="([^"]*)"|='([^']*)')?/g)) el.setAttribute(a[1]!, a[2] ?? a[3] ?? "");
    if (parent === undefined) root.push(el);
    else parent.appendChild(el);
    const selfClosing = m[4] === "/" || VOID.has(el.tagName.toLowerCase());
    if (!selfClosing) stack.push(el);
  }
  return root;
}

/** The page's document: elements, and the one head the panels' style goes on. */
export class FakeDocument {
  readonly head = new FakeEl("head");
  createElement(tag: string): FakeEl {
    return new FakeEl(tag);
  }
  getElementById(_id: string): null {
    return null;
  }
}

/** Put the fake document on the globals the tree panels reach for. */
export function installFakeDom(): FakeDocument {
  const doc = new FakeDocument();
  (globalThis as Record<string, unknown>)["document"] = doc;
  return doc;
}
