// Cross-platform line streaming over an async byte sequence.
//
// The LLM backends consume streaming HTTP bodies (ollama NDJSON, NIM SSE)
// line by line. URLSession's `.bytes(for:).lines` is Apple-only, so the
// backends now use AsyncHTTPClient (NIO, macOS + Linux) whose response body
// is an `AsyncSequence<ByteBuffer>`. This helper splits that byte stream into
// UTF-8 lines (on `\n`), the same surface the providers had before.

import Foundation
import NIOCore

/// Turn an async sequence of `ByteBuffer` chunks (e.g. an AsyncHTTPClient
/// response body) into a stream of UTF-8 lines, split on `\n`. The trailing
/// newline is stripped; a final line without a newline is still yielded.
/// Splitting on the raw newline BYTE (not decoded text) avoids dropping a
/// multi-byte UTF-8 character that straddles two chunks.
public func bytesToLines<S: AsyncSequence & Sendable>(
    _ body: S
) -> AsyncThrowingStream<String, Error> where S.Element == ByteBuffer {
    AsyncThrowingStream { continuation in
        let task = Task {
            var acc: [UInt8] = []
            do {
                for try await chunk in body {
                    acc.append(contentsOf: chunk.readableBytesView)
                    while let nl = acc.firstIndex(of: 0x0A) {
                        let lineBytes = Array(acc[..<nl])
                        acc.removeSubrange(...nl)
                        if let line = String(bytes: lineBytes, encoding: .utf8) {
                            continuation.yield(line)
                        }
                    }
                }
                if !acc.isEmpty, let line = String(bytes: acc, encoding: .utf8) {
                    continuation.yield(line)
                }
                continuation.finish()
            } catch {
                continuation.finish(throwing: error)
            }
        }
        continuation.onTermination = { _ in task.cancel() }
    }
}
