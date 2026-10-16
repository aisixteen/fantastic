//! WHERE KEYS ARE KEPT — the one choice about secrets the user makes here
//! (there is no default: nothing is stored until a mode is chosen).
//!
//! ```text
//!   <home>/agents/ai_agent/keys/   0700
//!   ├── mode                       0600   "plain" | "secure"
//!   ├── refs.json                  0600   {ref: host} — the keys stored, NO values
//!   └── credentials.yaml           0600   the PLAIN mode's values
//! ```
//!
//! The keys themselves live in the kernel's CREDENTIAL STORE (the
//! `credential_store` bundle, owner 2026-10-01): a value is a ref bound to one
//! host, written and read only with secret marks, never in a record, a row or
//! the environment. The mode decides that store's BACKEND ([`Mode::store`]):
//!
//! - PLAIN — one private file, `keys/credentials.yaml` (0600): no keychain,
//!   no prompts, readable by anything running as the user;
//! - SECURE — the platform keychain, one item per ref.
//!
//! Changing the mode with keys already stored is refused by the caller (the
//! values would stay behind in the old backend).

use std::path::PathBuf;

use serde_json::{json, Value};

use crate::{ai_agent_dir, private, private_dirs, read_private, write_private};

/// The keys dir under the home: `agents/ai_agent/keys`.
pub fn keys_dir(home: &std::path::Path) -> PathBuf {
    ai_agent_dir(home).join("keys")
}

/// The keychain namespace the SECURE mode's items live under.
pub const KEYCHAIN_NAMESPACE: &str = "aisixteen.fantastic-tui";

/// The two modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// One private file, in the clear.
    Plain,
    /// The platform keychain.
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
    /// The credential store's record fields for this mode, under `home`.
    pub fn store(self, home: &std::path::Path) -> Value {
        match self {
            Mode::Plain => json!({"backend": "file", "file": keys_dir(home).join(CREDENTIALS).display().to_string()}),
            Mode::Secure => json!({"backend": "keychain", "keychain_namespace": KEYCHAIN_NAMESPACE}),
        }
    }
}

/// The plain mode's file, in the keys dir.
pub const CREDENTIALS: &str = "credentials.yaml";

/// The key-mode choice over the keys dir.
pub struct Secrets {
    home: Result<PathBuf, String>,
}

const MODE: &str = "mode";
/// The refs stored so far: `{ref: host}`, no values.
const REFS: &str = "refs.json";

fn msg(v: Value) -> String {
    let e = v.get("error").and_then(Value::as_str).unwrap_or("secrets: failed");
    match v.get("hint").and_then(Value::as_str) {
        Some(h) => format!("{e} — {h}"),
        None => e.to_string(),
    }
}

impl Secrets {
    /// `home`: the resolved home dir (or why there is none).
    pub fn new(home: Result<PathBuf, String>) -> Self {
        Self { home }
    }

    /// The home itself.
    pub fn home(&self) -> Result<PathBuf, String> {
        self.home.clone()
    }

    fn dir(&self) -> Result<PathBuf, String> {
        Ok(keys_dir(self.home.as_ref().map_err(Clone::clone)?))
    }

    /// The chosen mode, `None` until the user chooses one. Creates nothing.
    pub fn mode(&self) -> Result<Option<Mode>, String> {
        let dir = self.dir()?;
        if !dir.exists() {
            return Ok(None);
        }
        private(&dir, true).map_err(msg)?;
        Ok(read_private(&dir.join(MODE))
            .map_err(msg)?
            .and_then(|b| Mode::parse(&String::from_utf8_lossy(&b))))
    }

    /// Where the keys are, in the user's words — shown next to the mode.
    pub fn describe(&self, mode: Option<Mode>) -> String {
        let dir = self.dir().map(|d| d.display().to_string()).unwrap_or_else(|e| e);
        match mode {
            None => "not chosen — pick plain or secure before storing a key".to_string(),
            Some(Mode::Plain) => format!("plain text in {dir}/{CREDENTIALS} (0600)"),
            Some(Mode::Secure) => format!("the keychain (namespace {KEYCHAIN_NAMESPACE}, one item per key)"),
        }
    }

    /// The refs stored so far (`ref → host`) — metadata only, so a new
    /// project's credential store knows the keys this home already holds.
    pub fn refs(&self) -> Result<Vec<(String, String)>, String> {
        let dir = self.dir()?;
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let Some(b) = read_private(&dir.join(REFS)).map_err(msg)? else { return Ok(Vec::new()) };
        let v: Value = serde_json::from_slice(&b).map_err(|e| format!("secrets: {REFS}: {e}"))?;
        Ok(v.as_object()
            .map(|m| m.iter().map(|(k, h)| (k.clone(), h.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default())
    }

    /// Remember that a key is stored under `r`, bound to `host`.
    pub fn remember_ref(&self, r: &str, host: &str) -> Result<(), String> {
        let dir = self.dir()?;
        let mut all: serde_json::Map<String, Value> = self.refs()?.into_iter().map(|(k, h)| (k, json!(h))).collect();
        all.insert(r.to_string(), json!(host));
        let body = serde_json::to_vec_pretty(&Value::Object(all)).map_err(|e| e.to_string())?;
        write_private(&dir.join(REFS), &body).map_err(msg)
    }

    /// Record the mode. The home and each level down to the keys dir are created `0700`.
    pub fn set_mode(&self, to: Mode) -> Result<(), String> {
        let home = self.home.as_ref().map_err(Clone::clone)?;
        let dir = keys_dir(home);
        private_dirs(home, &dir).map_err(msg)?;
        write_private(&dir.join(MODE), to.name().as_bytes()).map_err(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn home() -> PathBuf {
        let d = std::env::temp_dir().join(format!("fc-secrets-{}-{}", std::process::id(), rand_suffix()));
        let _ = fs::remove_dir_all(&d);
        d
    }

    fn rand_suffix() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    #[test]
    fn no_mode_until_chosen_and_choosing_creates_nothing_but_the_mode() {
        let h = home();
        let s = Secrets::new(Ok(h.clone()));
        assert_eq!(s.mode().unwrap(), None);
        assert!(!h.exists(), "reading the mode creates nothing");
        s.set_mode(Mode::Plain).unwrap();
        assert_eq!(s.mode().unwrap(), Some(Mode::Plain));
        s.set_mode(Mode::Secure).unwrap();
        assert_eq!(s.mode().unwrap(), Some(Mode::Secure));
        assert!(s.refs().unwrap().is_empty());
        s.remember_ref("vllm/box:8000", "box:8000").unwrap();
        assert_eq!(s.refs().unwrap(), vec![("vllm/box:8000".to_string(), "box:8000".to_string())]);
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn keys_live_under_agents_ai_agent_keys_every_level_private() {
        let h = home();
        Secrets::new(Ok(h.clone())).set_mode(Mode::Plain).unwrap();
        for d in ["", "agents", "agents/ai_agent", "agents/ai_agent/keys"] {
            assert_eq!(crate::sys::mode_bits(&fs::metadata(h.join(d)).unwrap()), 0o700, "{d}");
        }
        assert!(h.join("agents/ai_agent/keys/mode").is_file());
        assert!(!h.join("secrets").exists(), "nothing at the old place");
        let _ = fs::remove_dir_all(&h);
    }

    #[test]
    fn each_mode_names_its_credential_store_backend() {
        let h = PathBuf::from("/h");
        assert_eq!(Mode::Plain.store(&h), json!({"backend": "file", "file": "/h/agents/ai_agent/keys/credentials.yaml"}));
        assert_eq!(Mode::Secure.store(&h)["backend"], "keychain");
    }
}
