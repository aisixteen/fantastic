// B4.5 + B4.9 — every provider backend's readme is python's `readme.md`, byte
// for byte. It is served from the bundle by `reflect readme=true`, so a swift-written summary was a second, shorter truth
// about the same bundle. (The ollama file's double-encoded lines are python's
// bytes too, and they are copied as they stand.)

import Foundation
import Testing

@testable import FantasticOllamaBackend

/// A file under python's `bundled_agents/ai/`, located from this source file.
private func pythonAISource(_ relative: String) -> URL {
    URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent()  // FantasticAIBackendsTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // swift
        .deletingLastPathComponent()  // lib
        .appendingPathComponent("python/bundled_agents/ai")
        .appendingPathComponent(relative)
}

private func pythonReadme(_ relative: String) throws -> String {
    try String(contentsOf: pythonAISource(relative), encoding: .utf8)
}

@Suite("Backend readmes")
struct BackendReadmeTests {

    @Test func ollamaReadmeIsPythonVerbatim() throws {
        let expected = try pythonReadme("ollama/ollama_backend/src/ollama_backend/readme.md")
        #expect(OllamaBackendBundle().readme == expected)
    }
}
