//! THE FANTASTIC MANAGER'S MODEL AND VIEWMODEL: everything `fantastic home`
//! knows about the fantastics on this machine, as plain data.
//!
//! ```text
//!   model      the registry entries and a folder's run file, parsed from text
//!   probe      what one fantastic looks like, asked one question at a time
//!   viewmodel  the home: events in, effects out, rows to draw
//!   intent     the user's intents, typed: the home's verbs on the wire
//!   menu       the menu spec a host shell builds its native menus from
//! ```
//!
//! PURE. Nothing here reads a file, a socket, a process, the clock or the
//! environment. Text arrives as `&str`, time arrives on events as `now_ms`, and
//! every question for the machine leaves as data (an `Ask` from a `Prober`, an
//! `Effect` from `Home::reduce`) for the OS layer to answer with the matching
//! `Answer` or `Event`. `tests/purity.rs` scans the sources to keep it so.
//!
//! NOTHING IS GUESSED. What does not parse is `None`, a field that is not what
//! it must be is not read as something else, and a refusal is a `problem` that
//! says what and why.

pub mod intent;
pub mod menu;
pub mod model;
pub mod probe;
pub mod viewmodel;

pub use intent::Intent;
pub use model::{registered, Canvas, CanvasRun, Registered, RUN_FILE};
pub use probe::{is_active, Answer, Ask, Mode, Probe, ProbeError, Prober, Status, Target};
pub use viewmodel::{Config, Demotion, Effect, Event, Home, Job, Row};

/// The README's example, compiled and run as a doctest.
#[doc = include_str!("../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;
