// Watching paths (owner, 2026-10-08) — python `file_bridge._watch_path`, text for
// text.
//
// `watch_path {path}` puts an EXPOSED file or directory under a disk watcher
// (DispatchSource vnode sources — native kqueue events): any change to it, by any
// writer (this bridge, an editor, another process), is emitted on this bridge's own
// inbox as `{type: changed, path, kind}`. A caller receives them by WATCHING this
// bridge. A file is watched through its parent directory as well as itself, so an
// editor's atomic save (a temp file renamed over) is seen. Each path is held per
// caller; the disk watch stops when no caller holds any path. RAM only.
//
// A vnode source says THAT something changed, not what: every event schedules one
// debounced rescan, which stats every tracked path and diffs against the last look
// — a path that appeared is `added` (a new inode under the name too: the rename an
// atomic save makes), one that vanished `deleted`, one whose mtime or size moved
// `modified`.

import Dispatch
import FantasticJSON
import FantasticKernel
import Foundation
import OrderedCollections

/// A burst of disk events (an editor's save) inside this window is one `changed`
/// per path (python `WATCH_DEBOUNCE_MS`).
let WATCH_DEBOUNCE_MS = 50

/// Every bridge's watches, per kernel.
final class Watches: @unchecked Sendable {
    private struct Held {
        var paths: [String: Set<String>] = [:]
        var watcher: DiskWatcher?
    }

    private let lock = NSLock()
    private var byBridge: [String: Held] = [:]

    private func key(_ kernel: Kernel, _ bridge: AgentId) -> String {
        "\(kernel.serial)\u{0}\(bridge.value)"
    }

    /// Who holds a watch: the sending agent, or the operator.
    private func caller() -> String {
        KernelTaskLocals.currentOrigin?.value ?? "kernel"
    }

    /// `watch_path {path}` after the exposure gate.
    func watchPath(
        _ bridge: AgentId, root: String, path: String, base: String, kernel: Kernel
    ) -> JSON {
        let norm = allowNorm(path) ?? "."
        let parent = norm.lastIndex(of: "/").map { String(norm[..<$0]) } ?? "."
        do throws(FsError) {
            let isDir = try fsIsDir(root, norm, base: base)
            let parentIsDir = try fsIsDir(root, parent, base: base)
            if !(isDir || parentIsDir) {
                return notFound(
                    "watch_path: no directory \(pyRepr(parent)) to watch \(pyRepr(path)) in",
                    path: path)
            }
        } catch {
            return Refusal.make(Refusal.transportError, error.message)
        }
        let k = key(kernel, bridge)
        let caller = caller()
        let paths = lock.withLock {
            byBridge[k, default: Held()].paths[norm, default: []].insert(caller)
            return byBridge[k]!.paths.keys.sorted()
        }
        rewatch(bridge, root: root, base: base, kernel: kernel)
        return .object([
            "watching": .string(norm), "paths": .array(paths.map { .string($0) }),
        ])
    }

    /// `unwatch_path {path}`: drop the caller's hold.
    func unwatchPath(
        _ bridge: AgentId, root: String, path: String, base: String, kernel: Kernel
    ) -> JSON {
        let norm = allowNorm(path) ?? "."
        let k = key(kernel, bridge)
        let caller = caller()
        let paths = lock.withLock {
            if var holders = byBridge[k]?.paths[norm] {
                holders.remove(caller)
                byBridge[k]!.paths[norm] = holders.isEmpty ? nil : holders
            }
            return (byBridge[k]?.paths.keys.sorted()) ?? []
        }
        rewatch(bridge, root: root, base: base, kernel: kernel)
        return .object(["watching": .bool(false), "paths": .array(paths.map { .string($0) })])
    }

    /// The bridge is going away: its watcher (and so its task) goes with it.
    func drop(_ kernel: Kernel, _ bridge: AgentId) {
        let held = lock.withLock { byBridge.removeValue(forKey: key(kernel, bridge)) }
        held?.watcher?.stop()
    }

    /// (Re)start the ONE disk watcher a bridge runs, over the paths held now. The
    /// old watcher is stopped first: its stream finishes and its task ends.
    private func rewatch(_ bridge: AgentId, root: String, base: String, kernel: Kernel) {
        let k = key(kernel, bridge)
        let (old, paths) = lock.withLock { () -> (DiskWatcher?, [String]) in
            let old = byBridge[k]?.watcher
            byBridge[k]?.watcher = nil
            return (old, byBridge[k].map { Array($0.paths.keys) } ?? [])
        }
        old?.stop()
        if paths.isEmpty { return }
        // resolved target → its path under the root
        var files: [String: String] = [:]
        var dirs: [String: String] = [:]
        for norm in paths {
            let rel = norm == "." ? "" : norm
            if (try? fsIsDir(root, norm, base: base)) == true {
                if let abs = try? fsResolve(root, rel, base: base) { dirs[abs] = norm }
            } else {
                // A file that may not exist yet: its PARENT resolves, the leaf is
                // the name the watcher waits for.
                let cut = rel.lastIndex(of: "/")
                let parentRel = cut.map { String(rel[..<$0]) } ?? ""
                let name = cut.map { String(rel[rel.index(after: $0)...]) } ?? rel
                if let parent = try? fsResolve(root, parentRel, base: base) {
                    files[parent + "/" + name] = norm
                }
            }
        }
        let watcher = DiskWatcher(files: files, dirs: dirs)
        let installed = lock.withLock { () -> Bool in
            guard byBridge[k] != nil else { return false }
            byBridge[k]!.watcher = watcher
            return true
        }
        guard installed else {
            watcher.stop()
            return
        }
        let stream = watcher.stream
        kernel.spawnTask(agent: bridge, verb: "watch_path") {
            for await batch in stream {
                for (rel, kind) in batch {
                    await kernel.emit(
                        bridge,
                        .object([
                            "type": .string("changed"), "path": .string(rel), "kind": .string(kind),
                        ]))
                }
            }
        }
    }
}

/// One bridge's disk watcher: a vnode source on every watched directory (and on
/// each watched file's parent) and on every tracked file, one debounced rescan per
/// burst, the changes yielded as `[(path under the root, kind)]`.
final class DiskWatcher: @unchecked Sendable {
    let stream: AsyncStream<[(String, String)]>
    private let out: AsyncStream<[(String, String)]>.Continuation
    private let queue = DispatchQueue(label: "file_bridge.watch_path")
    private let files: [String: String]
    private let dirs: [String: String]
    /// resolved path → its source and the inode it is open on
    private var sources: [String: (DispatchSourceFileSystemObject, UInt64)] = [:]
    /// tracked path → its path under the root, as of the last look
    private var tracked: [String: String] = [:]
    /// tracked path → its stamp, as of the last look (absent = not there)
    private var stamps: [String: FsStamp] = [:]
    private var scheduled = false
    private var stopped = false

    init(files: [String: String], dirs: [String: String]) {
        (stream, out) = AsyncStream.makeStream()
        self.files = files
        self.dirs = dirs
        queue.sync {
            tracked = track()
            stamps = tracked.keys.reduce(into: [:]) { acc, p in acc[p] = fsStamp(p) }
            arm()
        }
    }

    func stop() {
        queue.sync {
            stopped = true
            for (s, _) in sources.values { s.cancel() }
            sources = [:]
        }
        out.finish()
    }

    /// Every path a change is reported for → its path under the root: each
    /// watched file, and each entry of each watched directory (not recursive).
    private func track() -> [String: String] {
        var t = files
        for (d, norm) in dirs {
            for name in fsEntryNames(d) {
                t[d + "/" + name] = norm == "." ? name : "\(norm)/\(name)"
            }
        }
        return t
    }

    /// (Re)open a source on every directory and every present tracked file whose
    /// inode is not the one already watched; drop the rest.
    private func arm() {
        var want: [String: FsStamp] = [:]
        for d in Set(files.keys.map { parentPath($0) }).union(dirs.keys) {
            if let s = fsStamp(d) { want[d] = s }
        }
        for (p, s) in stamps where !s.isDir { want[p] = s }
        for (p, (src, ino)) in sources where want[p]?.ino != ino {
            src.cancel()
            sources[p] = nil
        }
        for (p, s) in want where sources[p] == nil {
            let fd = fsEventFd(p)
            if fd < 0 { continue }
            let src = DispatchSource.makeFileSystemObjectSource(
                fileDescriptor: fd,
                eventMask: s.isDir
                    ? [.write, .delete, .rename, .link]
                    : [.write, .extend, .attrib, .delete, .rename],
                queue: queue)
            src.setEventHandler { [weak self] in self?.schedule() }
            src.setCancelHandler { close(fd) }
            src.resume()
            sources[p] = (src, s.ino)
        }
    }

    /// One rescan per burst, `WATCH_DEBOUNCE_MS` after its first event.
    private func schedule() {
        if scheduled || stopped { return }
        scheduled = true
        queue.asyncAfter(deadline: .now() + .milliseconds(WATCH_DEBOUNCE_MS)) { [weak self] in
            self?.rescan()
        }
    }

    private func rescan() {
        scheduled = false
        if stopped { return }
        let now = track()
        var next: [String: FsStamp] = [:]
        var seen = OrderedDictionary<String, String>()
        for p in Set(now.keys).union(tracked.keys).sorted() {
            let rel = now[p] ?? tracked[p]!
            let was = stamps[p]
            let cur = now[p] == nil ? nil : fsStamp(p)
            if let cur { next[p] = cur }
            switch (was, cur) {
            case (nil, .some): seen[rel] = "added"
            case (.some, nil): seen[rel] = "deleted"
            case (let a?, let b?) where a.ino != b.ino: seen[rel] = "added"
            case (let a?, let b?) where !b.isDir && a != b: seen[rel] = "modified"
            default: break
            }
        }
        tracked = now
        stamps = next
        arm()
        if !seen.isEmpty { out.yield(seen.map { ($0.key, $0.value) }) }
    }
}
