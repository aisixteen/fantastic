//! SECRETS — where the user's provider keys are kept, in one of two modes the
//! user CHOOSES (there is no default: nothing is stored until a mode is set).
//!
//! ```text
//!   <home>/secrets/            0700
//!   ├── mode                   0600   "plain" | "secure"
//!   ├── <account>              0600   PLAIN: the key itself, one file per account
//!   └── vault.bin              0600   SECURE: every key, sealed (ChaCha20-Poly1305)
//! ```
//!
//! PLAIN keeps each key as a private file — no keychain, no prompts, readable by
//! anything running as the user. SECURE keeps them all in ONE sealed file whose
//! 32-byte master key is the only thing in the platform keychain ([`Vault`]): the
//! keychain is asked once, however many keys there are. A fresh home, or one with
//! nothing sealed yet, never asks it at all.
//!
//! Switching mode moves what is stored: plain → secure seals every key and
//! removes the plain files; secure → plain writes them back out and forgets the
//! sealed file and the master key.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use serde_json::Value;

use crate::{private, read_private, valid_name, write_private};

/// Where the SECURE mode keeps its master key — the platform keychain in the
/// product, an in-memory stand-in in tests.
pub trait Vault: Send + Sync {
    /// The master key, or `None` when none was ever stored.
    fn get(&self) -> Result<Option<[u8; 32]>, String>;
    /// Keep `key` as the master key.
    fn put(&self, key: &[u8; 32]) -> Result<(), String>;
    /// Forget the master key.
    fn delete(&self) -> Result<(), String>;
    /// Where it is, for the user ("the keychain, service … / account …").
    fn describe(&self) -> String;
}

/// The two modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One private file per key, in the clear.
    Plain,
    /// One sealed file; its master key in the [`Vault`].
    Secure,
}

impl Mode {
    /// Its name on disk and on the wire.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Plain => "plain",
            Mode::Secure => "secure",
        }
    }
    /// Parse a name.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "plain" => Some(Mode::Plain),
            "secure" => Some(Mode::Secure),
            _ => None,
        }
    }
}

/// The secrets store over `<home>/secrets`.
pub struct Secrets {
    home: Result<PathBuf, String>,
    vault: Box<dyn Vault>,
}

const VAULT: &str = "vault.bin";
const MODE: &str = "mode";

fn msg(v: Value) -> String {
    let e = v.get("error").and_then(Value::as_str).unwrap_or("secrets: failed");
    match v.get("hint").and_then(Value::as_str) {
        Some(h) => format!("{e} — {h}"),
        None => e.to_string(),
    }
}

impl Secrets {
    /// `home`: the resolved home dir (or why there is none). `vault`: where the
    /// secure mode's master key lives.
    pub fn new(home: Result<PathBuf, String>, vault: Box<dyn Vault>) -> Self {
        Self { home, vault }
    }

    fn dir(&self) -> Result<PathBuf, String> {
        Ok(self.home.as_ref().map_err(Clone::clone)?.join("secrets"))
    }

    /// The dir for a WRITE: created `0700` (with the home) if missing.
    fn dir_for_write(&self) -> Result<PathBuf, String> {
        let home = self.home.as_ref().map_err(Clone::clone)?;
        let dir = home.join("secrets");
        for d in [home.as_path(), dir.as_path()] {
            if !d.exists() {
                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(d)
                    .map_err(|e| format!("secrets: {}: {e}", d.display()))?;
            }
            private(d, true).map_err(msg)?;
        }
        Ok(dir)
    }

    /// The dir for a READ: `None` when nothing was ever stored (creates nothing).
    fn dir_if_any(&self) -> Result<Option<PathBuf>, String> {
        let dir = self.dir()?;
        if !dir.exists() {
            return Ok(None);
        }
        private(&dir, true).map_err(msg)?;
        Ok(Some(dir))
    }

    /// The chosen mode, `None` until the user chooses one.
    pub fn mode(&self) -> Result<Option<Mode>, String> {
        let Some(dir) = self.dir_if_any()? else { return Ok(None) };
        Ok(read_private(&dir.join(MODE))
            .map_err(msg)?
            .and_then(|b| Mode::parse(&String::from_utf8_lossy(&b))))
    }

    /// Where the keys are, in the user's words — shown next to the mode.
    pub fn describe(&self, mode: Option<Mode>) -> String {
        let dir = self.dir().map(|d| d.display().to_string()).unwrap_or_else(|e| e);
        match mode {
            None => format!("not chosen — keys are stored in {dir} once you pick plain or secure"),
            Some(Mode::Plain) => format!("plain text in {dir} (one 0600 file per key)"),
            Some(Mode::Secure) => format!("sealed in {dir}/{VAULT}; its master key in {}", self.vault.describe()),
        }
    }

    fn plain_all(dir: &Path) -> Result<BTreeMap<String, String>, String> {
        let mut out = BTreeMap::new();
        for e in fs::read_dir(dir).map_err(|e| format!("secrets: {}: {e}", dir.display()))?.flatten() {
            let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
            if name == MODE || name == VAULT || !valid_name(&name) {
                continue;
            }
            if let Some(b) = read_private(&e.path()).map_err(msg)? {
                out.insert(name, String::from_utf8_lossy(&b).to_string());
            }
        }
        Ok(out)
    }

    fn sealed_all(&self, dir: &Path) -> Result<BTreeMap<String, String>, String> {
        let Some(blob) = read_private(&dir.join(VAULT)).map_err(msg)? else {
            return Ok(BTreeMap::new());
        };
        let key = self
            .vault
            .get()?
            .ok_or_else(|| format!("secrets: {VAULT} is sealed but {} holds no master key", self.vault.describe()))?;
        if blob.len() < 12 {
            return Err(format!("secrets: {VAULT} is too short to be sealed"));
        }
        let (nonce, ct) = blob.split_at(12);
        let plain = ChaCha20Poly1305::new(Key::from_slice(&key))
            .decrypt(Nonce::from_slice(nonce), ct)
            .map_err(|_| format!("secrets: {VAULT} does not open with the master key in {}", self.vault.describe()))?;
        serde_json::from_slice(&plain).map_err(|e| format!("secrets: {VAULT}: {e}"))
    }

    fn seal(&self, dir: &Path, all: &BTreeMap<String, String>) -> Result<(), String> {
        let key = match self.vault.get()? {
            Some(k) => k,
            None => {
                let mut k = [0u8; 32];
                getrandom::getrandom(&mut k).map_err(|e| format!("secrets: no randomness: {e}"))?;
                self.vault.put(&k)?;
                k
            }
        };
        let mut nonce = [0u8; 12];
        getrandom::getrandom(&mut nonce).map_err(|e| format!("secrets: no randomness: {e}"))?;
        let plain = serde_json::to_vec(all).map_err(|e| e.to_string())?;
        let ct = ChaCha20Poly1305::new(Key::from_slice(&key))
            .encrypt(Nonce::from_slice(&nonce), plain.as_slice())
            .map_err(|_| "secrets: sealing failed".to_string())?;
        let mut blob = nonce.to_vec();
        blob.extend(ct);
        write_private(&dir.join(VAULT), &blob).map_err(msg)
    }

    /// Every stored key, by account. Touches the vault ONLY when something is
    /// sealed — a fresh home never asks the keychain.
    pub fn load_all(&self) -> Result<BTreeMap<String, String>, String> {
        let Some(dir) = self.dir_if_any()? else { return Ok(BTreeMap::new()) };
        match self.mode()? {
            None => Ok(BTreeMap::new()),
            Some(Mode::Plain) => Self::plain_all(&dir),
            Some(Mode::Secure) => self.sealed_all(&dir),
        }
    }

    /// Store `secret` for `account`, in the chosen mode. Refused until a mode is
    /// chosen. Returns where it went.
    pub fn store(&self, account: &str, secret: &str) -> Result<String, String> {
        if !valid_name(account) {
            return Err(format!("secrets: account {account:?} — letters, digits, - _"));
        }
        let mode = self.mode()?.ok_or("choose where keys are kept first — plain or secure")?;
        let dir = self.dir_for_write()?;
        match mode {
            Mode::Plain => {
                write_private(&dir.join(account), secret.as_bytes()).map_err(msg)?;
                Ok(format!("{}/{account} (plain text)", dir.display()))
            }
            Mode::Secure => {
                let mut all = self.sealed_all(&dir)?;
                all.insert(account.to_string(), secret.to_string());
                self.seal(&dir, &all)?;
                Ok(format!("{}/{VAULT} (sealed; master key in {})", dir.display(), self.vault.describe()))
            }
        }
    }

    /// Choose (or change) the mode, MOVING what is stored.
    pub fn set_mode(&self, to: Mode) -> Result<(), String> {
        let dir = self.dir_for_write()?;
        let from = self.mode()?;
        if from == Some(to) {
            return Ok(());
        }
        match (from, to) {
            (Some(Mode::Plain), Mode::Secure) => {
                let all = Self::plain_all(&dir)?;
                self.seal(&dir, &all)?;
                write_private(&dir.join(MODE), to.name().as_bytes()).map_err(msg)?;
                for acct in all.keys() {
                    let _ = fs::remove_file(dir.join(acct));
                }
            }
            (Some(Mode::Secure), Mode::Plain) => {
                let all = self.sealed_all(&dir)?;
                for (acct, secret) in &all {
                    write_private(&dir.join(acct), secret.as_bytes()).map_err(msg)?;
                }
                write_private(&dir.join(MODE), to.name().as_bytes()).map_err(msg)?;
                let _ = fs::remove_file(dir.join(VAULT));
                self.vault.delete()?;
            }
            _ => write_private(&dir.join(MODE), to.name().as_bytes()).map_err(msg)?,
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    /// The keychain stand-in: counts every read, so a test can prove a fresh
    /// home never asks it.
    #[derive(Default, Clone)]
    struct Fake(Arc<(Mutex<Option<[u8; 32]>>, AtomicUsize)>);
    impl Vault for Fake {
        fn get(&self) -> Result<Option<[u8; 32]>, String> {
            self.0 .1.fetch_add(1, Ordering::SeqCst);
            Ok(*self.0 .0.lock().unwrap())
        }
        fn put(&self, k: &[u8; 32]) -> Result<(), String> {
            *self.0 .0.lock().unwrap() = Some(*k);
            Ok(())
        }
        fn delete(&self) -> Result<(), String> {
            *self.0 .0.lock().unwrap() = None;
            Ok(())
        }
        fn describe(&self) -> String {
            "the test vault".into()
        }
    }

    fn store(v: &Fake) -> (tempfile::TempDir, PathBuf, Secrets) {
        let td = tempfile::tempdir().unwrap();
        let home = td.path().join("home");
        let s = Secrets::new(Ok(home.clone()), Box::new(v.clone()));
        (td, home, s)
    }

    #[test]
    fn nothing_is_stored_until_a_mode_is_chosen_and_a_fresh_home_never_asks_the_vault() {
        let v = Fake::default();
        let (_td, home, s) = store(&v);
        assert_eq!(s.mode().unwrap(), None);
        assert!(s.load_all().unwrap().is_empty());
        assert!(s.store("anthropic", "sk-1").unwrap_err().contains("choose where keys are kept"));
        assert!(!home.exists(), "reading and a refused store create nothing");
        assert_eq!(v.0 .1.load(Ordering::SeqCst), 0, "the keychain was never asked");
    }

    #[test]
    fn plain_keeps_one_private_file_per_key() {
        let v = Fake::default();
        let (_td, home, s) = store(&v);
        s.set_mode(Mode::Plain).unwrap();
        s.store("anthropic", "sk-1").unwrap();
        let f = home.join("secrets/anthropic");
        assert_eq!(fs::read_to_string(&f).unwrap(), "sk-1");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(fs::metadata(&f).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(s.load_all().unwrap().get("anthropic").map(String::as_str), Some("sk-1"));
        assert_eq!(v.0 .1.load(Ordering::SeqCst), 0, "plain never touches the keychain");
    }

    #[test]
    fn secure_seals_every_key_in_one_file_opened_by_one_master_key() {
        let v = Fake::default();
        let (_td, home, s) = store(&v);
        s.set_mode(Mode::Secure).unwrap();
        s.store("anthropic", "sk-ant-SECRET").unwrap();
        s.store("openai", "sk-oai-SECRET").unwrap();
        let blob = fs::read(home.join("secrets/vault.bin")).unwrap();
        assert!(!String::from_utf8_lossy(&blob).contains("SECRET"), "the file holds no key in the clear");
        assert!(!home.join("secrets/anthropic").exists());
        let all = s.load_all().unwrap();
        assert_eq!((all["anthropic"].as_str(), all["openai"].as_str()), ("sk-ant-SECRET", "sk-oai-SECRET"));
        // another master key cannot open it
        *v.0 .0.lock().unwrap() = Some([7u8; 32]);
        assert!(s.load_all().unwrap_err().contains("does not open"));
    }

    #[test]
    fn switching_mode_moves_the_keys_both_ways() {
        let v = Fake::default();
        let (_td, home, s) = store(&v);
        s.set_mode(Mode::Plain).unwrap();
        s.store("anthropic", "sk-1").unwrap();
        s.set_mode(Mode::Secure).unwrap();
        assert!(!home.join("secrets/anthropic").exists(), "the plain file is gone once sealed");
        assert_eq!(s.load_all().unwrap()["anthropic"], "sk-1");
        s.set_mode(Mode::Plain).unwrap();
        assert!(!home.join("secrets/vault.bin").exists());
        assert!(v.0 .0.lock().unwrap().is_none(), "the master key is forgotten");
        assert_eq!(fs::read_to_string(home.join("secrets/anthropic")).unwrap(), "sk-1");
    }
}
