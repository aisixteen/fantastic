// fs — THE single clamped disk surface, python `file_bridge.fs`, function for
// function.
//
// The running-dir law lives here: a `root` is clamped to the running dir, a
// `path` is clamped to that root (`../`, `~`, absolute paths and outward
// symlinks all refuse), and every read or write of a path a caller names funnels
// through one of these calls. The clamp is unbypassable for those paths:
// nothing reads or writes one outside its allowed root by going around this
// file — `FileBundle` is the GATE and the verb shape over this, and the cold
// bring-up readers (`static_loader`'s manifest) come through here too, as does
// `credential_store` (its `file` backend — secrets at an absolute path OUTSIDE
// the running dir by design, written 0600, read through `fsReadPrivate`).
//
// The ONLY disk code outside this file (none of it opens a caller-named path):
//   - the substrate module `FantasticKernel`, which cannot depend on a bundle:
//     `WorkdirLock` (`lock.yaml`), `Persistence` (`readRecord`, and the cold
//     tree read — `readTree` / `walkRecords` / `readRecordClamped`, clamped as
//     this file clamps);
//   - `FantasticKernelStartup`: `startKernel` reads the root record
//     (`readRecordClamped`) and, under the lock (`Kernel.takeSeed`), creates
//     `.fantastic` and writes a fresh root's record; `daemon` checks the workdir
//     is a directory;
//   - the `Fantastic` CLI target's `.env` read (`loadDotEnv`);
//   - `KernelStateBundle`'s `fileExists` probes of clamped paths (a forgotten
//     record's dir, a `reload_bundle` folder) before it writes, removes or
//     loads through here — its folder bundles (`Folders`) read through here;
//   - `FantasticExecBridge`'s process plumbing: `Spawn` probes the child's
//     `PATH` dirs for a bare command (`fileExists` + `access` — metadata only;
//     the OS opens what it execs), `PtyJob.pasteDirectory` creates a PTY job's
//     paste scratch dir (`mkdtemp` in `NSTemporaryDirectory()`) — the image
//     itself is written through here;
//   - `Bundle.module` reads of a target's OWN baked-in resources (the root
//     readme and docs text, a backend's readme, the web favicon).
//
// Every failure carries the text PYTHON writes for it: a clamp violation is the
// `ValueError` string (`fs: path 'x' escapes root`), a disk failure the
// `str(OSError)` (`[Errno 2] No such file or directory: '/abs/x'`), because both
// end up verbatim inside a refusal a caller reads.
//
// Path resolution is python's `Path.resolve(strict=False)` — `os.path.realpath`:
// symlinks followed component by component, `..` applied to the RESOLVED prefix,
// a missing tail kept as written (`pyRealpath`, shared with the cold tree read).

import FantasticJSON
import Foundation

/// A python `fs` failure, carrying the exact text python's exception prints:
/// the `ValueError` of a clamp violation or the `str(OSError)` of a disk error.
public struct FsError: Error, CustomStringConvertible, Equatable, Sendable {
    public let message: String
    /// Set when `fsReadPrivate` refused the entry as not private — python's
    /// `fs.NotPrivate` (a `PermissionError`): what the entry IS, for the
    /// caller's own refusal text. Nil for every other failure.
    public let notPrivate: FsNotPrivate?
    public init(_ message: String, notPrivate: FsNotPrivate? = nil) {
        self.message = message
        self.notPrivate = notPrivate
    }
    public var description: String { message }
}

/// What `fsReadPrivate` found instead of a private file: its permission bits and
/// its owner. Python `NotPrivate.mode` / `.uid`.
public struct FsNotPrivate: Equatable, Sendable {
    public let mode: Int
    public let uid: uid_t
    public init(mode: Int, uid: uid_t) {
        self.mode = mode
        self.uid = uid
    }
}

// MARK: - resolution (the clamp)

/// python `fs.resolve_root(root)` — clamp `root` to the running dir (`base`,
/// python's `Path.cwd()`). A relative root resolves under it; an absolute root
/// is allowed only inside it; `~`/`..` escapes refuse. Returns the resolved
/// absolute path.
public func fsResolveRoot(_ root: String, base: String) throws(FsError) -> String {
    let b = try fsRealpath(base)
    let p = try fsRealpath(root.hasPrefix("/") ? root : b + "/" + root)
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
    let target = try path.isEmpty ? rootPath : fsRealpath(rootPath + "/" + path)
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

/// `pyRealpath` (`FantasticJSON`, the one copy) under `fs`'s error: a cwd that
/// cannot be read is the `str(OSError)` python's `os.getcwd` raises.
private func fsRealpath(_ path: String) throws(FsError) -> String {
    do {
        return try pyRealpath(path)
    } catch {
        throw FsError(error.message)
    }
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

/// python `fs.read_private` — read a PRIVATE file (a regular file owned by this
/// user with no group/other permission bits) through ONE open. `O_NOFOLLOW`
/// refuses a symlink in the leaf's place, and the OPEN DESCRIPTOR is what is
/// judged (`fstat`) and read, so nothing can swap the file between the check and
/// the read (audit F5, 2026-10-03: an `lstat` of the path and then a read of the
/// path again was a window). `O_NONBLOCK` so a FIFO planted there cannot hang the
/// open — it is judged and refused like any other non-regular entry. Throws an
/// `FsError` carrying `notPrivate` (nothing read) when the entry is not private;
/// the `OSError` text (`[Errno 2] …`) when absent.
public func fsReadPrivate(_ root: String, _ path: String, base: String) throws(FsError) -> Data {
    let leaf = try fsLexicalLeaf(root, path, base: base)
    func refuse(_ mode: Int, _ uid: uid_t) -> FsError {
        FsError(
            "fs: \(leaf) is not a private file (mode 0o\(String(mode, radix: 8)), uid \(uid)) "
                + "— refused, nothing read",
            notPrivate: FsNotPrivate(mode: mode, uid: uid))
    }
    let fd = open(leaf, O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC)
    if fd < 0 {
        let e = errno
        guard e == ELOOP else { throw FsError(osErrorStr(e, leaf)) }
        // the symlink itself, named in the refusal — never read
        var st = stat()
        if lstat(leaf, &st) != 0 { throw FsError(osErrorStr(errno, leaf)) }
        throw refuse(Int(st.st_mode) & 0o7777, st.st_uid)
    }
    defer { close(fd) }
    var st = stat()
    if fstat(fd, &st) != 0 { throw FsError(osErrorStr(errno, leaf)) }
    let mode = Int(st.st_mode) & 0o7777
    if (st.st_mode & S_IFMT) != S_IFREG || st.st_uid != getuid() || mode & 0o077 != 0 {
        throw refuse(mode, st.st_uid)
    }
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

/// python `fs.read_bytes` — the whole file.
public func fsReadBytes(_ root: String, _ path: String, base: String) throws(FsError) -> Data {
    try readWhole(try fsResolve(root, path, base: base))
}

/// python `secrets.token_hex(n)` — `n` CSPRNG bytes as lowercase hex.
private func tokenHex(_ n: Int) -> String {
    // `SystemRandomNumberGenerator` is the OS CSPRNG
    (0..<n).map { _ -> String in
        let h = String(UInt8.random(in: 0...255), radix: 16)
        return h.count < 2 ? "0" + h : h
    }.joined()
}

/// python `fs.write_bytes` (atomic) — whole-file write through a fresh tmp
/// sibling (`<name>.tmp.<hex>`) and an atomic rename; parent dirs created.
///
/// `mode` creates the file with exactly those permission bits — a 0600 secret
/// file is never readable by others, not even between create and chmod: the tmp
/// is unlinked, created `O_EXCL` with the mode, then `fchmod`ed (the umask may
/// have narrowed it; never widened). Python's `mode=`.
public func fsWriteBytes(
    _ root: String, _ path: String, _ data: Data, base: String, mode: mode_t? = nil
) throws(FsError) {
    let target = try fsResolve(root, path, base: base)
    try pyMkdirParents(parentPath(target))
    // A FRESH tmp per write, created exclusively and never through a link: a
    // fixed `<name>.tmp` followed whatever symlink sat at that name (a write
    // escaped the root) and was shared by concurrent writers (audit 2026-10-14).
    let tmp = target + ".tmp." + tokenHex(4)
    let fd = open(tmp, O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW, mode ?? 0o666)
    if fd < 0 { throw FsError(osErrorStr(errno, tmp)) }
    let fail = { (e: Int32) -> FsError in
        close(fd)
        unlink(tmp)
        return FsError(osErrorStr(e))
    }
    if let mode, fchmod(fd, mode) != 0 { throw fail(errno) }
    var written = 0
    while written < data.count {
        let n = data.withUnsafeBytes { raw -> Int in
            write(fd, raw.baseAddress!.advanced(by: written), data.count - written)
        }
        if n < 0 { throw fail(errno) }
        if n == 0 { break }
        written += n
    }
    close(fd)
    if rename(tmp, target) != 0 {
        let e = errno
        unlink(tmp)
        throw FsError(osErrorStr(e, tmp, target))
    }
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

// MARK: - watching (event-only)
//
// `watch_path`'s disk reach — on absolute paths the clamp ALREADY resolved
// (`fsResolve`), and none of it reads or writes a byte: an event-only descriptor a
// vnode source listens on, a stat stamp, a directory's entry names.

/// What a watcher compares between two looks at one path.
struct FsStamp: Equatable {
    let ino: UInt64
    let isDir: Bool
    let mtimeNs: Int64
    let size: Int64
}

/// An EVENT-ONLY descriptor (`O_EVTONLY` — it can read nothing) on a resolved
/// absolute path; -1 when absent.
func fsEventFd(_ abs: String) -> Int32 {
    open(abs, O_EVTONLY)
}

/// The stamp of a resolved absolute path, following links; nil when absent.
func fsStamp(_ abs: String) -> FsStamp? {
    var st = stat()
    guard stat(abs, &st) == 0 else { return nil }
    return FsStamp(
        ino: UInt64(st.st_ino), isDir: (st.st_mode & S_IFMT) == S_IFDIR,
        mtimeNs: Int64(st.st_mtimespec.tv_sec) * 1_000_000_000 + Int64(st.st_mtimespec.tv_nsec),
        size: Int64(st.st_size))
}

/// The entry names of a resolved absolute directory; [] when absent.
func fsEntryNames(_ abs: String) -> [String] {
    (try? FileManager.default.contentsOfDirectory(atPath: abs)) ?? []
}
