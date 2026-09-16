// Stdlib-only `.env` autoloader. Existing environment entries win.
//
// Port of python `kernel/_env.py:_load_dotenv`, which every CLI mode runs as
// its FIRST step (`main.py:89-91`) — before the kernel is built, before argv is
// dispatched — so a `.env` beside the project seeds the process environment a
// connector later reads. Returns the count of vars actually set; the caller
// prints `[kernel] loaded N var(s) from .env` on stderr when that count is > 0.
//
// Accepted format, python's exactly:
//   - `KEY=value`, one per line
//   - blank lines and `#` comment lines skipped
//   - leading `export ` tolerated (so the same file works in bash)
//   - matching surrounding quotes stripped from the value (`"..."` / `'...'`)
//   - no variable expansion, no multiline values, no escapes
//
// An unreadable or non-UTF-8 file THROWS, exactly as python's
// `Path.read_text(encoding="utf-8")` raises out of an uncaught `main_dispatch`.
// There is no "treat it as empty" branch — a `.env` that cannot be read is a
// broken precondition, not an absent one.

import FantasticJSON
import FantasticKernel
import Foundation

enum DotEnvError: Error, CustomStringConvertible {
    case unreadable(String)

    var description: String {
        switch self {
        case .unreadable(let path): return "cannot read \(path) as utf-8"
        }
    }
}

/// Load `KEY=value` pairs from `path` into the process environment.
///
/// Existing entries are NEVER overwritten — the shell wins, so an
/// `ANTHROPIC_API_KEY` exported in the parent shell beats whatever `.env` says.
/// Silent no-op when the file is absent (python's `path.exists()` guard).
@discardableResult
func loadDotEnv(path: String = ".env") throws -> Int {
    guard FileManager.default.fileExists(atPath: path) else { return 0 }
    guard let data = FileManager.default.contents(atPath: path),
        let text = String(data: data, encoding: .utf8)
    else {
        throw DotEnvError.unreadable(path)
    }
    var count = 0
    for raw in pySplitlines(text) {
        var line = pyStrip(raw)
        if line.isEmpty || line.hasPrefix("#") { continue }
        if line.hasPrefix("export ") {
            line = pyLStrip(String(line.dropFirst("export ".count)))
        }
        guard let eq = line.firstIndex(of: "=") else { continue }
        let key = pyStrip(String(line[..<eq]))
        var val = pyStrip(String(line[line.index(after: eq)...]))
        if key.isEmpty { continue }
        if val.count >= 2, let f = val.first, let l = val.last, f == l, f == "'" || f == "\"" {
            val = String(val.dropFirst().dropLast())
        }
        if getenv(key) == nil {
            setenv(key, val, 0)
            count += 1
        }
    }
    return count
}

/// Python's `str.lstrip()` — the same character class `pyStrip` uses, leading
/// end only.
func pyLStrip(_ s: String) -> String {
    var out = Substring(s)
    while let f = out.first, f.isWhitespace || ("\u{1c}"..."\u{1f}").contains(f) {
        out = out.dropFirst()
    }
    return String(out)
}
