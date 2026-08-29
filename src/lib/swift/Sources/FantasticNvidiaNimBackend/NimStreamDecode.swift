// NIM Server-Sent-Events line → text pieces. The wire decode, as a PURE
// function.
//
// Mirrors the body of Python
// `nvidia_nim_backend.provider.NvidiaNimProvider.chat`'s `async for`
// (provider.py:61-101). It lives out here, apart from the HTTP/429 plumbing,
// because that is the only way it is testable under "unit tests only" — and
// reading one field instead of two is exactly how it shipped broken.

import FantasticAICore
import FantasticJSON

/// NIM's decoded line. The wire is the OpenAI-compatible one, so this is an
/// alias for `FantasticAICore.OpenAIDecoded` — kept as a name because this
/// backend's tests and call sites read better with it.
typealias NimDecoded = OpenAIDecoded

/// Decode one NIM `data:` SSE line.
///
/// DELEGATES to `decodeOpenAISSELine`: NIM and `llama-server` speak the same
/// wire, and two copies of this decode would drift on exactly the detail it
/// exists to get right — reading BOTH channels, not just `content`.
func decodeNimSSELine(_ line: String, wrap: inout ThinkingWrap) -> NimDecoded {
    decodeOpenAISSELine(line, wrap: &wrap)
}
