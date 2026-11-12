//! The verbs the `home` agent declares (issue #642): one struct per verb, then `verbs!` —
//! the list the agent's own entry checks every call against, and that `reflect` and
//! `docs` render. `reflect` is declared once, in the kernel.
//!
//! Every client verb answers the state right after its event.

use schemars::JsonSchema;
use serde::Deserialize;

/// One client verb: its arguments.
macro_rules! client_verb {
    ($(#[$m:meta])* $name:ident { $( $(#[$fm:meta])* $f:ident : $t:ty ),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Deserialize, JsonSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            $( $(#[$fm])* pub $f: $t, )*
        }
    };
}

/// The kernel starts the home: restores the kept pages and selection from this agent's record, then starts the poll (every 2 s). Idempotent. Refuses a malformed `pages` or `selection` on the record.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Boot {}

/// The kernel stops the home at a clean stop: the poll stops; the pages and selection are already on the record. Returns {stopped:true}.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Shutdown {
    /// Why it stops.
    pub reason: Option<String>,
}

client_verb! {
    /// The state: the rows, the open pages, the selection and the problem.
    List {}
}

client_verb! {
    /// Open each folder's page, the last one shown: a listed row's path (its state folder) as it is, else an absolute work folder (its state folder is `<folder>/.fantastic`); a stopped one is started, an unlisted one is started and added to the list. Returns the state. Refuses a path that is not absolute, not a folder, or the home's own folder.
    Open {
        /// The folders to open: each an absolute path.
        #[schemars(length(min = 1), inner(length(min = 1), regex(pattern = r"^(?:/|[A-Za-z]:[\\/]|\\\\)")))]
        paths: Vec<String>,
    }
}

client_verb! {
    /// Add an absolute folder the list does not hold, as `open` does. Returns the state. Refuses a path that is not absolute, not a folder, the home's own folder, or listed already.
    Register {
        /// The folder: an absolute path.
        #[schemars(length(min = 1), regex(pattern = r"^(?:/|[A-Za-z]:[\\/]|\\\\)"))]
        path: String,
    }
}

client_verb! {
    /// Start the stopped fantastic `id` (`fantastic canvas --statedir <its path>`, run in the work folder its root record names). Returns the state. Refuses an unknown id, or one whose actions lack start.
    Start {
        /// The row's id.
        #[schemars(length(min = 1))]
        id: String,
    }
}

client_verb! {
    /// Ask the process holding the folder of `id` to stop. Returns the state. Refuses an unknown id, or one whose actions lack stop.
    Stop {
        /// The row's id.
        #[schemars(length(min = 1))]
        id: String,
    }
}

client_verb! {
    /// Remove the registry row `id` from the list, through its running fantastic's kernel (or a running one's, when it is stopped). Returns the state. Refuses an unknown id, or one whose actions lack remove.
    Remove {
        /// The row's id.
        #[schemars(length(min = 1))]
        id: String,
    }
}

client_verb! {
    /// Show the page of row `id` (opened if it is not), or the Home when no id is given. Returns the state. Refuses an unknown id.
    Select {
        /// The row's id; absent, the Home.
        #[schemars(length(min = 1))]
        id: Option<String>,
    }
}

client_verb! {
    /// Show the open page at `index`. Returns the state. Refuses an index past the last page.
    SelectPage {
        /// The page's position, from 0.
        index: u64,
    }
}

client_verb! {
    /// Close the open page `id`; its fantastic keeps running. Returns the state. Refuses a page that is not open.
    ClosePage {
        /// The page's id.
        #[schemars(length(min = 1))]
        id: String,
    }
}

client_verb! {
    /// Clear the problem. Returns the state.
    Dismiss {}
}

client_verb! {
    /// The launch log file of row `id` and its last lines. Returns {id, file, tail} (tail null when unreadable). Refuses an unknown id, or one with no launch log.
    Log {
        /// The row's id.
        #[schemars(length(min = 1))]
        id: String,
    }
}

client_verb! {
    /// Emit {type:"focus"} on home for the host shell (raise its window); nothing else changes. Returns the state.
    Focus {}
}

client_verb! {
    /// The native menus a host shell builds, at the state's seq. The same payload is emitted on home after every state emit that changed it. Returns {type:"menu_spec", seq, spec}.
    Menu {}
}

fantastic_kernel::verbs! {
    "boot" => Boot,
    "shutdown" => Shutdown,
    "list" => List,
    "open" => Open,
    "register" => Register,
    "start" => Start,
    "stop" => Stop,
    "remove" => Remove,
    "select" => Select,
    "select_page" => SelectPage,
    "close_page" => ClosePage,
    "dismiss" => Dismiss,
    "log" => Log,
    "focus" => Focus,
    "menu" => Menu,
}

/// What a page reaches through `user` (the grant follows this list): every declared
/// verb but the lifecycle (`boot`, `shutdown`), then `reflect`.
#[must_use]
pub fn page_verbs() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = VERBS
        .iter()
        .map(|v| v.name)
        .filter(|n| !matches!(*n, "boot" | "shutdown"))
        .collect();
    names.push("reflect");
    names
}
