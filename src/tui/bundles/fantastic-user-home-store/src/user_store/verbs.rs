//! The verbs the user store declares (issue #642): one struct per verb, then `verbs!` —
//! the list the agent's own entry checks every call against, and that `reflect` and
//! `docs` render. `reflect` is declared once, in the kernel.

use schemars::JsonSchema;
use serde::Deserialize;

/// The kernel starts the store. With the switch on, this fantastic's folder is written again (a home that cannot be written answers its refusal); off, a folder of this guid left behind is removed. Answers {stored} | {unstored:true} | nothing.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// The kernel stops the store, at a clean stop: with the switch on, this fantastic's folder gets pid and canvas null. Returns {stopped}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {
    /// Why it stops.
    pub reason: Option<String>,
}

/// What the home holds. Returns {home, here:{path, guid, name, folder, stored, register_home_folder}, keys:[{ref, host}] (never values), saved:[name], locations:[{folder, guid, name, path, pid, canvas, stored_at, last_seen, here, exists}]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct List {}

/// The switch: on records it and writes this fantastic's folder (`<home>/fantastics/<name or guid>/fantastic.yaml`), written again on every start while on; off records it and removes the folder. Off until switched on. Returns {register_home_folder, stored | unstored}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RegisterHomeFolder {
    /// `true` registers, `false` unregisters.
    pub value: bool,
}

/// Record the folder's name; with the switch on the folder is renamed now. Returns {name, folder, stored?}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NameHomeFolder {
    /// Letters, digits, '-' and '_', starting with a letter or digit, at most 64; absent, the guid.
    #[schemars(length(min = 1), regex(pattern = "^[A-Za-z0-9][A-Za-z0-9_-]{0,63}$"))]
    pub name: Option<String>,
}

/// The canvas this run serves, written into the folder when the switch is on. Returns {canvas, stored?}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetCanvas {
    /// The canvas URL; absent, none is served.
    #[schemars(length(min = 1))]
    pub url: Option<String>,
}

/// Remove another fantastic's folder (this one follows `register_home_folder`); a folder holding anything else is refused. Returns {unstored}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Unstore {
    /// The other fantastic's guid.
    #[schemars(length(min = 1))]
    pub guid: String,
}

/// Remove every fantastic's folder but this one's. Returns {unstored:[guid]}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnstoreOthers {}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "shutdown" => Shutdown,
    "list" => List,
    "register_home_folder" => RegisterHomeFolder,
    "name_home_folder" => NameHomeFolder,
    "set_canvas" => SetCanvas,
    "unstore" => Unstore,
    "unstore_others" => UnstoreOthers,
}
