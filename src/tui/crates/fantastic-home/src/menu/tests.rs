use super::*;
use crate::viewmodel::{Config, Event};
use crate::Mode;

/// A home whose registry holds `n` entries `g1…gn` (`last_seen` = their number)
/// with every one's page open, and `selected` shown.
fn home_with(n: usize, selected: Option<usize>) -> Home {
    let mut home = Home::new(Config::new(Mode::Lock));
    let entries = (1..=n)
        .map(|i| {
            (
                format!("f{i}"),
                format!("guid: g{i}\nname: N{i}\npath: /w/n{i}\nstored_at: {i}\nlast_seen: {i}\n"),
            )
        })
        .collect();
    home.reduce(Event::RegistryRead { now_ms: 1, entries });
    for i in 1..=n {
        home.reduce(Event::Select {
            id: Some(format!("g{i}")),
        });
    }
    home.reduce(Event::Select {
        id: selected.map(|i| format!("g{i}")),
    });
    home
}

fn find<'a>(menu: &'a Menu, id: &str) -> Option<&'a Item> {
    menu.items()
        .into_iter()
        .find(|i| matches!(i, Item::Action { id: x, .. } if x == id))
}

fn bar(menu: &Menu) -> Vec<&str> {
    menu.bar.iter().map(|s| s.label.as_str()).collect()
}

#[test]
fn the_default_menu_validates_on_every_os() {
    for home in [
        home_with(0, None),
        home_with(3, Some(2)),
        home_with(12, Some(11)),
    ] {
        let menu = home.menu();
        assert_eq!(menu.validate(), Ok(()));
        for os in Os::ALL {
            assert_eq!(menu.for_os(os).validate(), Ok(()), "{os:?}");
        }
    }
    let m = home_with(1, None).menu();
    assert_eq!(
        bar(&m.for_os(Os::Macos)),
        ["Aisixteen Fantastic", "File", "Edit", "View", "Window"]
    );
    assert_eq!(bar(&m.for_os(Os::Windows)), ["File", "View", "Window"]);
    assert_eq!(bar(&m.for_os(Os::Linux)), ["File", "View"]);
}

#[test]
fn pages_become_cmd_1_to_9_and_stop_at_nine() {
    let menu = home_with(12, None).menu();
    for n in 1..=9 {
        let Some(Item::Action {
            label,
            accelerator,
            send,
            enabled,
            ..
        }) = find(&menu, &format!("page:{n}"))
        else {
            panic!("no page:{n}");
        };
        assert_eq!(label, &format!("N{n}"));
        assert_eq!(
            accelerator.as_deref(),
            Some(format!("CmdOrCtrl+{n}").as_str())
        );
        assert_eq!(send, &Some(Intent::SelectPage { index: n - 1 }));
        assert!(enabled);
    }
    assert!(find(&menu, "page:10").is_none());
    let Some(Item::Action {
        accelerator, send, ..
    }) = find(&menu, "page:home")
    else {
        panic!("no page:home");
    };
    assert_eq!(accelerator.as_deref(), Some("CmdOrCtrl+0"));
    assert_eq!(send, &Some(Intent::Select { id: None }));
    // no pages: View holds Home, and no separator dangles under it on Linux
    let empty = home_with(0, None).menu().for_os(Os::Linux);
    let view = empty.bar.iter().find(|s| s.label == "View").expect("View");
    assert_eq!(view.items.len(), 1);
}

#[test]
fn close_page_is_enabled_only_with_a_selection() {
    let without = home_with(2, None).menu();
    assert!(matches!(
        find(&without, "close_page"),
        Some(Item::Action { enabled: false, send: None, accelerator: Some(a), .. }) if a == "CmdOrCtrl+W"
    ));
    let with = home_with(2, Some(2)).menu();
    assert!(matches!(
        find(&with, "close_page"),
        Some(Item::Action { enabled: true, send: Some(Intent::ClosePage { id }), .. }) if id == "g2"
    ));
}

#[test]
fn open_recent_is_newest_first_capped_at_ten_and_absent_when_empty() {
    let none = home_with(0, None).menu();
    assert!(!none
        .items()
        .iter()
        .any(|i| matches!(i, Item::Submenu(s) if s.label == "Open Recent")));
    let menu = home_with(12, None).menu();
    let recent = menu
        .items()
        .into_iter()
        .find_map(|i| match i {
            Item::Submenu(s) if s.label == "Open Recent" => Some(s),
            _ => None,
        })
        .expect("Open Recent");
    let ids: Vec<&str> = recent
        .items
        .iter()
        .map(|i| match i {
            Item::Action { id, .. } => id.as_str(),
            other => panic!("not an action: {other:?}"),
        })
        .collect();
    let want: Vec<String> = (3..=12).rev().map(|i| format!("recent:g{i}")).collect();
    assert_eq!(ids, want);
    assert!(matches!(
        &recent.items[0],
        Item::Action { send: Some(Intent::Open { paths }), label, .. } if paths == &["/w/n12"] && label == "N12"
    ));
}

#[test]
fn for_os_drops_items_marked_for_other_systems() {
    let menu = home_with(1, Some(1)).menu();
    let roles = |m: &Menu| -> Vec<Role> {
        m.items()
            .into_iter()
            .filter_map(|i| match i {
                Item::Role { role, .. } => Some(*role),
                _ => None,
            })
            .collect()
    };
    let win = menu.for_os(Os::Windows);
    assert_eq!(roles(&win), [Role::Quit, Role::Minimize, Role::Maximize]);
    assert!(roles(&menu.for_os(Os::Linux)).is_empty());
    let mac = roles(&menu.for_os(Os::Macos));
    assert!(mac.contains(&Role::BringAllToFront) && mac.contains(&Role::Copy));
    // the Windows quit and its separator are gone on macOS; the app menu's quit stays
    assert_eq!(mac.iter().filter(|r| **r == Role::Quit).count(), 1);
    // nested menus are filtered too
    let nested = Menu {
        version: MENU_VERSION,
        bar: vec![menu_of(vec![Item::Submenu(Submenu {
            label: "Inner".into(),
            os: vec![],
            system: None,
            items: vec![role(Role::Copy, MAC), sep(WIN)],
        })])],
    };
    let Item::Submenu(inner) = &nested.for_os(Os::Linux).bar[0].items[0] else {
        panic!("a submenu")
    };
    assert!(inner.items.is_empty());
}

fn menu_of(items: Vec<Item>) -> Submenu {
    Submenu {
        label: "M".into(),
        os: vec![],
        system: None,
        items,
    }
}

fn open_action(id: &str, accel: Option<&str>) -> Item {
    action(
        id.into(),
        "L".into(),
        accel.map(str::to_string),
        Some(Intent::Open {
            paths: vec!["/a".into()],
        }),
    )
}

#[test]
fn validate_refuses() {
    let ok = Menu {
        version: MENU_VERSION,
        bar: vec![menu_of(vec![open_action("a", Some("CmdOrCtrl+A"))])],
    };
    assert_eq!(ok.validate(), Ok(()));

    let wrong_version = Menu {
        version: 2,
        ..ok.clone()
    };
    let duplicate = Menu {
        version: MENU_VERSION,
        bar: vec![menu_of(vec![
            open_action("a", None),
            open_action("a", None),
        ])],
    };
    let no_send = Menu {
        version: MENU_VERSION,
        bar: vec![menu_of(vec![Item::Action {
            id: "a".into(),
            label: "L".into(),
            accelerator: None,
            enabled: true,
            send: None,
            pick: None,
            os: vec![],
        }])],
    };
    let pick_other = Menu {
        version: MENU_VERSION,
        bar: vec![menu_of(vec![Item::Action {
            id: "a".into(),
            label: "L".into(),
            accelerator: None,
            enabled: true,
            send: Some(Intent::Select { id: None }),
            pick: Some(Pick::Folders),
            os: vec![],
        }])],
    };
    let help = |label: &str| Submenu {
        label: label.into(),
        os: vec![],
        system: Some(System::Help),
        items: vec![],
    };
    let two_helps = Menu {
        version: MENU_VERSION,
        bar: vec![help("Help"), help("More help")],
    };
    let doubled = Menu {
        version: MENU_VERSION,
        bar: vec![menu_of(vec![
            open_action("a", Some("CmdOrCtrl+K")),
            open_action("b", Some("CmdOrCtrl+K")),
        ])],
    };
    let empty_label = Menu {
        version: MENU_VERSION,
        bar: vec![Submenu {
            label: String::new(),
            ..menu_of(vec![])
        }],
    };
    for (menu, says) in [
        (wrong_version, "version 2"),
        (duplicate, "used twice"),
        (no_send, "sends nothing"),
        (pick_other, "must send open"),
        (two_helps, "Help menu"),
        (doubled, "share CmdOrCtrl+K"),
        (empty_label, "empty label"),
    ] {
        let err = menu.validate().expect_err(says);
        assert!(err.contains(says), "{err} names {says}");
    }
}

#[test]
fn an_accelerator_on_disjoint_systems_is_not_doubled() {
    let on_os = |id: &str, os: Os| match open_action(id, Some("CmdOrCtrl+K")) {
        Item::Action {
            id,
            label,
            accelerator,
            enabled,
            send,
            pick,
            ..
        } => Item::Action {
            id,
            label,
            accelerator,
            enabled,
            send,
            pick,
            os: vec![os],
        },
        _ => unreachable!(),
    };
    let menu = Menu {
        version: MENU_VERSION,
        bar: vec![menu_of(vec![
            on_os("a", Os::Macos),
            on_os("b", Os::Windows),
        ])],
    };
    assert_eq!(menu.validate(), Ok(()));
}

#[test]
fn menu_spec_round_trips_through_json() {
    let msg = MenuSpecMsg {
        seq: 7,
        spec: home_with(3, Some(1)).menu(),
    };
    let text = serde_json::to_string(&msg).expect("serializes");
    let back: MenuSpecMsg = serde_json::from_str(&text).expect("parses");
    assert_eq!(back, msg);
    let value: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert_eq!(value["spec"]["version"], 1);
    let file = &value["spec"]["bar"][1];
    assert_eq!(file["label"], "File");
    assert_eq!(
        file["items"][0],
        serde_json::json!({"kind": "action", "id": "open_folder", "label": "Open Folder…",
            "accelerator": "CmdOrCtrl+O", "enabled": true, "send": {"type": "open", "paths": []},
            "pick": "folders", "os": []})
    );
    assert_eq!(file["items"][1]["kind"], "submenu");
    assert_eq!(value["spec"]["bar"][4]["system"], "window");
    // an unknown field in a menu is refused
    let bad = r#"{"seq":1,"spec":{"version":1,"bar":[{"label":"M","items":[],"colour":"red"}]}}"#;
    assert!(serde_json::from_str::<MenuSpecMsg>(bad).is_err());
}
