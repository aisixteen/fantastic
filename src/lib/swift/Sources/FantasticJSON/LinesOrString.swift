/// A `code` / `content` value as text, accepting a LIST OF LINES.
///
/// A program written as one JSON string is the most fragile thing a model
/// emits: every newline is an escape and every inner quote another, so one
/// missed `\"` swallows the rest of the object and the call is lost entirely.
/// Measured on gemma-4-12B, and again on the `subai_graph` probe, which burned
/// three consecutive turns on it.
///
/// A list removes the failure rather than softening it: newlines become the
/// array structure instead of escapes, and each element is short enough that a
/// lost quote costs ONE line rather than the whole document.
///
/// A LIST is the only newly accepted shape — anything else returns `nil` so the
/// existing refusals still fire. A bare `42` is a mistake, not a one-line
/// program, and quietly stringifying it would spawn an interpreter on nonsense.
///
/// It lives in JSON because it is a JSON-SHAPE question, and because both edges
/// an LLM writes through — the process edge (`exec_bridge`) and the disk edge
/// (`file_bridge`) — need exactly this. Putting it in AICore would have made the
/// disk surface depend on the AI stack to write a file.
public func linesOrString(_ value: JSON?) -> String? {
    guard let value else { return nil }
    if let s = value.asString { return s }
    if let items = value.asArray {
        return items.map { $0.asString ?? ($0.isNull ? "" : $0.serialize()) }
            .joined(separator: "\n")
    }
    return nil
}
