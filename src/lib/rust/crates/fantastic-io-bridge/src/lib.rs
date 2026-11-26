//! fantastic-io-bridge — the IO base. A **shared library, NOT a bundle** (no agent,
//! no entry point): the authorization rule registries of the WIRE legs (`ws_bridge`,
//! `relay_connector`, `web_ws`, `web_rest`), the door an EXTERNAL client (an HTTP/WS
//! client, a remote kernel) passes. It never judges an in-kernel agent's call: that
//! is the kernel's `can` lines alone. Plus the frame codec and permessage-deflate.

pub mod authorizer;
pub mod codec;
pub mod inbound;
pub mod ws_deflate;

