// Python path resolution — CPython `posixpath.realpath`, the ONE copy.
//
// Python's `fs` clamp compares a path against its root through
// `os.path.realpath`, and so does every swift clamp: `fs` (`FantasticFile`) for
// a path a caller names, and the cold tree read (`Persistence`, in
// `FantasticKernel`, which cannot depend on a bundle). Both depend on this
// module, so the resolution lives here once.
//
// Foundation's `standardizedFileURL` folds `..` lexically instead, which admits
// `link/..` escapes, and `resolvingSymlinksInPath` drops `/private`.

import Foundation

/// CPython `posixpath.realpath(path, strict=False)`: walk the components,
/// resolving every symlink, applying `..` to what is already resolved and
/// keeping a component that does not exist. A symlink loop stops resolving and
/// the rest is kept as written (python raises `RuntimeError` one frame up; the
/// partial path it built is what this returns).
///
/// A RELATIVE result joins the cwd (`posixpath.abspath`); a cwd that cannot be
/// read throws the `OSError` python's `os.getcwd` raises.
public func pyRealpath(_ path: String) throws(PyError) -> String {
    var seen: [String: String?] = [:]
    let (resolved, _) = joinRealpath("", path, &seen)
    if resolved.hasPrefix("/") { return resolved }
    guard let cwd = getcwd(nil, 0) else { throw pyOSError(errno) }
    defer { free(cwd) }
    return pyNormpath(String(cString: cwd) + "/" + resolved)
}

/// `posixpath.split` — the head keeps a lone `/`, otherwise trailing slashes go.
public func pySplit(_ p: String) -> (head: String, tail: String) {
    guard let slash = p.lastIndex(of: "/") else { return ("", p) }
    var head = String(p[p.startIndex...slash])
    let tail = String(p[p.index(after: slash)...])
    if !head.isEmpty && head != String(repeating: "/", count: head.count) {
        while head.hasSuffix("/") { head.removeLast() }
    }
    return (head, tail)
}

private func joinRealpath(
    _ start: String, _ rest: String, _ seen: inout [String: String?]
) -> (String, Bool) {
    var path = start
    var rest = Substring(rest)
    if rest.hasPrefix("/") {
        rest = rest.dropFirst()
        path = "/"
    }
    while !rest.isEmpty {
        let name: Substring
        if let slash = rest.firstIndex(of: "/") {
            name = rest[rest.startIndex..<slash]
            rest = rest[rest.index(after: slash)...]
        } else {
            name = rest
            rest = rest[rest.endIndex...]
        }
        if name.isEmpty || name == "." { continue }
        if name == ".." {
            if !path.isEmpty {
                let (head, tail) = pySplit(path)
                path = tail == ".." ? pyJoin(pyJoin(path, ".."), "..") : head
            } else {
                path = ".."
            }
            continue
        }
        let newPath = pyJoin(path, String(name))
        var st = stat()
        let isLink = lstat(newPath, &st) == 0 && (st.st_mode & S_IFMT) == S_IFLNK
        if !isLink {
            path = newPath
            continue
        }
        if let cached = seen[newPath] {
            if let resolved = cached {
                path = resolved
                continue
            }
            // A symlink loop: the already-resolved part + the rest, unchanged.
            return (pyJoin(newPath, String(rest)), false)
        }
        seen[newPath] = String?.none
        let (joined, ok) = joinRealpath(path, readLink(newPath), &seen)
        if !ok { return (pyJoin(joined, String(rest)), false) }
        path = joined
        seen[newPath] = joined
    }
    return (path, true)
}

/// `posixpath.join` of exactly two components.
private func pyJoin(_ a: String, _ b: String) -> String {
    if b.hasPrefix("/") { return b }
    if a.isEmpty || a.hasSuffix("/") { return a + b }
    return a + "/" + b
}

/// `posixpath.normpath` — enough of it for an absolute path: fold `.`, `..` and
/// repeated slashes.
private func pyNormpath(_ p: String) -> String {
    let absolute = p.hasPrefix("/")
    var parts: [Substring] = []
    for part in p.split(separator: "/") {
        if part == "." { continue }
        if part == ".." {
            if let last = parts.last, last != ".." {
                parts.removeLast()
            } else if !absolute {
                parts.append(part)
            }
            continue
        }
        parts.append(part)
    }
    let joined = parts.joined(separator: "/")
    if absolute { return "/" + joined }
    return joined.isEmpty ? "." : joined
}

private func readLink(_ path: String) -> String {
    var buf = [CChar](repeating: 0, count: Int(PATH_MAX) + 1)
    let n = readlink(path, &buf, buf.count - 1)
    guard n >= 0 else { return "" }
    buf[n] = 0
    return String(cString: buf)
}
