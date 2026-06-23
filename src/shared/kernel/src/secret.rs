//! OS-native secret store for connector API keys — macOS Keychain / Linux Secret
//! Service / Windows Credential Manager, via the `keyring` crate. Keys NEVER touch
//! `settings.json` on disk ("raw key is retarded"). If no OS keychain is available
//! the store fails LOUD (no raw/weak fallback) — the key must then be supplied via
//! env for that session.
//!
//! The backend is injectable ([`use_store`]) so tests use an in-memory [`MemStore`]
//! and never touch the real keychain.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// Keychain service name (the app identity).
const SERVICE: &str = "aisixteen.fantastic-tui";

/// A secret backend: store/read/delete a string by account (the provider id).
pub trait SecretStore: Send + Sync {
    fn set(&self, account: &str, value: &str) -> Result<(), String>;
    fn get(&self, account: &str) -> Option<String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

/// The real OS keychain backend.
struct KeyringStore;

impl SecretStore for KeyringStore {
    fn set(&self, account: &str, value: &str) -> Result<(), String> {
        let e = keyring::Entry::new(SERVICE, account)
            .map_err(|e| format!("no OS keychain available ({e}); set the key via env"))?;
        e.set_password(value)
            .map_err(|e| format!("keychain write failed: {e}"))
    }
    fn get(&self, account: &str) -> Option<String> {
        keyring::Entry::new(SERVICE, account)
            .ok()?
            .get_password()
            .ok()
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        let e = keyring::Entry::new(SERVICE, account)
            .map_err(|e| format!("no OS keychain available ({e})"))?;
        match e.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keychain delete failed: {e}")),
        }
    }
}

static STORE: OnceLock<Box<dyn SecretStore>> = OnceLock::new();

fn store() -> &'static dyn SecretStore {
    STORE.get_or_init(|| Box::new(KeyringStore)).as_ref()
}

/// Inject a backend (e.g. [`MemStore`] in tests). No-op if already initialized —
/// call before the first secret access.
pub fn use_store(s: Box<dyn SecretStore>) {
    let _ = STORE.set(s);
}

/// Store the key for `account` (the provider id). Fails loud if no keychain.
pub fn set_key(account: &str, value: &str) -> Result<(), String> {
    store().set(account, value)
}

/// Read the key for `account` — used ONLY to hydrate the env. `None` if unset.
pub fn get_key(account: &str) -> Option<String> {
    store().get(account)
}

/// Delete the key for `account` (idempotent).
pub fn delete_key(account: &str) -> Result<(), String> {
    store().delete(account)
}

/// Whether a key exists for `account` (the UI/CRUD use this — never the raw key).
pub fn has_key(account: &str) -> bool {
    store().get(account).is_some()
}

/// In-memory store for tests, so CRUD never touches the real OS keychain.
#[derive(Default)]
pub struct MemStore(Mutex<HashMap<String, String>>);

impl MemStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SecretStore for MemStore {
    fn set(&self, account: &str, value: &str) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(account.to_string(), value.to_string());
        Ok(())
    }
    fn get(&self, account: &str) -> Option<String> {
        self.0.lock().unwrap().get(account).cloned()
    }
    fn delete(&self, account: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(account);
        Ok(())
    }
}
