// fs — THE single clamped disk surface, python `file_bridge.fs`, function for
// function.
//
// The running-dir law lives here: a `root` is clamped to the running dir, a
// `path` is clamped to that root (`../`, `~`, absolute paths and outward
// symlinks all refuse), and every read or write funnels through one of these
// calls. The clamp is unbypassable because there is no other disk code to go
// around to — `FileBundle` is the GATE and the verb shape over this, and the
// cold bring-up readers (`static_loader`'s manifest) come through here too.
//
// Every failure carries the text PYTHON writes for it: a clamp violation is the
// `ValueError` string (`fs: path 'x' escapes root`), a disk failure the
// `str(OSError)` (`[Errno 2] No such file or directory: '/abs/x'`), because both
// end up verbatim inside a refusal a caller reads.
//
// Path resolution is python's `Path.resolve(strict=False)` — `os.path.realpath`:
// symlinks followed component by component, `..` applied to the RESOLVED prefix,
// a missing tail kept as written. Foundation's `standardizedFileURL` folds `..`
// lexically instead, which admits `link/..` escapes, and
// `resolvingSymlinksInPath` drops `/private`.

import FantasticJSON
import Foundation

/// A python `fs` failure, carrying the exact text python's exception prints:
/// the `ValueError` of a clamp violation or the `str(OSError)` of a disk error.
public struct FsError: Error, CustomStringConvertible, Equatable, Sendable {
    public let message: String
    public init(_ message: String) { self.message = message }
    public var description: String { message }
}

// MARK: - resolution (the clamp)

/// python `fs.resolve_root(root)` — clamp `root` to the running dir (`base`,
/// python's `Path.cwd()`). A relative root resolves under it; an absolute root
/// is allowed only inside it; `~`/`..` escapes refuse. Returns the resolved
/// absolute path.
public func fsResolveRoot(_ root: String, base: String) throws(FsError) -> String {
    let b = pyRealpath(base)
    let p = pyRealpath(root.hasPrefix("/") ? root : b + "/" + root)
    guard pathIsUnder(p, b) else {
        throw FsError("fs: root \(pyRepr(root)) escapes the running dir")
    }
    return p
}

/// python `fs.resolve(root, path)` — clamp `path` within `root` (itself clamped
/// to `base`). An absolute path is a hard escape, never silently re-rooted; a
/// `..` or an outward symlink escapes too. Returns the resolved absolute path.
public func fsResolve(_ root: String, _ path: String, base: String) throws(FsError) -> String {
    let rootPath = try fsResolveRoot(root, base: base)
    if path.hasPrefix("/") { throw FsError("fs: path \(pyRepr(path)) escapes root") }
    let target = path.isEmpty ? rootPath : pyRealpath(rootPath + "/" + path)
    guard pathIsUnder(target, rootPath) else {
        throw FsError("fs: path \(pyRepr(path)) escapes root")
    }
    return target
}

/// python `fs._lexical_leaf` — the UNRESOLVED leaf under a resolved parent: the
/// path a symlink itself lives at. `fsResolve` follows the link to its target,
/// so the leaf must be probed before the follow; the parent is still clamped.
func fsLexicalLeaf(_ root: String, _ path: String, base: String) throws(FsError) -> String {
    let cut = path.lastIndex(of: "/")
    let parentRel = cut.map { String(path[path.startIndex..<$0]) } ?? ""
    let name = cut.map { String(path[path.index(after: $0)...]) } ?? path
    let parent = try fsResolve(root, parentRel, base: base)
    // pathlib's join drops a `.` component and keeps `..`.
    if name.isEmpty || name == "." { return parent }
    return parent == "/" ? "/" + name : parent + "/" + name
}

/// python `pathlib.Path.relative_to` as the clamp uses it: `p` is `r` or lies
/// under it, compared by PATH COMPONENT (so `/a/bc` is not under `/a/b`).
private func pathIsUnder(_ p: String, _ r: String) -> Bool {
    if p == r { return true }
    if r == "/" { return p.hasPrefix("/") }
    return p.hasPrefix(r + "/")
}

/// CPython `posixpath.realpath(path, strict=False)`: walk the components,
/// resolving every symlink, applying `..` to what is already resolved and
/// keeping a component that does not exist. A symlink loop stops resolving and
/// the rest is kept as written (python raises `RuntimeError` one frame up; the
/// partial path it built is what this returns).
public func pyRealpath(_ path: String) -> String {
    var seen: [String: String?] = [:]
    let (resolved, _) = joinRealpath("", path, &seen)
    // posixpath.abspath: a relative result joins the cwd (python's `os.getcwd`).
    if resolved.hasPrefix("/") { return resolved }
    let cwd = FileManager.default.currentDirectoryPath
    return pyNormpath(cwd + "/" + resolved)
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

/// `posixpath.split` — the head keeps a lone `/`, otherwise trailing slashes go.
private func pySplit(_ p: String) -> (head: String, tail: String) {
    guard let slash = p.lastIndex(of: "/") else { return ("", p) }
    var head = String(p[p.startIndex...slash])
    let tail = String(p[p.index(after: slash)...])
    if !head.isEmpty && head != String(repeating: "/", count: head.count) {
        while head.hasSuffix("/") { head.removeLast() }
    }
    return (head, tail)
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

// MARK: - existence / stat

/// `os.stat`, or nil when python's `Path.exists()`-family would answer False
/// (ENOENT/ENOTDIR/EBADF/ELOOP); any other errno is the `OSError` python raises.
private func statOrNil(_ path: String, follow: Bool = true) throws(FsError) -> stat? {
    var st = stat()
    let rc = follow ? stat(path, &st) : lstat(path, &st)
    if rc == 0 { return st }
    let e = errno
    if e == ENOENT || e == ENOTDIR || e == EBADF || e == ELOOP { return nil }
    throw FsError(osErrorStr(e, path))
}

public func fsExists(_ root: String, _ path: String, base: String) throws(FsError) -> Bool {
    try statOrNil(try fsResolve(root, path, base: base)) != nil
}

public func fsIsFile(_ root: String, _ path: String, base: String) throws(FsError) -> Bool {
    guard let st = try statOrNil(try fsResolve(root, path, base: base)) else { return false }
    return (st.st_mode & S_IFMT) == S_IFREG
}

public func fsIsDir(_ root: String, _ path: String, base: String) throws(FsError) -> Bool {
    guard let st = try statOrNil(try fsResolve(root, path, base: base)) else { return false }
    return (st.st_mode & S_IFMT) == S_IFDIR
}

/// python `fs.is_symlink` — true when `path` ITSELF is a symlink, dangling or
/// not (`exists()` follows links, so a dangling one reports absent while still
/// occupying the name).
public func fsIsSymlink(_ root: String, _ path: String, base: String) throws(FsError) -> Bool {
    let leaf = try fsLexicalLeaf(root, path, base: base)
    guard let st = try statOrNil(leaf, follow: false) else { return false }
    return (st.st_mode & S_IFMT) == S_IFLNK
}

/// `Path.is_dir()` on an already-resolved absolute path.
func pathIsDir(_ path: String) -> Bool {
    var st = stat()
    return stat(path, &st) == 0 && (st.st_mode & S_IFMT) == S_IFDIR
}

/// `Path.stat().st_size` on an already-resolved absolute path, or nil.
func pathSize(_ path: String) -> Int64? {
    var st = stat()
    guard stat(path, &st) == 0 else { return nil }
    return Int64(st.st_size)
}

// MARK: - whole-file read

/// python `fs.read_text` — `Path.read_text(encoding="utf-8")`: the open error is
/// an `OSError`, the decode error a `UnicodeDecodeError` (a `ValueError`), and
/// `static_loader` prints either one verbatim.
public func fsReadText(_ root: String, _ path: String, base: String) throws(FsError) -> String {
    let target = try fsResolve(root, path, base: base)
    let data = try readWhole(target)
    if let bad = pyUtf8DecodeError(data) { throw FsError(bad) }
    return String(decoding: data, as: UTF8.self)
}

/// python `fs.read_bytes` — the whole file.
public func fsReadBytes(_ root: String, _ path: String, base: String) throws(FsError) -> Data {
    try readWhole(try fsResolve(root, path, base: base))
}

/// python `fs.write_bytes` (atomic) — whole-file write through a tmp sibling
/// (`<name>.tmp`) and an atomic rename; parent dirs created.
public func fsWriteBytes(_ root: String, _ path: String, _ data: Data, base: String)
    throws(FsError)
{
    let target = try fsResolve(root, path, base: base)
    try pyMkdirParents(parentPath(target))
    let tmp = target + ".tmp"
    let fd = open(tmp, O_WRONLY | O_CREAT | O_TRUNC, 0o666)
    if fd < 0 { throw FsError(osErrorStr(errno, tmp)) }
    var written = 0
    while written < data.count {
        let n = data.withUnsafeBytes { raw -> Int in
            write(fd, raw.baseAddress!.advanced(by: written), data.count - written)
        }
        if n < 0 {
            let e = errno
            close(fd)
            throw FsError(osErrorStr(e))
        }
        if n == 0 { break }
        written += n
    }
    close(fd)
    if rename(tmp, target) != 0 { throw FsError(osErrorStr(errno, tmp, target)) }
}

private func readWhole(_ target: String) throws(FsError) -> Data {
    let fd = open(target, O_RDONLY)
    if fd < 0 { throw FsError(osErrorStr(errno, target)) }
    defer { close(fd) }
    var st = stat()
    if fstat(fd, &st) != 0 { throw FsError(osErrorStr(errno, target)) }
    if (st.st_mode & S_IFMT) == S_IFDIR { throw FsError(osErrorStr(EISDIR, target)) }
    var out = Data()
    var buf = [UInt8](repeating: 0, count: 65536)
    while true {
        let n = buf.withUnsafeMutableBytes { read(fd, $0.baseAddress, 65536) }
        if n < 0 { throw FsError(osErrorStr(errno)) }
        if n == 0 { break }
        out.append(contentsOf: buf[0..<n])
    }
    return out
}

// MARK: - chunked / streaming (the stream-verb backing)

/// python `fs.read_chunk` — ONE chunk at `offset`, plus the file's total size.
/// The handle opens and closes inside this call (no held stream).
public func fsReadChunk(
    _ root: String, _ path: String, offset: Int64, length: Int64, base: String
) throws(FsError) -> (chunk: Data, size: Int64) {
    let target = try fsResolve(root, path, base: base)
    var st = stat()
    if stat(target, &st) != 0 { throw FsError(osErrorStr(errno, target)) }
    let size = Int64(st.st_size)
    let fd = open(target, O_RDONLY)
    if fd < 0 { throw FsError(osErrorStr(errno, target)) }
    defer { close(fd) }
    // Read only what EXISTS: the request bounds the chunk, the file bounds the
    // request — a caller-chosen `length` never sizes a buffer (python's
    // `f.read(n)` would try to allocate `n`).
    let want = Int(min(max(0, length), max(0, size - offset)))
    if want == 0 { return (Data(), size) }
    var buf = [UInt8](repeating: 0, count: want)
    let n = buf.withUnsafeMutableBytes { pread(fd, $0.baseAddress, want, off_t(offset)) }
    if n < 0 { throw FsError(osErrorStr(errno)) }
    return (Data(buf[0..<n]), size)
}

/// python `fs.write_chunk` — write ONE chunk at `offset` (nil ⇒ append at end).
/// `truncate` (or a first write to a new file) starts fresh. Returns the offset
/// written and the file's new size.
public func fsWriteChunk(
    _ root: String, _ path: String, _ data: Data, offset: Int64?, truncate: Bool, base: String
) throws(FsError) -> (offset: Int64, size: Int64) {
    let target = try fsResolve(root, path, base: base)
    try pyMkdirParents(parentPath(target))
    let fresh = truncate ? true : (try statOrNil(target)) == nil
    if fresh {
        let fd = open(target, O_WRONLY | O_CREAT | O_TRUNC, 0o666)
        if fd < 0 { throw FsError(osErrorStr(errno, target)) }
        close(fd)
    }
    var off = offset ?? 0
    if offset == nil {
        var st = stat()
        if stat(target, &st) != 0 { throw FsError(osErrorStr(errno, target)) }
        off = Int64(st.st_size)
    }
    let fd = open(target, O_RDWR)
    if fd < 0 { throw FsError(osErrorStr(errno, target)) }
    defer { close(fd) }
    var written = 0
    while written < data.count {
        let n = data.withUnsafeBytes { raw -> Int in
            pwrite(
                fd, raw.baseAddress!.advanced(by: written), data.count - written,
                off_t(off) + off_t(written))
        }
        if n < 0 { throw FsError(osErrorStr(errno)) }
        if n == 0 { break }
        written += n
    }
    var st = stat()
    if stat(target, &st) != 0 { throw FsError(osErrorStr(errno, target)) }
    return (off, Int64(st.st_size))
}

// MARK: - directory walk / mutate

/// python `fs.list_dir` — the dir's entries as resolved absolute paths, sorted
/// the way `sorted(Path...)` sorts them (by path string, code point order).
/// Exposure is the AGENT's `allow` list, not a disk concern.
public func fsListDir(_ root: String, _ path: String, base: String) throws(FsError) -> [String] {
    let target = try fsResolve(root, path, base: base)
    return try listNames(target).sorted {
        $0.unicodeScalars.lexicographicallyPrecedes($1.unicodeScalars) { $0.value < $1.value }
    }.map { target == "/" ? "/" + $0 : target + "/" + $0 }
}

/// `os.listdir` — the names in directory order.
private func listNames(_ target: String) throws(FsError) -> [String] {
    guard let dir = opendir(target) else { throw FsError(osErrorStr(errno, target)) }
    defer { closedir(dir) }
    var names: [String] = []
    while let entry = readdir(dir) {
        let name = withUnsafeBytes(of: entry.pointee.d_name) { raw in
            String(cString: raw.baseAddress!.assumingMemoryBound(to: CChar.self))
        }
        if name == "." || name == ".." { continue }
        names.append(name)
    }
    return names
}

public func fsMkdir(_ root: String, _ path: String, base: String) throws(FsError) {
    try pyMkdirParents(try fsResolve(root, path, base: base))
}

public func fsRename(_ root: String, _ old: String, _ new: String, base: String) throws(FsError) {
    let o = try fsResolve(root, old, base: base)
    let n = try fsResolve(root, new, base: base)
    try pyMkdirParents(parentPath(n))
    if rename(o, n) != 0 { throw FsError(osErrorStr(errno, o, n)) }
}

/// python `fs.remove` — a file, an empty dir, or (recursive) a dir tree.
///
/// A symlink is UNLINKED, never followed: the leaf is probed before `fsResolve`
/// follows it, so `delete link` cannot recurse into the real directory and leave
/// the name dangling. A dangling symlink is deletable for the same reason.
public func fsRemove(_ root: String, _ path: String, recursive: Bool, base: String) throws(FsError)
{
    let leaf = try fsLexicalLeaf(root, path, base: base)
    var st = stat()
    if lstat(leaf, &st) == 0 && (st.st_mode & S_IFMT) == S_IFLNK {
        if unlink(leaf) != 0 { throw FsError(osErrorStr(errno, leaf)) }
        return
    }
    let target = try fsResolve(root, path, base: base)
    if pathIsDir(target) {
        if recursive {
            try rmtree(target)
        } else if rmdir(target) != 0 {
            throw FsError(osErrorStr(errno, target))
        }
    } else if unlink(target) != 0 {
        throw FsError(osErrorStr(errno, target))
    }
}

private func rmtree(_ target: String) throws(FsError) {
    for name in try listNames(target) {
        let sub = target == "/" ? "/" + name : target + "/" + name
        var st = stat()
        let isLink = lstat(sub, &st) == 0 && (st.st_mode & S_IFMT) == S_IFLNK
        if pathIsDir(sub) && !isLink {
            try rmtree(sub)
        } else if unlink(sub) != 0 {
            throw FsError(osErrorStr(errno, sub))
        }
    }
    if rmdir(target) != 0 { throw FsError(osErrorStr(errno, target)) }
}

/// python `Path.mkdir(parents=True, exist_ok=True)`, errno and filename for
/// filename: a missing parent is created first, an existing DIRECTORY is fine,
/// an existing file is the `FileExistsError` python raises.
func pyMkdirParents(_ path: String) throws(FsError) {
    if mkdir(path, 0o777) == 0 { return }
    let e = errno
    if e == ENOENT {
        let parent = parentPath(path)
        if parent == path { throw FsError(osErrorStr(e, path)) }
        try pyMkdirParents(parent)
        if mkdir(path, 0o777) == 0 { return }
        let again = errno
        if !pathIsDir(path) { throw FsError(osErrorStr(again, path)) }
        return
    }
    if !pathIsDir(path) { throw FsError(osErrorStr(e, path)) }
}

/// `Path.parent` of an absolute path.
func parentPath(_ path: String) -> String {
    let head = pySplit(path).head
    return head.isEmpty ? path : head
}
