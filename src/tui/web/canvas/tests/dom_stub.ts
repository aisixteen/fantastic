// A STAND-IN DOM for mounting a cell's view under node: every element is a stub that
// takes any property and any call, keeps what is set on it, and has no children and
// no events. Enough for a view to mount and unmount; nothing is drawn or measured.

type Stub = Record<PropertyKey, unknown> & (() => Stub);

export function stub(): Stub {
  const set = new Map<PropertyKey, unknown>();
  return new Proxy(function () {} as unknown as Stub, {
    get(_t, k) {
      if (set.has(k)) return set.get(k);
      if (k === Symbol.toPrimitive) return () => "";
      if (k === Symbol.iterator) return function* () {};
      if (typeof k === "symbol") return undefined; // not a regexp, nor any other protocol
      if (k === "then") return undefined; // not a promise
      if (k === "length") return 0;
      return stub();
    },
    set(_t, k, v) {
      set.set(k, v);
      return true;
    },
    apply: () => stub(),
  });
}

/** Put the stand-in on the globals a view reaches for. */
export function installDom(): void {
  const g = globalThis as Record<string, unknown>;
  g["document"] = stub();
  g["requestAnimationFrame"] = () => 0;
  g["cancelAnimationFrame"] = () => {};
  g["getComputedStyle"] = () => stub();
  g["HTMLElement"] ??= class {};
}
