//! The fields a `web` agent's record carries, beside the ones every record has
//! (issue #642).

use schemars::JsonSchema;

/// The fields a web host's record carries.
#[derive(Debug, JsonSchema)]
pub struct Record {
    /// The TCP port to listen on; required to boot (no default).
    #[schemars(range(min = 1))]
    pub port: Option<u16>,
    /// The name the Host/Origin guard knows this host by; loopback when absent.
    #[schemars(length(min = 1))]
    pub host: Option<String>,
    /// The address the listener binds when it differs from `host` (`0.0.0.0` publishes the host).
    #[schemars(length(min = 1))]
    pub bind: Option<String>,
    /// Further browser origins (exact strings) a page may call this host from.
    pub allowed_origins: Option<Vec<String>>,
}

fantastic_kernel::record!(Record);
