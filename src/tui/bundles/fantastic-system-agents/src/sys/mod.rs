//! THE OS TREE: the ports the driver runs the ViewModels against, and the one
//! place each OS implements them. Linux has the real ones (`linux/`); every
//! other OS gets [`other`], which refuses. Nothing outside this tree, and
//! nothing outside `linux/` inside it, may name an OS crate or a process.

use std::sync::Arc;

use fantastic_system::{audio, displays, lock, network, notifications, power};

#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(not(target_os = "linux"))]
pub mod other;

/// Why an OS call failed. The reply's `reason` is the lower-case name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsError {
    /// The service is not there.
    Unavailable(String),
    /// The OS said no (authentication, policy).
    Denied(String),
    /// Anything else.
    Failed(String),
}

impl OsError {
    pub fn reason(&self) -> &'static str {
        match self {
            OsError::Unavailable(_) => "unavailable",
            OsError::Denied(_) => "denied",
            OsError::Failed(_) => "failed",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            OsError::Unavailable(m) | OsError::Denied(m) | OsError::Failed(m) => m,
        }
    }
}

/// The power agent's port: UPower and logind, flattened.
#[async_trait::async_trait]
pub trait PowerOs: Send + Sync {
    async fn read(&self) -> Result<power::Read, OsError>;
    async fn call(&self, c: power::Call) -> Result<(), OsError>;
    /// OS-side changes, as VM events, until the port is dropped.
    fn changes(&self) -> tokio::sync::mpsc::Receiver<power::Event>;
}

/// The real power port of this OS.
pub async fn power() -> Arc<dyn PowerOs> {
    #[cfg(target_os = "linux")]
    {
        linux::power().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(other::Unsupported)
    }
}

/// The audio agent's port: PipeWire through `pw-dump` and `wpctl`.
#[async_trait::async_trait]
pub trait AudioOs: Send + Sync {
    /// The whole `pw-dump` array, parsed.
    async fn read(&self) -> Result<serde_json::Value, OsError>;
    async fn call(&self, c: audio::Call) -> Result<(), OsError>;
    /// OS-side changes (`pw-dump --monitor`), as VM events, until the port is dropped.
    fn changes(&self) -> tokio::sync::mpsc::Receiver<audio::Event>;
}

/// The real audio port of this OS.
pub async fn audio() -> Arc<dyn AudioOs> {
    #[cfg(target_os = "linux")]
    {
        linux::audio()
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(other::Unsupported)
    }
}

/// The network agent's port: NetworkManager, flattened.
#[async_trait::async_trait]
pub trait NetworkOs: Send + Sync {
    async fn read(&self) -> Result<network::Read, OsError>;
    async fn call(&self, c: network::Call) -> Result<(), OsError>;
    /// OS-side changes, as VM events, until the port is dropped.
    fn changes(&self) -> tokio::sync::mpsc::Receiver<network::Event>;
}

/// The real network port of this OS.
pub async fn network() -> Arc<dyn NetworkOs> {
    #[cfg(target_os = "linux")]
    {
        linux::network().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(other::Unsupported)
    }
}

/// The name of the user running this process: whom a new connection belongs to.
pub fn user_name() -> String {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "fantastic".to_string())
}

/// The notifications agent's port: the session bus, where it is the server.
#[async_trait::async_trait]
pub trait NotificationsOs: Send + Sync {
    /// Own `org.freedesktop.Notifications` without queueing and without
    /// replacing another daemon, and serve it.
    async fn request(&self) -> Result<notifications::Ownership, OsError>;
    /// A signal to the clients, or the answer to the `Notify` call that waits.
    async fn call(&self, c: notifications::Call) -> Result<(), OsError>;
    /// What clients send (`Notify`, `CloseNotification`), as VM events, until the port is dropped.
    fn changes(&self) -> tokio::sync::mpsc::Receiver<notifications::Event>;
}

/// The real notifications port of this OS.
pub async fn notifications() -> Arc<dyn NotificationsOs> {
    #[cfg(target_os = "linux")]
    {
        linux::notifications().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(other::Unsupported)
    }
}

/// The displays agent's port: `wlr-randr`, logind's brightness and the backlight files.
#[async_trait::async_trait]
pub trait DisplaysOs: Send + Sync {
    /// `wlr-randr --json`, parsed, and the backlight devices.
    async fn read(&self) -> Result<displays::Read, OsError>;
    async fn call(&self, c: displays::Call) -> Result<(), OsError>;
    /// There is no change stream (`wlr-randr` has none): the agent reads on its clock.
    fn changes(&self) -> tokio::sync::mpsc::Receiver<displays::Event>;
}

/// The real displays port of this OS.
pub async fn displays() -> Arc<dyn DisplaysOs> {
    #[cfg(target_os = "linux")]
    {
        linux::displays().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(other::Unsupported)
    }
}

/// The lock agent's port: logind's display session and `<home>/lock.yaml`.
#[async_trait::async_trait]
pub trait LockOs: Send + Sync {
    /// The stored settings; an error when logind or the display session is not there.
    async fn read(&self) -> Result<lock::Read, OsError>;
    /// Is `pin` the stored PIN? (Argon2id; the PIN is never kept.)
    async fn check_pin(&self, pin: lock::Pin) -> Result<bool, OsError>;
    async fn call(&self, c: lock::Call) -> Result<(), OsError>;
    /// What logind says (`Lock`, `Unlock` on the display session, which is
    /// followed, and `PrepareForSleep`), as VM events, until the port is dropped.
    fn changes(&self) -> tokio::sync::mpsc::Receiver<lock::Event>;
}

/// The real lock port of this OS.
pub async fn lock() -> Arc<dyn LockOs> {
    #[cfg(target_os = "linux")]
    {
        linux::lock().await
    }
    #[cfg(not(target_os = "linux"))]
    {
        Arc::new(other::Unsupported)
    }
}
