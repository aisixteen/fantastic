//! The OS keychain behind ONE small trait — python's `keyring` calls
//! (`get_password` / `set_password` / `delete_password`), nothing more.
//!
//! [`OsKeychain`] is the real one: the macOS/iOS Keychain or the Windows
//! Credential Manager through the `keyring` crate, a generic-password item per
//! `(service, account)`. On any other platform this runtime has NO keychain, and
//! every call fails loud naming the platform — the store never answers from
//! another backend. [`MemoryKeychain`] is the in-memory double the tests use, so
//! no test ever touches the real keychain.

use std::collections::BTreeMap;
use std::sync::Mutex;

/// The three calls the `keychain` backend makes. `Err` is the platform's
/// failure, as text.
pub trait Keychain: Send + Sync {
    /// The item's value, or `None` when there is no such item.
    ///
    /// # Errors
    /// The platform's failure.
    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String>;

    /// Create or replace the item.
    ///
    /// # Errors
    /// The platform's failure.
    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String>;

    /// Remove the item.
    ///
    /// # Errors
    /// The platform's failure.
    fn delete_password(&self, service: &str, account: &str) -> Result<(), String>;
}

/// The OS keychain of this platform.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsKeychain;

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "windows"))]
impl Keychain for OsKeychain {
    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        let entry = keyring::Entry::new(service, account).map_err(|e| e.to_string())?;
        match entry.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }

    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        keyring::Entry::new(service, account)
            .and_then(|e| e.set_password(password))
            .map_err(|e| e.to_string())
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        keyring::Entry::new(service, account)
            .and_then(|e| e.delete_credential())
            .map_err(|e| e.to_string())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios", target_os = "windows")))]
impl Keychain for OsKeychain {
    fn get_password(&self, _service: &str, _account: &str) -> Result<Option<String>, String> {
        Err(no_keychain())
    }

    fn set_password(&self, _service: &str, _account: &str, _password: &str) -> Result<(), String> {
        Err(no_keychain())
    }

    fn delete_password(&self, _service: &str, _account: &str) -> Result<(), String> {
        Err(no_keychain())
    }
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

/// An in-memory keychain — the tests' double, never the real one.
#[derive(Debug, Default)]
pub struct MemoryKeychain {
    items: Mutex<BTreeMap<(String, String), String>>,
}

impl MemoryKeychain {
    /// Every `(service, account)` it holds, in order.
    #[must_use]
    pub fn accounts(&self) -> Vec<(String, String)> {
        self.items
            .lock()
            .expect("keychain poisoned")
            .keys()
            .cloned()
            .collect()
    }
}

impl Keychain for MemoryKeychain {
    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        Ok(self
            .items
            .lock()
            .expect("keychain poisoned")
            .get(&(service.to_string(), account.to_string()))
            .cloned())
    }

    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        self.items.lock().expect("keychain poisoned").insert(
            (service.to_string(), account.to_string()),
            password.to_string(),
        );
        Ok(())
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        self.items
            .lock()
            .expect("keychain poisoned")
            .remove(&(service.to_string(), account.to_string()));
        Ok(())
    }
}
