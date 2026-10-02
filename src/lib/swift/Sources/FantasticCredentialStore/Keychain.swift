// The OS keychain behind `credential_store`'s `keychain` backend — a small seam
// (python reaches it through `keyring`).
//
// One generic-password item per ref: service `keychain_service` (default
// `fantastic`), account `<store id>/<ref>`, the value the ref's fields as a JSON
// object — the same item python's `keyring` (macOS backend) reads and writes, so
// either runtime finds the other's credential.
//
// The real keychain is Security.framework's SecItem API. A platform without it
// FAILS LOUD at use, naming the platform — it never falls back to another
// backend (the `keychain` backend is the operator's explicit choice). Tests use
// an in-memory double and never touch the real keychain.

import FantasticJSON
import Foundation

#if canImport(Security)
    import Security
#endif

/// Where the `keychain` backend keeps its items: get / set / delete one item by
/// `(service, account)`. A failure throws (`PyError`), naming the operation.
public protocol KeychainStore: Sendable {
    /// The item's value, or nil when there is none.
    func get(service: String, account: String) throws -> String?
    /// Create the item, or replace its value.
    func set(service: String, account: String, value: String) throws
    /// Remove the item (absent is not an error).
    func delete(service: String, account: String) throws
}

#if canImport(Security)

    /// The OS keychain: Security.framework generic-password items.
    public struct SystemKeychain: KeychainStore {
        public init() {}

        private func query(_ service: String, _ account: String) -> [String: Any] {
            [
                kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: service,
                kSecAttrAccount as String: account,
            ]
        }

        private func failure(_ op: String, _ status: OSStatus) -> PyError {
            let why = (SecCopyErrorMessageString(status, nil) as String?) ?? "unknown error"
            return PyError(
                "KeyringError",
                "credential_store: the keychain refused to \(op) (OSStatus \(status): \(why))")
        }

        public func get(service: String, account: String) throws -> String? {
            var q = query(service, account)
            q[kSecReturnData as String] = true
            q[kSecMatchLimit as String] = kSecMatchLimitOne
            var out: CFTypeRef?
            let status = SecItemCopyMatching(q as CFDictionary, &out)
            if status == errSecItemNotFound { return nil }
            guard status == errSecSuccess, let data = out as? Data else {
                throw failure("read \(account)", status)
            }
            return String(decoding: data, as: UTF8.self)
        }

        public func set(service: String, account: String, value: String) throws {
            let data = Data(value.utf8)
            var add = query(service, account)
            add[kSecValueData as String] = data
            let status = SecItemAdd(add as CFDictionary, nil)
            if status == errSecSuccess { return }
            guard status == errSecDuplicateItem else { throw failure("store \(account)", status) }
            let upd = SecItemUpdate(
                query(service, account) as CFDictionary,
                [kSecValueData as String: data] as CFDictionary)
            guard upd == errSecSuccess else { throw failure("store \(account)", upd) }
        }

        public func delete(service: String, account: String) throws {
            let status = SecItemDelete(query(service, account) as CFDictionary)
            guard status == errSecSuccess || status == errSecItemNotFound else {
                throw failure("delete \(account)", status)
            }
        }
    }

#else

    /// No OS keychain on this platform: every use fails LOUD, naming it — never a
    /// fallback to another backend.
    public struct SystemKeychain: KeychainStore {
        public init() {}

        private var platform: String {
            #if os(Linux)
                return "Linux"
            #elseif os(Windows)
                return "Windows"
            #else
                return "this platform"
            #endif
        }

        private func unavailable() -> PyError {
            PyError(
                "RuntimeError",
                "credential_store: the keychain backend needs the OS keychain "
                    + "(Security.framework), which \(platform) does not have — choose the "
                    + "`file` or `env` backend")
        }

        public func get(service: String, account: String) throws -> String? {
            throw unavailable()
        }

        public func set(service: String, account: String, value: String) throws {
            throw unavailable()
        }

        public func delete(service: String, account: String) throws {
            throw unavailable()
        }
    }

#endif
