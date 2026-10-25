//! `sys::linux::network` against a fake bus: python-dbusmock's
//! `networkmanager` template on a private daemon, driven through the real
//! bundle in a real in-memory kernel.

#![cfg(target_os = "linux")]

#[path = "support/bus.rs"]
mod bus;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system_agents::network::{NetworkBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::linux::LinuxNetwork;
use serde_json::{json, Value};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value as V};

const MOCK: &str = "org.freedesktop.DBus.Mock";
const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const AP_IFACE: &str = "org.freedesktop.NetworkManager.AccessPoint";

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn state_where(k: &Arc<Kernel>, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "network", json!({"type": "state"})).await;
        if ok(&s) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never saw: {what}");
}

/// A mock helper that returns an object path.
async fn path_of(
    conn: &zbus::Connection,
    at: &str,
    member: &str,
    body: &(impl zbus::export::serde::Serialize + zbus::zvariant::DynamicType),
) -> OwnedObjectPath {
    let r = conn
        .call_method(Some(NM), at, Some(MOCK), member, body)
        .await
        .unwrap_or_else(|e| panic!("{member}: {e}"));
    r.body().deserialize().expect("an object path")
}

/// The calls the mock logged for a method of an object: each call's arguments.
async fn calls(conn: &zbus::Connection, at: &str, method: &str) -> Vec<Vec<OwnedValue>> {
    let r = conn
        .call_method(Some(NM), at, Some(MOCK), "GetMethodCalls", &(method,))
        .await
        .expect("GetMethodCalls");
    let all: Vec<(u64, Vec<OwnedValue>)> = r.body().deserialize().expect("a(tav)");
    all.into_iter().map(|(_, args)| args).collect()
}

/// Set the three flag properties of an access point exactly (the template
/// derives them from one `security` number).
async fn flags(conn: &zbus::Connection, ap: &OwnedObjectPath, flags: u32, wpa: u32, rsn: u32) {
    let mut props: HashMap<&str, V<'_>> = HashMap::new();
    props.insert("Flags", V::U32(flags));
    props.insert("WpaFlags", V::U32(wpa));
    props.insert("RsnFlags", V::U32(rsn));
    conn.call_method(
        Some(NM),
        ap.as_str(),
        Some(MOCK),
        "UpdateProperties",
        &(AP_IFACE, props),
    )
    .await
    .expect("UpdateProperties");
}

#[ignore = "needs dbus-daemon and python3-dbusmock: cargo test -p fantastic-system-agents -- --ignored"]
#[tokio::test(flavor = "multi_thread")]
async fn network_reads_classifies_connects_and_forgets_on_a_fake_bus() {
    let bus = bus::Bus::start(&[("networkmanager", None)]).await;
    let conn = bus.connect().await;

    // 1. an ethernet device, a Wi-Fi device, three APs (open, WPA2, SAE) and a
    //    saved connection for the SAE one
    let mut props: HashMap<&str, V<'_>> = HashMap::new();
    props.insert("WirelessEnabled", V::Bool(true));
    props.insert("WirelessHardwareEnabled", V::Bool(true));
    conn.call_method(
        Some(NM),
        NM_PATH,
        Some(MOCK),
        "UpdateProperties",
        &(NM, props),
    )
    .await
    .expect("UpdateProperties");
    path_of(
        &conn,
        NM_PATH,
        "AddEthernetDevice",
        &("eth0", "enp0s2", 100u32),
    )
    .await;
    let wifi = path_of(&conn, NM_PATH, "AddWiFiDevice", &("wifi0", "wlan0", 30u32)).await;
    let add_ap = |name: &'static str, ssid: &'static str, mac: &'static str, strength: u8| {
        (
            wifi.as_str().to_string(),
            name,
            ssid,
            mac,
            2u32,
            2437u32,
            54000u32,
            strength,
            0u32,
        )
    };
    let open = path_of(
        &conn,
        NM_PATH,
        "AddAccessPoint",
        &add_ap("ap-open", "cafe", "00:11:22:33:44:01", 80),
    )
    .await;
    let psk = path_of(
        &conn,
        NM_PATH,
        "AddAccessPoint",
        &add_ap("ap-psk", "home", "00:11:22:33:44:02", 70),
    )
    .await;
    let sae = path_of(
        &conn,
        NM_PATH,
        "AddAccessPoint",
        &add_ap("ap-sae", "safe", "00:11:22:33:44:03", 60),
    )
    .await;
    flags(&conn, &open, 0, 0, 0).await;
    flags(&conn, &psk, 1, 0, 0x188).await;
    flags(&conn, &sae, 1, 0, 0x408).await;
    let known = path_of(
        &conn,
        NM_PATH,
        "AddWiFiConnection",
        &(wifi.as_str(), "safe-conn", "safe", "wpa-psk"),
    )
    .await;
    // the saved connection's settings, exactly (what the template stores varies)
    conn.call_method(
        Some(NM),
        known.as_str(),
        Some(MOCK),
        "AddMethod",
        &(
            "org.freedesktop.NetworkManager.Settings.Connection",
            "GetSettings",
            "",
            "a{sa{sv}}",
            "ret = {'connection': {'id': 'safe-conn', 'uuid': 'uuid-safe', 'type': '802-11-wireless'}, \
             '802-11-wireless': {'ssid': dbus.ByteArray(b'safe')}}",
        ),
    )
    .await
    .expect("AddMethod GetSettings");

    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        NetworkBundle::with_port(Arc::new(LinuxNetwork::new(bus.connect().await))),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "network", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");

    // 2. the state reads right, and the security is classified
    let s = state_where(&k, "three access points", |s| {
        s["access_points"].as_array().is_some_and(|a| a.len() == 3)
    })
    .await;
    let devs: Vec<(&str, &str, &str)> = s["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["iface"].as_str().unwrap(),
                d["kind"].as_str().unwrap(),
                d["state"].as_str().unwrap(),
            )
        })
        .collect();
    assert!(devs.contains(&("enp0s2", "ethernet", "activated")), "{s}");
    assert!(devs.contains(&("wlan0", "wifi", "disconnected")), "{s}");
    let aps: Vec<(&str, &str, bool)> = s["access_points"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["ssid"].as_str().unwrap(),
                a["security"].as_str().unwrap(),
                a["known"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        aps,
        vec![
            ("cafe", "open", false),
            ("home", "wpa-psk", false),
            ("safe", "sae", true)
        ]
    );
    assert_eq!(s["known"][0]["uuid"], "uuid-safe", "{s}");

    // 3. connect to the unknown WPA2 AP: AddAndActivateConnection with the tree
    let r = op(
        &k,
        "network",
        json!({"type": "connect", "ssid": "home", "password": "correct horse"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    let made = calls(&conn, NM_PATH, "AddAndActivateConnection").await;
    assert_eq!(made.len(), 1, "{made:?}");
    let tree: HashMap<String, HashMap<String, OwnedValue>> = made[0][0]
        .try_clone()
        .unwrap()
        .try_into()
        .expect("a{sa{sv}}");
    let text = |section: &str, key: &str| {
        String::try_from(tree[section][key].try_clone().unwrap()).unwrap()
    };
    assert_eq!(text("connection", "type"), "802-11-wireless");
    assert_eq!(text("connection", "id"), "home");
    assert_eq!(text("802-11-wireless", "mode"), "infrastructure");
    assert_eq!(text("802-11-wireless-security", "key-mgmt"), "wpa-psk");
    assert_eq!(text("802-11-wireless-security", "psk"), "correct horse");
    let ssid: Vec<u8> = tree["802-11-wireless"]["ssid"]
        .try_clone()
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(ssid, b"home");
    let perms: Vec<String> = tree["connection"]["permissions"]
        .try_clone()
        .unwrap()
        .try_into()
        .unwrap();
    assert_eq!(
        perms,
        vec![format!(
            "user:{}",
            fantastic_system_agents::sys::user_name()
        )]
    );
    let on: OwnedObjectPath = made[0][2].try_clone().unwrap().try_into().unwrap();
    assert_eq!(
        on.as_str(),
        psk.as_str(),
        "the AP's path is the specific object"
    );

    // ... to the known one: ActivateConnection, no new connection
    let r = op(&k, "network", json!({"type": "connect", "ssid": "safe"})).await;
    assert!(r.get("error").is_none(), "{r}");
    let activated = calls(&conn, NM_PATH, "ActivateConnection").await;
    assert_eq!(activated.len(), 1, "{activated:?}");
    let conn_path: OwnedObjectPath = activated[0][0].try_clone().unwrap().try_into().unwrap();
    assert_eq!(conn_path.as_str(), known.as_str());
    assert_eq!(
        calls(&conn, NM_PATH, "AddAndActivateConnection")
            .await
            .len(),
        1
    );

    // 4. forget deletes it
    let r = op(
        &k,
        "network",
        json!({"type": "forget", "uuid": "uuid-safe"}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    assert_eq!(calls(&conn, known.as_str(), "Delete").await.len(), 1);
}
