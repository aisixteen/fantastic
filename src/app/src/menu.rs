//! THE MENUS, built from the spec the `home` agent serves
//! (`fantastic_home::menu`): no menu content lives in the shell.
//!
//! ```text
//!   menu_spec ──validate · for_os(MENU_OS) · roles──▶ build ──▶ app.set_menu
//!   a click ──Clicks──▶ (ask_folders) ──▶ home.call(send)
//! ```
//!
//! A broken spec is fatal (the caller's): the home served what no shell can build.
//! The kiosk applies no menu at all.

use std::collections::HashMap;
use std::sync::Mutex;

use fantastic_home::menu::{Item, Menu as Spec, MenuSpecMsg, Os, Pick, Role, Submenu as SpecMenu};
use fantastic_home::Intent;
use serde_json::Value;
use tauri::menu::{Menu, MenuItemBuilder, PredefinedMenuItem, Submenu};
use tauri::{AppHandle, Wry};

use crate::{home, intents, sys};

/// What each action item does: its `send` and its `pick`, by id.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Clicks(HashMap<String, (Option<Intent>, Option<Pick>)>);

impl Clicks {
    /// Every action of `spec`, nested menus walked.
    pub fn of(spec: &Spec) -> Clicks {
        fn walk(items: &[Item], out: &mut HashMap<String, (Option<Intent>, Option<Pick>)>) {
            for item in items {
                match item {
                    Item::Action { id, send, pick, .. } => {
                        out.insert(id.clone(), (send.clone(), *pick));
                    }
                    Item::Submenu(s) => walk(&s.items, out),
                    Item::Role { .. } | Item::Separator { .. } => {}
                }
            }
        }
        let mut out = HashMap::new();
        for s in &spec.bar {
            walk(&s.items, &mut out);
        }
        Clicks(out)
    }

    pub fn get(&self, id: &str) -> Option<&(Option<Intent>, Option<Pick>)> {
        self.0.get(id)
    }
}

/// The spec applied now, and what its items do.
#[derive(Default)]
pub struct Applied {
    /// Held while one spec is checked, built and set: specs apply one at a time, in seq order.
    applying: tokio::sync::Mutex<Option<(u64, Spec)>>,
    clicks: Mutex<Clicks>,
}

/// Can `os` build a native `role` (muda 0.20 `items/predefined.rs`)? GTK 3 has
/// no undo, redo, hide, minimize, maximize or quit; muda performs cut, copy,
/// paste and select-all on GTK through libxdo, which tauri links without — such
/// an item would take the key from the webview and do nothing.
pub fn supported(role: Role, os: Os) -> bool {
    match role {
        Role::About => true,
        Role::Undo
        | Role::Redo
        | Role::Hide
        | Role::Minimize
        | Role::Maximize
        | Role::Quit
        | Role::Cut
        | Role::Copy
        | Role::Paste
        | Role::SelectAll => os != Os::Linux,
        Role::Services
        | Role::HideOthers
        | Role::ShowAll
        | Role::BringAllToFront
        | Role::Fullscreen => os == Os::Macos,
    }
}

/// Every role `spec` (already for `os`) holds is one `os` can build.
pub fn check_roles(spec: &Spec, os: Os) -> Result<(), String> {
    fn walk(items: &[Item], os: Os) -> Result<(), String> {
        for item in items {
            match item {
                Item::Role { role, .. } if !supported(*role, os) => {
                    return Err(format!(
                        "menu: the role {role:?} has no native item on {os:?}"
                    ))
                }
                Item::Submenu(s) => walk(&s.items, os)?,
                _ => {}
            }
        }
        Ok(())
    }
    spec.bar.iter().try_for_each(|s| walk(&s.items, os))
}

/// Apply a `menu_spec` payload: refused unless it validates and every role is
/// this OS's; ignored unless its seq is newer than the applied one; set only
/// when what this OS shows changed.
pub async fn apply(app: &AppHandle, payload: Value) -> Result<(), String> {
    let msg: MenuSpecMsg =
        serde_json::from_value(payload).map_err(|e| format!("menu_spec: {e}"))?;
    let shell = home::shell(app);
    let mut applied = shell.menu.applying.lock().await;
    if applied.as_ref().is_some_and(|(seq, _)| msg.seq <= *seq) {
        return Ok(());
    }
    msg.spec.validate()?;
    let spec = msg.spec.for_os(sys::MENU_OS);
    check_roles(&spec, sys::MENU_OS)?;
    if applied.as_ref().is_some_and(|(_, shown)| *shown == spec) {
        *applied = Some((msg.seq, spec));
        return Ok(());
    }
    let (menu, systems) = build(app, &spec)?;
    app.set_menu(menu).map_err(|e| format!("menu: {e}"))?;
    for (submenu, system) in systems {
        sys::system_menu(&submenu, system).map_err(|e| format!("menu: {e}"))?;
    }
    *shell.menu.clicks.lock().expect("the clicks are poisoned") = Clicks::of(&spec);
    *applied = Some((msg.seq, spec));
    Ok(())
}

/// The submenus that are NSApp's Window or Help menu, set once the menu is.
pub type Systems = Vec<(Submenu<Wry>, fantastic_home::menu::System)>;

/// The native menu for `spec` (already for this OS), and its system menus.
pub fn build(app: &AppHandle, spec: &Spec) -> Result<(Menu<Wry>, Systems), String> {
    let menu = Menu::new(app).map_err(err)?;
    let mut systems = Vec::new();
    for s in &spec.bar {
        let sub = submenu(app, s, &mut systems)?;
        menu.append(&sub).map_err(err)?;
    }
    Ok((menu, systems))
}

fn err(e: tauri::Error) -> String {
    format!("menu: {e}")
}

fn submenu(app: &AppHandle, s: &SpecMenu, systems: &mut Systems) -> Result<Submenu<Wry>, String> {
    let sub = Submenu::new(app, &s.label, true).map_err(err)?;
    for item in &s.items {
        match item {
            Item::Action {
                id,
                label,
                accelerator,
                enabled,
                ..
            } => {
                let mut b = MenuItemBuilder::with_id(id.as_str(), label).enabled(*enabled);
                if let Some(a) = accelerator {
                    b = b.accelerator(a);
                }
                let built = b.build(app).map_err(|e| format!("menu: \"{id}\": {e}"))?;
                sub.append(&built).map_err(err)?;
            }
            Item::Role { role, .. } => sub.append(&predefined(app, *role)?).map_err(err)?,
            Item::Separator { .. } => sub
                .append(&PredefinedMenuItem::separator(app).map_err(err)?)
                .map_err(err)?,
            Item::Submenu(inner) => sub.append(&submenu(app, inner, systems)?).map_err(err)?,
        }
    }
    if let Some(system) = s.system {
        systems.push((sub.clone(), system));
    }
    Ok(sub)
}

fn predefined(app: &AppHandle, role: Role) -> Result<PredefinedMenuItem<Wry>, String> {
    let item = match role {
        Role::About => PredefinedMenuItem::about(app, None, None),
        Role::Services => PredefinedMenuItem::services(app, None),
        Role::Hide => PredefinedMenuItem::hide(app, None),
        Role::HideOthers => PredefinedMenuItem::hide_others(app, None),
        Role::ShowAll => PredefinedMenuItem::show_all(app, None),
        Role::Quit => PredefinedMenuItem::quit(app, None),
        Role::Undo => PredefinedMenuItem::undo(app, None),
        Role::Redo => PredefinedMenuItem::redo(app, None),
        Role::Cut => PredefinedMenuItem::cut(app, None),
        Role::Copy => PredefinedMenuItem::copy(app, None),
        Role::Paste => PredefinedMenuItem::paste(app, None),
        Role::SelectAll => PredefinedMenuItem::select_all(app, None),
        Role::Minimize => PredefinedMenuItem::minimize(app, None),
        Role::Maximize => PredefinedMenuItem::maximize(app, None),
        Role::Fullscreen => PredefinedMenuItem::fullscreen(app, None),
        Role::BringAllToFront => PredefinedMenuItem::bring_all_to_front(app, None),
    };
    item.map_err(err)
}

/// `RunEvent::MenuEvent`: the item's intent, after asking for folders when it
/// picks. A role's click is handled natively and never reaches here as an action.
pub fn clicked(app: &AppHandle, id: &str) {
    let shell = home::shell(app);
    let Some((Some(send), pick)) = shell
        .menu
        .clicks
        .lock()
        .expect("the clicks are poisoned")
        .get(id)
        .cloned()
    else {
        return;
    };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match pick {
            Some(Pick::Folders) => match intents::ask_folders(&app).await {
                Ok(paths) if paths.is_empty() => {}
                Ok(paths) => home::send(&app, Intent::Open { paths }).await,
                Err(e) => home::warn(&app, &e),
            },
            None => home::send(&app, send).await,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use fantastic_home::menu::{System, MENU_VERSION};
    use fantastic_home::{Config, Home, Mode};

    #[test]
    fn the_role_table_passes_the_default_menu_on_every_os() {
        let spec = Home::new(Config::new(Mode::Lock)).menu();
        for os in Os::ALL {
            assert_eq!(check_roles(&spec.for_os(os), os), Ok(()), "{os:?}");
        }
    }

    #[test]
    fn a_role_the_os_cannot_build_is_refused() {
        let spec = Spec {
            version: MENU_VERSION,
            bar: vec![SpecMenu {
                label: "Edit".into(),
                os: vec![],
                system: Some(System::Window),
                items: vec![Item::Role {
                    role: Role::Copy,
                    os: vec![],
                }],
            }],
        };
        assert_eq!(check_roles(&spec, Os::Macos), Ok(()));
        assert_eq!(check_roles(&spec, Os::Windows), Ok(()));
        let e = check_roles(&spec, Os::Linux).expect_err("no copy on GTK");
        assert!(e.contains("Copy") && e.contains("Linux"), "{e}");
        assert!(supported(Role::About, Os::Linux));
        assert!(!supported(Role::Quit, Os::Linux));
        assert!(supported(Role::Quit, Os::Windows));
        assert!(!supported(Role::Services, Os::Windows));
        assert!(supported(Role::BringAllToFront, Os::Macos));
    }

    #[test]
    fn clicks_give_an_items_send_and_its_pick() {
        let spec = Home::new(Config::new(Mode::Lock)).menu();
        let clicks = Clicks::of(&spec);
        assert_eq!(
            clicks.get("open_folder"),
            Some(&(Some(Intent::Open { paths: vec![] }), Some(Pick::Folders)))
        );
        assert_eq!(
            clicks.get("page:home"),
            Some(&(Some(Intent::Select { id: None }), None))
        );
        assert_eq!(clicks.get("close_page"), Some(&(None, None)));
        assert_eq!(clicks.get("quit"), None);
    }
}
