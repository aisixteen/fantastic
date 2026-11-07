//! Every OS but Linux: the agents exist where a record names them, and say
//! they are not available.

use fantastic_system::{audio, displays, lock, network, notifications, power};
use tokio::sync::mpsc;

use super::{AudioOs, DisplaysOs, LockOs, NetworkOs, NotificationsOs, OsError, PowerOs};

pub struct Unsupported;

const WHY: &str = "the system agents need Linux with D-Bus";

#[async_trait::async_trait]
impl PowerOs for Unsupported {
    async fn read(&self) -> Result<power::Read, OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    async fn call(&self, _c: power::Call) -> Result<(), OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    fn changes(&self) -> mpsc::Receiver<power::Event> {
        mpsc::channel(1).1
    }
}

#[async_trait::async_trait]
impl AudioOs for Unsupported {
    async fn read(&self) -> Result<serde_json::Value, OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    async fn call(&self, _c: audio::Call) -> Result<(), OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    fn changes(&self) -> mpsc::Receiver<audio::Event> {
        mpsc::channel(1).1
    }
}

#[async_trait::async_trait]
impl NetworkOs for Unsupported {
    async fn read(&self) -> Result<network::Read, OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    async fn call(&self, _c: network::Call) -> Result<(), OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    fn changes(&self) -> mpsc::Receiver<network::Event> {
        mpsc::channel(1).1
    }
}

#[async_trait::async_trait]
impl NotificationsOs for Unsupported {
    async fn request(&self) -> Result<notifications::Ownership, OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    async fn call(&self, _c: notifications::Call) -> Result<(), OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    fn changes(&self) -> mpsc::Receiver<notifications::Event> {
        mpsc::channel(1).1
    }
}

#[async_trait::async_trait]
impl DisplaysOs for Unsupported {
    async fn read(&self) -> Result<displays::Read, OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    async fn call(&self, _c: displays::Call) -> Result<(), OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    fn changes(&self) -> mpsc::Receiver<displays::Event> {
        mpsc::channel(1).1
    }
}

#[async_trait::async_trait]
impl LockOs for Unsupported {
    async fn read(&self) -> Result<lock::Read, OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    async fn check_pin(&self, _pin: lock::Pin) -> Result<bool, OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    async fn call(&self, _c: lock::Call) -> Result<(), OsError> {
        Err(OsError::Unavailable(WHY.into()))
    }
    fn changes(&self) -> mpsc::Receiver<lock::Event> {
        mpsc::channel(1).1
    }
}
