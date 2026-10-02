// `fs` — the atomic write's `mode` and `lstat` (python `file_bridge.fs`,
// 2026-10-01): a 0600 secret file is created with exactly those bits, never
// readable by others in between; `lstat` reports what a path IS without
// following a symlink.

import FantasticFile
import FantasticJSON
import Foundation
import Testing

@Suite("fs — mode and lstat")
struct FsModeTests {
    private func tmp() -> String {
        let d = FileManager.default.temporaryDirectory
            .appendingPathComponent("fsmode-\(UUID().uuidString.prefix(8))").path
        try? FileManager.default.createDirectory(atPath: d, withIntermediateDirectories: true)
        return d
    }

    @Test func anAtomicWriteWithAModeCreatesExactlyThoseBits() throws {
        let d = tmp()
        defer { try? FileManager.default.removeItem(atPath: d) }
        try fsWriteBytes(d, "s/secret.yaml", Data("k: v\n".utf8), base: d, mode: 0o600)
        let st = try fsLstat(d, "s/secret.yaml", base: d)
        #expect(Int(st.st_mode) & 0o777 == 0o600)
        #expect((st.st_mode & S_IFMT) == S_IFREG)
        #expect(st.st_uid == getuid())
        // a rewrite over a stale tmp keeps the mode (the tmp is unlinked, O_EXCL)
        FileManager.default.createFile(atPath: d + "/s/secret.yaml.tmp", contents: Data("x".utf8))
        chmod(d + "/s/secret.yaml.tmp", 0o644)
        try fsWriteBytes(d, "s/secret.yaml", Data("k: w\n".utf8), base: d, mode: 0o600)
        #expect(Int(try fsLstat(d, "s/secret.yaml", base: d).st_mode) & 0o777 == 0o600)
        #expect(try fsReadText(d, "s/secret.yaml", base: d) == "k: w\n")
        #expect(!FileManager.default.fileExists(atPath: d + "/s/secret.yaml.tmp"))
    }

    @Test func lstatDoesNotFollowASymlinkAndAMissingEntryIsTheOSError() throws {
        let d = tmp()
        defer { try? FileManager.default.removeItem(atPath: d) }
        try fsWriteBytes(d, "real", Data("x".utf8), base: d)
        symlink(d + "/real", d + "/link")
        let st = try fsLstat(d, "link", base: d)
        #expect((st.st_mode & S_IFMT) == S_IFLNK)
        do {
            _ = try fsLstat(d, "absent", base: d)
            Issue.record("a missing entry must raise")
        } catch {
            #expect(error.message.hasPrefix("[Errno 2] No such file or directory: "), "\(error)")
        }
        #expect(throws: FsError.self) { try fsLstat(d, "../escape", base: d) }
    }
}
