//! Every OS but Linux: the agents exist where a record names them, and say
//! they are not available.

use fantastic_system::power;
use tokio::sync::mpsc;

use super::{OsError, PowerOs};

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
