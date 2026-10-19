// The fantastic home, and the one rule about it: a kernel never keeps its own
// state there — python `kernel/_home.py`, rust `bootstrap::refuse_home_as_state_dir`.
//
// The home is `FANTASTIC_HOME`, else `~/.fantastic`. It holds what the user keeps
// across every fantastic (`user/`, `fantastics/`, `logs/`, the app's `app/`), so a
// kernel booted in `~` — whose `.fantastic` IS the home — is refused. Paths are
// compared absolutized and folded (`.`/`..`), with no filesystem access.

import Foundation

/// The variable naming the fantastic home.
public let FANTASTIC_HOME_VAR = "FANTASTIC_HOME"

/// A disk bootstrap asked to keep the kernel's state in the fantastic home.
public struct HomeAsStateDir: Error, CustomStringConvertible, Sendable {
    /// The workdir the kernel was booted in.
    public let dir: String
    /// The fantastic home.
    public let home: String

    public var description: String {
        "\(dir): its .fantastic is the fantastic home (\(home)) — run fantastic in a project folder"
    }
}

/// `p` made absolute against `cwd`, `.`/`..` folded (python `os.path.abspath`).
public func foldPath(_ p: String, cwd: String = FileManager.default.currentDirectoryPath) -> String {
    let joined = p.hasPrefix("/") ? p : cwd + "/" + p
    var out: [Substring] = []
    for c in joined.split(separator: "/", omittingEmptySubsequences: true) {
        if c == "." { continue }
        if c == ".." {
            if !out.isEmpty { out.removeLast() }
            continue
        }
        out.append(c)
    }
    return "/" + out.joined(separator: "/")
}

/// The home as a path is written: `FANTASTIC_HOME` made absolute, else
/// `~/.fantastic` (the `~` kept, for a reader).
public func fantasticHomeSpelled(
    environment: [String: String] = ProcessInfo.processInfo.environment
) -> String {
    if let h = environment[FANTASTIC_HOME_VAR], !h.isEmpty { return foldPath(h) }
    return "~/.fantastic"
}

/// The home dir, `~` expanded; `nil` when neither `FANTASTIC_HOME` nor `HOME`
/// is set.
public func fantasticHome(
    environment: [String: String] = ProcessInfo.processInfo.environment
) -> String? {
    if let h = environment[FANTASTIC_HOME_VAR], !h.isEmpty { return foldPath(h) }
    guard let u = environment["HOME"], !u.isEmpty else { return nil }
    return foldPath(u + "/.fantastic")
}

/// THE GUARD: throw `HomeAsStateDir` when `stateDir` is `home`.
public func refuseHomeAsStateDir(workdir: String, stateDir: String, home: String?) throws {
    guard let home else { return }
    if foldPath(stateDir) == foldPath(home) {
        throw HomeAsStateDir(dir: foldPath(workdir), home: home)
    }
}
