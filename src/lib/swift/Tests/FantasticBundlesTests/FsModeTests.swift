// `fs` — the atomic write's `mode` and `read_private` (python `file_bridge.fs`):
// a 0600 secret file is created with exactly
// those bits, never readable by others in between; a private file is read
// through ONE open whose descriptor is what is judged.

import FantasticFile
import FantasticJSON
import Foundation
import Testing

@Suite("fs — mode and read_private")
struct FsModeTests {
    private func tmp() -> String {
        let d = FileManager.default.temporaryDirectory
            .appendingPathComponent("fsmode-\(UUID().uuidString.prefix(8))").path
        try? FileManager.default.createDirectory(atPath: d, withIntermediateDirectories: true)
        return d
    }

    /// The entry's own stat, symlinks not followed — the test's eye, not `fs`'s.
    private func own(_ path: String) -> stat {
        var st = stat()
        _ = lstat(path, &st)
        return st
    }

    @Test func anAtomicWriteWithAModeCreatesExactlyThoseBits() throws {
        let d = tmp()
        defer { try? FileManager.default.removeItem(atPath: d) }
        try fsWriteBytes(d, "s/secret.yaml", Data("k: v\n".utf8), base: d, mode: 0o600)
        let st = own(d + "/s/secret.yaml")
        #expect(Int(st.st_mode) & 0o777 == 0o600)
        #expect((st.st_mode & S_IFMT) == S_IFREG)
        #expect(st.st_uid == getuid())
        // a rewrite keeps the mode, and leaves no tmp behind
        try fsWriteBytes(d, "s/secret.yaml", Data("k: w\n".utf8), base: d, mode: 0o600)
        #expect(Int(own(d + "/s/secret.yaml").st_mode) & 0o777 == 0o600)
        #expect(try fsReadText(d, "s/secret.yaml", base: d) == "k: w\n")
        #expect(try FileManager.default.contentsOfDirectory(atPath: d + "/s") == ["secret.yaml"])
    }

    /// A symlink at `<name>.tmp` pointing outside the root is never written
    /// through: each write stages in a fresh, exclusively created tmp.
    @Test func anAtomicWriteNeverFollowsAPlantedTmpLink() throws {
        let d = tmp()
        defer { try? FileManager.default.removeItem(atPath: d) }
        let outside = d + "/outside.txt"
        FileManager.default.createFile(atPath: outside, contents: Data("keep".utf8))
        let root = d + "/root"
        try FileManager.default.createDirectory(atPath: root, withIntermediateDirectories: true)
        symlink(outside, root + "/f.txt.tmp")
        for mode: mode_t? in [nil, 0o600] {
            try fsWriteBytes(root, "f.txt", Data("new".utf8), base: d, mode: mode)
            #expect(try String(contentsOfFile: outside, encoding: .utf8) == "keep")
            #expect(try fsReadText(root, "f.txt", base: d) == "new")
        }
    }

    /// `fsReadPrivate` opens ONCE (`O_NOFOLLOW`) and
    /// judges the open descriptor — regular, this user's, no group/other bits —
    /// before reading it, so no swap can land between the check and the read. A
    /// symlink in the leaf's place, a widened mode and a FIFO are refused (the
    /// error's `notPrivate`, nothing read); an absent file is the OSError.
    @Test func readPrivateJudgesTheOpenDescriptorItReads() throws {
        let d = tmp()
        defer { try? FileManager.default.removeItem(atPath: d) }
        let real = try pyRealpath(d)
        let secret = d + "/s.yaml"
        FileManager.default.createFile(atPath: secret, contents: Data("k: v\n".utf8))
        chmod(secret, 0o600)
        #expect(try fsReadPrivate(d, "s.yaml", base: d) == Data("k: v\n".utf8))
        chmod(secret, 0o644)
        do {
            _ = try fsReadPrivate(d, "s.yaml", base: d)
            Issue.record("a widened mode must refuse")
        } catch {
            #expect(error.notPrivate == FsNotPrivate(mode: 0o644, uid: getuid()), "\(error)")
            #expect(
                error.message
                    == "fs: \(real)/s.yaml is not a private file (mode 0o644, uid "
                    + "\(getuid())) — refused, nothing read")
        }
        chmod(secret, 0o600)
        symlink(secret, d + "/link.yaml")
        do {
            _ = try fsReadPrivate(d, "link.yaml", base: d)
            Issue.record("a symlink in the leaf's place must refuse")
        } catch {
            #expect(error.notPrivate != nil, "\(error)")
        }
        mkfifo(d + "/pipe", 0o600)
        do {  // judged, never a hang on the open
            _ = try fsReadPrivate(d, "pipe", base: d)
            Issue.record("a FIFO must refuse")
        } catch {
            #expect(error.notPrivate != nil, "\(error)")
        }
        do {
            _ = try fsReadPrivate(d, "absent.yaml", base: d)
            Issue.record("an absent file must raise")
        } catch {
            #expect(error.notPrivate == nil)
            #expect(error.message.hasPrefix("[Errno 2] No such file or directory: "), "\(error)")
        }
    }
}
