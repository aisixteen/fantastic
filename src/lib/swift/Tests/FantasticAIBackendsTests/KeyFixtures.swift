// The key reader a provider awaits before each request — in the kernel, the
// record's `credentials` read from a credential store (`Credentials.swift`); here
// a fixed answer, so a provider test never touches a store, the environment or
// the keychain.

import FantasticAICore
import FantasticJSON

/// A reader that answers `value` (nil = no `credentials` on the record: no key).
func fixedKey(_ value: String?) -> KeyReader {
    { _ in value }
}

/// A reader the store refused — the turn fails with this text.
func refusedKey(_ text: String) -> KeyReader {
    { _ in throw PyError("RuntimeError", text) }
}
