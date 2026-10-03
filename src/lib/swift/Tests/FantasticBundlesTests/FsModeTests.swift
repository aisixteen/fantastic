// `fs` — the atomic write's `mode` and `read_private` (python `file_bridge.fs`,
// 2026-10-01 / audit F5 2026-10-03): a 0600 secret file is created with exactly
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
        // a rewrite over a stale tmp keeps the mode (the tmp is unlinked, O_EXCL)
        FileManager.default.createFile(atPath: d + "/s/secret.yaml.tmp", contents: Data("x".utf8))
        chmod(d + "/s/secret.yaml.tmp", 0o644)
        try fsWriteBytes(d, "s/secret.yaml", Data("k: w\n".utf8), base: d, mode: 0o600)
        #expect(Int(own(d + "/s/secret.yaml").st_mode) & 0o777 == 0o600)
        #expect(try fsReadText(d, "s/secret.yaml", base: d) == "k: w\n")
        #expect(!FileManager.default.fileExists(atPath: d + "/s/secret.yaml.tmp"))
    }

    /// F5 (audit 2026-10-03): `fsReadPrivate` opens ONCE (`O_NOFOLLOW`) and
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
