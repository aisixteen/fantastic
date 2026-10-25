//! LINUX: the OS side of the ports. The only place in the crate that names
//! the bus crates or runs a program. Each D-Bus port is given its connection
//! (`LinuxPower::new`); the CLI's own are made once, here, on first use. Audio
//! runs PipeWire's own tools (`LinuxAudio`).

use std::sync::Arc;

use tokio::sync::OnceCell;

use super::{AudioOs, OsError, PowerOs};

pub mod audio;
pub mod power;

pub use audio::LinuxAudio;
pub use power::LinuxPower;

/// The system bus, connected once per process.
static SYSTEM: OnceCell<Result<zbus::Connection, String>> = OnceCell::const_new();

async fn system() -> Result<zbus::Connection, OsError> {
    SYSTEM
        .get_or_init(|| async { zbus::Connection::system().await.map_err(|e| e.to_string()) })
        .await
        .clone()
        .map_err(|e| OsError::Unavailable(format!("the system bus is not reachable: {e}")))
}

/// The power port on the system bus; a bus that cannot be reached is a port
/// that says so, never a failed boot.
pub async fn power() -> Arc<dyn PowerOs> {
    match system().await {
        Ok(c) => Arc::new(LinuxPower::new(c)),
        Err(e) => Arc::new(Absent(e)),
    }
}

/// The audio port: `pw-dump` and `wpctl` from `$PATH`. A missing tool is a port
/// that says so on its first read.
pub fn audio() -> Arc<dyn AudioOs> {
    Arc::new(LinuxAudio::new())
}

/// A port whose bus never connected.
struct Absent(OsError);

#[async_trait::async_trait]
impl PowerOs for Absent {
    async fn read(&self) -> Result<fantastic_system::power::Read, OsError> {
        Err(self.0.clone())
    }
    async fn call(&self, _c: fantastic_system::power::Call) -> Result<(), OsError> {
        Err(self.0.clone())
    }
    fn changes(&self) -> tokio::sync::mpsc::Receiver<fantastic_system::power::Event> {
        tokio::sync::mpsc::channel(1).1
    }
}

/// A D-Bus error as the port's error.
pub(crate) fn os_error(e: zbus::Error, service: &str) -> OsError {
    let fdo = match e {
        zbus::Error::FDO(b) => *b,
        other => zbus::fdo::Error::from(other),
    };
    match fdo {
        zbus::fdo::Error::ServiceUnknown(_) | zbus::fdo::Error::NameHasNoOwner(_) => {
            OsError::Unavailable(format!("{service} is not on the system bus"))
        }
        zbus::fdo::Error::AccessDenied(m) | zbus::fdo::Error::AuthFailed(m) => OsError::Denied(m),
        zbus::fdo::Error::Failed(m) => OsError::Failed(m),
        other => OsError::Failed(other.to_string()),
    }
}
