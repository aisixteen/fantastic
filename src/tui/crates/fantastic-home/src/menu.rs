//! THE MENU SPEC: the native menus a host shell builds, as data the home serves.
//!
//! ```text
//!   Home::menu()          the default content, from the registry, the pages and the selection
//!   Menu::validate()      what a shell may trust before it builds one
//!   Menu::for_os(os)      what one OS shows
//! ```
//!
//! The `home` agent answers `menu` with `{"type":"menu_spec","seq":…,"spec":…}`
//! ([`MenuSpecMsg`]) and emits the same after every state emit that changed it.
//! A shell builds native items from it and, on a click, sends the item's
//! [`Intent`] back to the home (after asking the OS for folders, with `pick`).
//! No logic lives in the shell: what the menu holds and what it does is here.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::intent::Intent;
use crate::viewmodel::Home;

/// The spec's version; a shell refuses any other.
pub const MENU_VERSION: u32 = 1;

/// How many entries File ▸ Open Recent lists.
pub const RECENT_CAP: usize = 10;

/// How many pages View lists (⌘1 … ⌘9).
pub const PAGE_KEYS: usize = 9;

/// The payload `type` of a spec on the wire.
pub const MENU_SPEC: &str = "menu_spec";

/// A spec on the wire: `{"type":"menu_spec","seq":…,"spec":…}`. `seq` is the
/// home's state seq when it was made; a shell applies only a newer one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MenuSpecMsg {
    pub seq: u64,
    pub spec: Menu,
}

/// The menu bar.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Menu {
    pub version: u32,
    pub bar: Vec<Submenu>,
}

/// One menu of the bar, or a nested one.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Submenu {
    pub label: String,
    /// The systems that show it; empty: every one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<Os>,
    /// macOS: this is NSApp's Window or Help menu.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<System>,
    pub items: Vec<Item>,
}

/// One item of a menu.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    /// An item the home handles: a click sends `send`.
    Action {
        /// Unique in the whole spec.
        id: String,
        label: String,
        /// muda's grammar: `"CmdOrCtrl+O"`.
        #[serde(default)]
        accelerator: Option<String>,
        enabled: bool,
        /// Required when enabled.
        #[serde(default)]
        send: Option<Intent>,
        /// Ask the OS first; only with `send = open {paths: []}`, whose paths
        /// are then the ones chosen.
        #[serde(default)]
        pick: Option<Pick>,
        #[serde(default)]
        os: Vec<Os>,
    },
    /// A native item the OS handles: copy, quit, …
    Role {
        role: Role,
        #[serde(default)]
        os: Vec<Os>,
    },
    Separator {
        #[serde(default)]
        os: Vec<Os>,
    },
    /// A nested menu (Open Recent).
    Submenu(Submenu),
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Os {
    Macos,
    Windows,
    Linux,
}

impl Os {
    pub const ALL: [Os; 3] = [Os::Macos, Os::Windows, Os::Linux];
}

/// NSApp's own menus on macOS.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum System {
    Window,
    Help,
}

/// What the shell asks the OS for before it sends.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Pick {
    Folders,
}

/// A native item.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    About,
    Services,
    Hide,
    HideOthers,
    ShowAll,
    Quit,
    Undo,
    Redo,
    Cut,
    Copy,
    Paste,
    SelectAll,
    Minimize,
    Maximize,
    Fullscreen,
    BringAllToFront,
}

/// Shown on `os`: an empty list is every system.
fn on(os: &[Os], which: Os) -> bool {
    os.is_empty() || os.contains(&which)
}

impl Item {
    fn os(&self) -> &[Os] {
        match self {
            Item::Action { os, .. } | Item::Role { os, .. } | Item::Separator { os } => os,
            Item::Submenu(s) => &s.os,
        }
    }
}

impl Submenu {
    fn for_os(&self, which: Os) -> Submenu {
        Submenu {
            label: self.label.clone(),
            os: self.os.clone(),
            system: self.system,
            items: self
                .items
                .iter()
                .filter(|i| on(i.os(), which))
                .map(|i| match i {
                    Item::Submenu(s) => Item::Submenu(s.for_os(which)),
                    other => other.clone(),
                })
                .collect(),
        }
    }

    /// Every item, nested menus walked, depth first.
    fn walk<'a>(&'a self, out: &mut Vec<&'a Item>) {
        for item in &self.items {
            out.push(item);
            if let Item::Submenu(s) = item {
                s.walk(out);
            }
        }
    }
}

impl Menu {
    /// What `which` shows: every menu and item marked for other systems dropped.
    pub fn for_os(&self, which: Os) -> Menu {
        Menu {
            version: self.version,
            bar: self
                .bar
                .iter()
                .filter(|s| on(&s.os, which))
                .map(|s| s.for_os(which))
                .collect(),
        }
    }

    fn items(&self) -> Vec<&Item> {
        let mut out = Vec::new();
        for s in &self.bar {
            s.walk(&mut out);
        }
        out
    }

    fn submenus(&self) -> Vec<&Submenu> {
        let mut out: Vec<&Submenu> = self.bar.iter().collect();
        out.extend(self.items().into_iter().filter_map(|i| match i {
            Item::Submenu(s) => Some(s),
            _ => None,
        }));
        out
    }

    /// The checks a shell relies on: the version; unique action ids and
    /// non-empty labels; `enabled` implies `send`; `pick: folders` only with
    /// `send = open {paths: []}`; at most one Window and one Help menu; no two
    /// items one OS shows share an accelerator.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != MENU_VERSION {
            return Err(format!(
                "menu: version {} (this shell reads {MENU_VERSION})",
                self.version
            ));
        }
        for s in self.submenus() {
            if s.label.is_empty() {
                return Err("menu: a menu has an empty label".into());
            }
        }
        let mut ids = BTreeSet::new();
        for item in self.items() {
            let Item::Action {
                id,
                label,
                enabled,
                send,
                pick,
                ..
            } = item
            else {
                continue;
            };
            if id.is_empty() {
                return Err(format!("menu: the item \"{label}\" has an empty id"));
            }
            if !ids.insert(id.as_str()) {
                return Err(format!("menu: the id \"{id}\" is used twice"));
            }
            if label.is_empty() {
                return Err(format!("menu: the item \"{id}\" has an empty label"));
            }
            if *enabled && send.is_none() {
                return Err(format!("menu: \"{id}\" is enabled and sends nothing"));
            }
            if pick.is_some() && *send != Some(Intent::Open { paths: Vec::new() }) {
                return Err(format!(
                    "menu: \"{id}\" picks folders, so it must send open {{paths: []}}"
                ));
            }
        }
        for system in [System::Window, System::Help] {
            let n = self.bar.iter().filter(|s| s.system == Some(system)).count()
                + self
                    .items()
                    .iter()
                    .filter(|i| matches!(i, Item::Submenu(s) if s.system == Some(system)))
                    .count();
            if n > 1 {
                return Err(format!("menu: {n} menus are the {system:?} menu"));
            }
        }
        for which in Os::ALL {
            let mut keys: BTreeMap<&str, &str> = BTreeMap::new();
            let shown = self.for_os(which);
            for item in shown.items() {
                if let Item::Action {
                    id,
                    accelerator: Some(a),
                    ..
                } = item
                {
                    if let Some(other) = keys.insert(a.as_str(), id.as_str()) {
                        return Err(format!(
                            "menu: \"{other}\" and \"{id}\" share {a} on {which:?}"
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

// ── the default content ────────────────────────────────────────────────────

fn action(id: String, label: String, accelerator: Option<String>, send: Option<Intent>) -> Item {
    Item::Action {
        id,
        label,
        accelerator,
        enabled: send.is_some(),
        send,
        pick: None,
        os: Vec::new(),
    }
}

fn role(role: Role, os: &[Os]) -> Item {
    Item::Role {
        role,
        os: os.to_vec(),
    }
}

fn sep(os: &[Os]) -> Item {
    Item::Separator { os: os.to_vec() }
}

fn menu(label: &str, os: &[Os], system: Option<System>, items: Vec<Item>) -> Submenu {
    Submenu {
        label: label.into(),
        os: os.to_vec(),
        system,
        items,
    }
}

const MAC: &[Os] = &[Os::Macos];
const WIN: &[Os] = &[Os::Windows];
const ALL: &[Os] = &[];

impl Home {
    /// The menu bar for this home now: the app menu (macOS), File (Open Folder,
    /// Open Recent, Close Page), Edit (macOS), View (Home, the first nine pages)
    /// and Window.
    pub fn menu(&self) -> Menu {
        let app = menu(
            "Aisixteen Fantastic",
            MAC,
            None,
            vec![
                role(Role::About, ALL),
                sep(ALL),
                role(Role::Services, ALL),
                sep(ALL),
                role(Role::Hide, ALL),
                role(Role::HideOthers, ALL),
                role(Role::ShowAll, ALL),
                sep(ALL),
                role(Role::Quit, ALL),
            ],
        );

        let mut file = vec![Item::Action {
            id: "open_folder".into(),
            label: "Open Folder…".into(),
            accelerator: Some("CmdOrCtrl+O".into()),
            enabled: true,
            send: Some(Intent::Open { paths: Vec::new() }),
            pick: Some(Pick::Folders),
            os: Vec::new(),
        }];
        let mut recent: Vec<_> = self.entries().iter().collect();
        // newest first; a tie keeps the registry's order
        recent.sort_by_key(|r| std::cmp::Reverse(r.last_seen));
        let recent: Vec<Item> = recent
            .into_iter()
            .take(RECENT_CAP)
            .map(|reg| {
                action(
                    format!("recent:{}", reg.guid),
                    reg.display_name(),
                    None,
                    Some(Intent::Open {
                        paths: vec![reg.path.clone()],
                    }),
                )
            })
            .collect();
        if !recent.is_empty() {
            file.push(Item::Submenu(menu("Open Recent", ALL, None, recent)));
        }
        file.push(sep(ALL));
        file.push(action(
            "close_page".into(),
            "Close Page".into(),
            Some("CmdOrCtrl+W".into()),
            self.selection()
                .map(|id| Intent::ClosePage { id: id.to_string() }),
        ));
        file.push(sep(WIN));
        file.push(role(Role::Quit, WIN));

        let edit = menu(
            "Edit",
            MAC,
            None,
            vec![
                role(Role::Undo, ALL),
                role(Role::Redo, ALL),
                sep(ALL),
                role(Role::Cut, ALL),
                role(Role::Copy, ALL),
                role(Role::Paste, ALL),
                role(Role::SelectAll, ALL),
            ],
        );

        let mut view = vec![action(
            "page:home".into(),
            "Home".into(),
            Some("CmdOrCtrl+0".into()),
            Some(Intent::Select { id: None }),
        )];
        let pages: Vec<Item> = self
            .pages()
            .iter()
            .take(PAGE_KEYS)
            .enumerate()
            .map(|(i, id)| {
                let n = i + 1;
                action(
                    format!("page:{n}"),
                    self.name_of_id(id),
                    Some(format!("CmdOrCtrl+{n}")),
                    Some(Intent::SelectPage { index: i }),
                )
            })
            .collect();
        if !pages.is_empty() {
            view.push(sep(ALL));
            view.extend(pages);
        }
        view.push(sep(MAC));
        view.push(role(Role::Fullscreen, MAC));

        let window = menu(
            "Window",
            &[Os::Macos, Os::Windows],
            Some(System::Window),
            vec![
                role(Role::Minimize, ALL),
                role(Role::Maximize, ALL),
                role(Role::BringAllToFront, MAC),
            ],
        );

        Menu {
            version: MENU_VERSION,
            bar: vec![
                app,
                menu("File", ALL, None, file),
                edit,
                menu("View", ALL, None, view),
                window,
            ],
        }
    }
}

#[cfg(test)]
mod tests;
