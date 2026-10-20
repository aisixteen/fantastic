// WHO HOLDS WHICH WATCH, page-wide. Every cell on the page calls the file
// bridge as the same caller (`user`), and the bridge keys a `watch_path` hold by
// caller: one cell's `unwatch_path` would end every other cell's watch of that
// path. So the page counts the holders itself and only the FIRST acquire
// watches and only the LAST release unwatches. Pure.

export class WatchLedger {
  private readonly holders = new Map<string, Set<string>>();

  /** `"watch"` when `owner` is the first holder of `path`, else null. */
  acquire(path: string, owner: string): "watch" | null {
    let set = this.holders.get(path);
    if (set === undefined) this.holders.set(path, (set = new Set()));
    const first = set.size === 0;
    set.add(owner);
    return first ? "watch" : null;
  }

  /** `"unwatch"` when `owner` was the last holder of `path`, else null. */
  release(path: string, owner: string): "unwatch" | null {
    const set = this.holders.get(path);
    if (set === undefined || !set.delete(owner)) return null;
    if (set.size > 0) return null;
    this.holders.delete(path);
    return "unwatch";
  }

  /** `owner` lets go of everything: the paths now held by no one. */
  releaseAll(owner: string): string[] {
    const out: string[] = [];
    for (const path of [...this.holders.keys()]) if (this.release(path, owner) === "unwatch") out.push(path);
    return out;
  }

  /** The holders of `path`. */
  ownersOf(path: string): string[] {
    return [...(this.holders.get(path) ?? [])];
  }

  /** Every path held. */
  held(): string[] {
    return [...this.holders.keys()].sort();
  }
}
