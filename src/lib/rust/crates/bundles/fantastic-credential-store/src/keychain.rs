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

impl Keychain for OsKeychain {
    fn get_password(&self, service: &str, account: &str) -> Result<Option<String>, String> {
        crate::sys::keychain_get(service, account)
    }

    fn set_password(&self, service: &str, account: &str, password: &str) -> Result<(), String> {
        crate::sys::keychain_set(service, account, password)
    }

    fn delete_password(&self, service: &str, account: &str) -> Result<(), String> {
        crate::sys::keychain_delete(service, account)
    }
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
