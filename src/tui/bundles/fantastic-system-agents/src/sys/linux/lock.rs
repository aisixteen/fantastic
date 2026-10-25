//! THE LOCK on Linux: logind's display session (followed: it changes at every
//! session start and end), its `Lock` and `Unlock` signals, `SetLockedHint`,
//! the sleep inhibitor, and the PIN in `<home>/lock.yaml` (Argon2id, mode
//! 0600). A kiosk UI lock, not a security boundary.

use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use fantastic_system::lock::{Call, Event, PinChange, Read, DEFAULT_AUTO_LOCK_MIN};
use futures_util::{Stream, StreamExt};
use rand_core::OsRng;
use tokio::sync::mpsc;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedFd, OwnedObjectPath};

use crate::sys::linux::logind::{
    display_session, user_path, LoginManagerProxy, LoginSessionProxy, LOGIND,
};
use crate::sys::linux::os_error;
use crate::sys::{LockOs, OsError};

/// How long the follower waits before it looks for the display session again.
const RETRY: Duration = Duration::from_secs(2);

/// What `<home>/lock.yaml` holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Settings {
    /// The PHC string of the PIN's Argon2id hash.
    pin_hash: Option<String>,
    auto_lock_min: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            pin_hash: None,
            auto_lock_min: DEFAULT_AUTO_LOCK_MIN,
        }
    }
}

fn io(path: &Path, e: std::io::Error) -> OsError {
    OsError::Failed(format!("{}: {e}", path.display()))
}

impl Settings {
    /// The file's settings; a missing file is the defaults.
    fn load(path: &Path) -> Result<Settings, OsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::default()),
            Err(e) => return Err(io(path, e)),
        };
        let doc: serde_yaml::Value = serde_yaml::from_str(&text)
            .map_err(|e| OsError::Failed(format!("{}: {e}", path.display())))?;
        let map = doc.as_mapping();
        Ok(Settings {
            pin_hash: map
                .and_then(|m| m.get("pin_hash"))
                .and_then(serde_yaml::Value::as_str)
                .map(str::to_string),
            auto_lock_min: map
                .and_then(|m| m.get("auto_lock_min"))
                .and_then(serde_yaml::Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(DEFAULT_AUTO_LOCK_MIN),
        })
    }

    /// Write the file whole and atomically, mode 0600 from the first byte.
    fn save(&self, path: &Path) -> Result<(), OsError> {
        use std::io::Write;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        }
        let mut doc = serde_yaml::Mapping::new();
        doc.insert(
            "pin_hash".into(),
            self.pin_hash
                .clone()
                .map_or(serde_yaml::Value::Null, serde_yaml::Value::String),
        );
        doc.insert("auto_lock_min".into(), self.auto_lock_min.into());
        let text = serde_yaml::to_string(&serde_yaml::Value::Mapping(doc))
            .map_err(|e| OsError::Failed(e.to_string()))?;
        let tmp = path.with_extension("yaml.tmp");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| io(&tmp, e))?;
        file.write_all(text.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|e| io(&tmp, e))?;
        std::fs::rename(&tmp, path).map_err(|e| io(path, e))
    }
}

/// The Argon2id hash of a PIN, as a PHC string.
fn hash_pin(pin: &str) -> Result<String, OsError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| OsError::Failed(format!("hashing the PIN: {e}")))
}

/// Is `pin` what `hash` hashes?
fn pin_matches(pin: &str, hash: &str) -> Result<bool, OsError> {
    let parsed = PasswordHash::new(hash)
        .map_err(|e| OsError::Failed(format!("the stored PIN hash is unreadable: {e}")))?;
    match Argon2::default().verify_password(pin.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(e) => Err(OsError::Failed(format!("checking the PIN: {e}"))),
    }
}

/// logind and `<home>/lock.yaml`, on a given system-bus connection.
pub struct LinuxLock {
    conn: zbus::Connection,
    /// `<home>/lock.yaml`, or why there is no home.
    file: Result<PathBuf, String>,
    /// The sleep inhibitor's fd; dropping it lets sleep proceed.
    inhibitor: Mutex<Option<OwnedFd>>,
}

impl LinuxLock {
    /// With `<home>` resolved as the user store does (`FANTASTIC_HOME`, else `~/.fantastic`).
    pub fn new(system: zbus::Connection) -> Self {
        LinuxLock {
            conn: system,
            file: fantastic_user_home_store::home_dir_from_env().map(|h| h.join("lock.yaml")),
            inhibitor: Mutex::new(None),
        }
    }

    /// With a given home (tests use a temp dir).
    pub fn with_home(system: zbus::Connection, home: &Path) -> Self {
        LinuxLock {
            conn: system,
            file: Ok(home.join("lock.yaml")),
            inhibitor: Mutex::new(None),
        }
    }

    fn path(&self) -> Result<&Path, OsError> {
        self.file
            .as_deref()
            .map_err(|e| OsError::Unavailable(format!("lock: {e}")))
    }

    async fn manager(&self) -> Result<LoginManagerProxy<'_>, OsError> {
        LoginManagerProxy::builder(&self.conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await
            .map_err(|e| os_error(e, LOGIND))
    }
}

#[async_trait::async_trait]
impl LockOs for LinuxLock {
    async fn read(&self) -> Result<Read, OsError> {
        let settings = Settings::load(self.path()?)?;
        // the display session is the lock's whole OS: no session, no lock
        display_session(&self.conn).await?;
        Ok(Read {
            has_pin: settings.pin_hash.is_some(),
            auto_lock_min: settings.auto_lock_min,
        })
    }

    async fn check_pin(&self, pin: fantastic_system::lock::Pin) -> Result<bool, OsError> {
        let path = self.path()?.to_path_buf();
        tokio::task::spawn_blocking(move || match Settings::load(&path)?.pin_hash {
            Some(hash) => pin_matches(pin.expose(), &hash),
            None => Ok(false),
        })
        .await
        .map_err(|e| OsError::Failed(e.to_string()))?
    }

    async fn call(&self, c: Call) -> Result<(), OsError> {
        match c {
            Call::SetLockedHint { locked } => {
                let e = |e| os_error(e, LOGIND);
                let (_, path) = display_session(&self.conn).await?;
                LoginSessionProxy::builder(&self.conn)
                    .path(path)
                    .map_err(e)?
                    .cache_properties(CacheProperties::No)
                    .build()
                    .await
                    .map_err(e)?
                    .set_locked_hint(locked)
                    .await
                    .map_err(e)
            }
            Call::HoldInhibitor { hold: false } => {
                self.inhibitor.lock().expect("inhibitor poisoned").take();
                Ok(())
            }
            Call::HoldInhibitor { hold: true } => {
                if self.inhibitor.lock().expect("inhibitor poisoned").is_some() {
                    return Ok(());
                }
                let fd = self
                    .manager()
                    .await?
                    .inhibit(
                        "sleep",
                        "fantastic",
                        "lock the screen before sleep",
                        "delay",
                    )
                    .await
                    .map_err(|e| os_error(e, LOGIND))?;
                *self.inhibitor.lock().expect("inhibitor poisoned") = Some(fd);
                Ok(())
            }
            Call::Persist { pin, auto_lock_min } => {
                let path = self.path()?.to_path_buf();
                tokio::task::spawn_blocking(move || {
                    let mut s = Settings::load(&path).unwrap_or_default();
                    s.auto_lock_min = auto_lock_min;
                    match pin {
                        PinChange::Keep => {}
                        PinChange::Clear => s.pin_hash = None,
                        PinChange::Set(p) => s.pin_hash = Some(hash_pin(p.expose())?),
                    }
                    s.save(&path)
                })
                .await
                .map_err(|e| OsError::Failed(e.to_string()))?
            }
        }
    }

    fn changes(&self) -> mpsc::Receiver<Event> {
        let (tx, rx) = mpsc::channel(16);
        tokio::spawn(follow(self.conn.clone(), tx));
        rx
    }
}

type Events = Pin<Box<dyn Stream<Item = Event> + Send>>;

/// The display session's `Lock` and `Unlock`, as events.
async fn session_events(conn: &zbus::Connection, path: OwnedObjectPath) -> Option<Events> {
    let session = LoginSessionProxy::builder(conn)
        .path(path)
        .ok()?
        .cache_properties(CacheProperties::No)
        .build()
        .await
        .ok()?;
    let lock = session
        .receive_lock()
        .await
        .ok()?
        .map(|_| Event::LogindLock);
    let unlock = session
        .receive_unlock()
        .await
        .ok()?
        .map(|_| Event::LogindUnlock);
    Some(Box::pin(futures_util::stream::select(lock, unlock)))
}

/// The next event of an optional stream; never, when there is none.
async fn next(s: &mut Option<Events>) -> Option<Event> {
    match s {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

/// Follow the user's display session and logind's sleep: `Lock`/`Unlock` of the
/// session `Display` names now, the subscription moved whenever the user's
/// `Display` changes (the special session paths emit no signals), and
/// `PrepareForSleep`. Until the end of the receiver.
async fn follow(conn: zbus::Connection, tx: mpsc::Sender<Event>) {
    // the user, and the manager's sleep signal: asked for again until logind answers
    let (manager, user) = loop {
        let manager = LoginManagerProxy::builder(&conn)
            .cache_properties(CacheProperties::No)
            .build()
            .await;
        if let (Ok(m), Ok(u)) = (manager, user_path(&conn).await) {
            break (m, u);
        }
        tokio::select! {
            _ = tx.closed() => return,
            _ = tokio::time::sleep(RETRY) => {}
        }
    };
    let Ok(mut sleep) = manager.receive_prepare_for_sleep().await else {
        return;
    };
    let props = async {
        zbus::fdo::PropertiesProxy::builder(&conn)
            .destination(LOGIND)?
            .path(user.clone())?
            .build()
            .await?
            .receive_properties_changed()
            .await
    }
    .await;
    let Ok(mut props) = props else {
        return;
    };
    let mut current: Option<OwnedObjectPath> = None;
    let mut events: Option<Events> = None;
    loop {
        if events.is_none() {
            if let Ok((_, path)) = display_session(&conn).await {
                events = session_events(&conn, path.clone()).await;
                current = events.is_some().then_some(path);
            }
        }
        tokio::select! {
            _ = tx.closed() => return,
            ev = next(&mut events) => match ev {
                Some(ev) => { if tx.send(ev).await.is_err() { return; } }
                None => { events = None; current = None; }
            },
            changed = props.next() => {
                if changed.is_none() { return; }
                // the display may have moved: look again, resubscribe only if it did
                let now = display_session(&conn).await.ok().map(|(_, p)| p);
                if now != current { events = None; current = None; }
            },
            s = sleep.next() => match s {
                Some(s) => {
                    let Ok(args) = s.args() else { continue };
                    if tx.send(Event::PrepareForSleep { start: args.start }).await.is_err() { return; }
                }
                None => return,
            },
            // no session now (a restart of the kiosk): try again soon
            _ = tokio::time::sleep(RETRY), if events.is_none() => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fantastic_lock_{}_{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn lock_yaml_round_trips_with_mode_0600() {
        let dir = temp("yaml");
        let file = dir.join("lock.yaml");
        // nothing there yet: the defaults, no PIN
        assert_eq!(Settings::load(&file).unwrap(), Settings::default());
        let hash = hash_pin("2468").unwrap();
        let saved = Settings {
            pin_hash: Some(hash.clone()),
            auto_lock_min: 7,
        };
        saved.save(&file).unwrap();
        // the file is private, and holds a hash that is not the PIN
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.contains("$argon2id$"), "{text}");
        assert!(!text.contains("2468"), "the PIN is not in the file");
        assert_ne!(hash, "2468");
        // has_pin survives a restart: a fresh load sees the same settings and the PIN checks
        let again = Settings::load(&file).unwrap();
        assert_eq!(again, saved);
        assert_eq!(again.auto_lock_min, 7);
        assert!(pin_matches("2468", again.pin_hash.as_deref().unwrap()).unwrap());
        assert!(!pin_matches("2469", &hash).unwrap());
        // a second hash of the same PIN differs (a salt), and both check
        let other = hash_pin("2468").unwrap();
        assert_ne!(other, hash);
        assert!(pin_matches("2468", &other).unwrap());
        // clearing the PIN keeps the minutes; an overwrite stays 0600
        Settings {
            pin_hash: None,
            auto_lock_min: 7,
        }
        .save(&file)
        .unwrap();
        let cleared = Settings::load(&file).unwrap();
        assert_eq!((cleared.pin_hash, cleared.auto_lock_min), (None, 7));
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!dir.join("lock.yaml.tmp").exists(), "no temp file is left");
        // a garbled file is an error, not a silent reset
        std::fs::write(&file, "pin_hash: [unclosed").unwrap();
        assert!(Settings::load(&file).is_err());
        assert!(pin_matches("1234", "not a hash").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
