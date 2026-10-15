//! The OS keychain: the macOS/iOS Keychain or the Windows Credential Manager
//! through the `keyring` crate, a generic-password item per
//! `(service, account)`. Every other platform has no keychain in this runtime
//! and every call fails loud naming it.

/// The item's value, or `None` when there is no such item.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
pub(crate) fn keychain_get(service: &str, account: &str) -> Result<Option<String>, String> {
    let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
    match entry.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Create or replace the item.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
pub(crate) fn keychain_set(service: &str, account: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(service, account)
        .and_then(|e| e.set_password(password))
        .map_err(|e| e.to_string())
}

/// Remove the item.
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
pub(crate) fn keychain_delete(service: &str, account: &str) -> Result<(), String> {
    keyring::Entry::new(service, account)
        .and_then(|e| e.delete_credential())
        .map_err(|e| e.to_string())
}

/// No keychain on this platform: fails loud.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
pub(crate) fn keychain_get(_service: &str, _account: &str) -> Result<Option<String>, String> {
    Err(no_keychain())
}

/// No keychain on this platform: fails loud.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
pub(crate) fn keychain_set(_service: &str, _account: &str, _password: &str) -> Result<(), String> {
    Err(no_keychain())
}

/// No keychain on this platform: fails loud.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
pub(crate) fn keychain_delete(_service: &str, _account: &str) -> Result<(), String> {
    Err(no_keychain())
}

/// The loud answer of a platform with no keychain in this runtime.
#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
fn no_keychain() -> String {
    format!(
        "credential_store: the keychain backend has no OS keychain on {} in this runtime \
         — choose backend file or env",
        std::env::consts::OS
    )
}
