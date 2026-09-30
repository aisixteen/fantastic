//! PROVIDER KEYS, kept where the USER chose (`fantastic_user_home_store::secrets`):
//!
//! - PLAIN — one `0600` file per key under `<home>/secrets/` (no keychain, no
//!   prompt; readable by anything running as the user — the pages warn);
//! - SECURE — every key in one sealed file there, its master key the ONLY item
//!   in the keychain (asked once however many keys; never on a fresh home).
//!
//! There is no default: nothing is stored until a mode is chosen on the set-up
//! page (terminal or browser), and the page shows where the keys are.
//!
//! `keyed()` lists the linked backends that read a key: the account it is stored
//! under, and the environment variable the backend reads it from. A build that
//! links no such backend has an empty table, and the set-up page offers no `k`.
//!
//! A key stored from the page is loaded on the NEXT start: the backend reads it
//! from the process environment, and setting the environment once runtime
//! threads exist is a race, so it is set only before the runtime is built
//! (`load`, called first thing in `main`).

use fantastic_user_home_store::secrets::{Mode, Secrets, Vault};

/// The keychain service the shell uses (the secure mode's master key; the
/// dev-llm key).
#[cfg_attr(not(any(feature = "anthropic", feature = "responses")), allow(dead_code))]
pub const KEYCHAIN_SERVICE: &str = "aisixteen.fantastic-tui";

/// One linked backend that takes a key.
pub struct Keyed {
    pub module: &'static str,
    /// The account the key is stored under (a file name / a sealed entry).
    pub account: &'static str,
    /// The ONE variable the backend reads the key from; `load` sets it.
    pub var: &'static str,
}

/// Every linked backend that takes a key.
pub fn keyed() -> Vec<Keyed> {
    #[allow(unused_mut)]
    let mut v = Vec::new();
    #[cfg(feature = "anthropic")]
    v.push(Keyed {
        module: fantastic_anthropic_backend::HANDLER_MODULE,
        account: "anthropic",
        var: "ANTHROPIC_API_KEY",
    });
    #[cfg(feature = "responses")]
    v.push(Keyed {
        module: fantastic_responses_backend::HANDLER_MODULE,
        account: "openai",
        var: fantastic_responses_backend::KEY_VAR,
    });
    v
}

/// The keychain account for `module`'s key, if it takes one.
pub fn account_of(module: &str) -> Option<&'static str> {
    keyed().into_iter().find(|k| k.module == module).map(|k| k.account)
}

/// The keychain item that holds the SECURE mode's master key.
#[cfg_attr(not(any(feature = "anthropic", feature = "responses")), allow(dead_code))]
pub const MASTER_KEY_ACCOUNT: &str = "master-key";

/// The platform keychain as the secure mode's [`Vault`]: ONE item, the master
/// key, hex-encoded (the keychain stores a string).
struct Keychain;

#[cfg(any(feature = "anthropic", feature = "responses"))]
impl Vault for Keychain {
    fn get(&self) -> Result<Option<[u8; 32]>, String> {
        let e = keyring::Entry::new(KEYCHAIN_SERVICE, MASTER_KEY_ACCOUNT).map_err(|e| format!("keychain: {e}"))?;
        let hex = match e.get_password() {
            Ok(h) => h,
            Err(keyring::Error::NoEntry) => return Ok(None),
            Err(e) => return Err(format!("keychain: {e}")),
        };
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
            .collect::<Option<_>>()
            .ok_or("keychain: the master key is not hex")?;
        let key: [u8; 32] = bytes.try_into().map_err(|_| "keychain: the master key is not 32 bytes".to_string())?;
        Ok(Some(key))
    }
    fn put(&self, key: &[u8; 32]) -> Result<(), String> {
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        keyring::Entry::new(KEYCHAIN_SERVICE, MASTER_KEY_ACCOUNT)
            .and_then(|e| e.set_password(&hex))
            .map_err(|e| format!("keychain: {e}"))
    }
    fn delete(&self) -> Result<(), String> {
        match keyring::Entry::new(KEYCHAIN_SERVICE, MASTER_KEY_ACCOUNT).and_then(|e| e.delete_credential()) {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(format!("keychain: {e}")),
        }
    }
    fn describe(&self) -> String {
        format!("the keychain ({KEYCHAIN_SERVICE} / {MASTER_KEY_ACCOUNT})")
    }
}

/// A build with no keyed backend links no keychain: the secure mode says so.
#[cfg(not(any(feature = "anthropic", feature = "responses")))]
impl Vault for Keychain {
    fn get(&self) -> Result<Option<[u8; 32]>, String> {
        Err("this build links no keychain".into())
    }
    fn put(&self, _key: &[u8; 32]) -> Result<(), String> {
        Err("this build links no keychain".into())
    }
    fn delete(&self) -> Result<(), String> {
        Ok(())
    }
    fn describe(&self) -> String {
        "the keychain (not linked into this build)".into()
    }
}

fn secrets() -> Secrets {
    Secrets::new(fantastic_user_home_store::home_dir_from_env(), Box::new(Keychain))
}

/// Where keys are kept: the chosen mode (`None` until chosen) and, in words,
/// where that is.
pub fn mode() -> Result<(Option<Mode>, String), String> {
    let s = secrets();
    let m = s.mode()?;
    Ok((m, s.describe(m)))
}

/// Choose (or change) where keys are kept — moving any already stored.
pub fn set_mode(m: Mode) -> Result<String, String> {
    let s = secrets();
    s.set_mode(m)?;
    Ok(s.describe(Some(m)))
}

/// Store `secret` for `module`, where the user chose. Returns where it went.
pub fn store(module: &str, secret: &str) -> Result<String, String> {
    let account = account_of(module).ok_or_else(|| format!("{module} takes no key in this build"))?;
    secrets().store(account, secret)
}

/// Put each stored key into the process environment. MUST run before the
/// runtime exists. A key already in the environment wins; a key never stored is
/// not an error (the backend refuses the call, with its own words, when it needs
/// one); a store that cannot be read is logged, loud, and nothing is set.
///
/// CRITICAL TODO(dev-llm): a dev build loads the anthropic key itself
/// (`dev_llm::load_key`, which fails loud), so it is skipped here.
pub fn load() {
    let table = keyed();
    if table.iter().all(|k| std::env::var_os(k.var).is_some()) {
        return; // every key is in the environment: the store is not even read
    }
    let all = match secrets().load_all() {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!("provider keys not loaded: {e}");
            return;
        }
    };
    for k in table {
        #[cfg(feature = "dev-llm")]
        if k.account == "anthropic" {
            continue;
        }
        if std::env::var_os(k.var).is_some() {
            continue;
        }
        if let Some(v) = all.get(k.account) {
            std::env::set_var(k.var, v);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_keyed_backend_names_an_account_and_a_variable() {
        for k in super::keyed() {
            assert!(!k.account.is_empty() && !k.var.is_empty(), "{}", k.module);
            assert_eq!(super::account_of(k.module), Some(k.account));
        }
    }
}
