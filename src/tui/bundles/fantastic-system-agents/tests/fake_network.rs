//! The network agent against a real in-memory kernel, on a fake OS port: verb ->
//! port call -> reply, port change -> `state` event to a watcher, no
//! NetworkManager -> `available: false`, a refused connect never reaches the
//! port, `on_delete` stops the loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system::network::{ApRead, Call, DeviceRead, Event, KnownRead, Read};
use fantastic_system_agents::network::{NetworkBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::{NetworkOs, OsError};
use serde_json::{json, Value};
use tokio::sync::mpsc;

const WPA2: u32 = 0x188;
const ENTERPRISE: u32 = 0x208;

fn ap(path: &str, ssid: &str, strength: u8, rsn: u32) -> ApRead {
    ApRead {
        path: path.into(),
        ssid: ssid.as_bytes().to_vec(),
        strength,
        flags: u32::from(rsn != 0),
        wpa_flags: 0,
        rsn_flags: rsn,
        frequency: 2437,
        active: false,
    }
}

/// An ethernet device with the address asked for, and a Wi-Fi device with
/// three networks (`work` is saved).
fn read(ip: &str) -> Read {
    Read {
        state: 70,
        connectivity: 4,
        wireless_enabled: true,
        wireless_hw_enabled: true,
        devices: vec![
            DeviceRead {
                path: "/d/1".into(),
                iface: "enp0s2".into(),
                device_type: 1,
                state: 100,
                connection: "Wired connection 1".into(),
                ip4: vec![ip.into()],
                aps: vec![],
            },
            DeviceRead {
                path: "/d/2".into(),
                iface: "wlan0".into(),
                device_type: 2,
                state: 30,
                connection: String::new(),
                ip4: vec![],
                aps: vec![
                    ap("/ap/1", "home", 70, WPA2),
                    ap("/ap/2", "work", 50, WPA2),
                    ap("/ap/3", "corp", 30, ENTERPRISE),
                ],
            },
        ],
        known: vec![KnownRead {
            uuid: "uuid-work".into(),
            id: "work".into(),
            kind: "802-11-wireless".into(),
            autoconnect: true,
            ssid: Some(b"work".to_vec()),
        }],
    }
}

/// The fake: what it reads, what it was called with, and the channel its changes ride.
struct Fake {
    read: Mutex<Result<Read, OsError>>,
    calls: Mutex<Vec<Call>>,
    tx: mpsc::Sender<Event>,
    rx: Mutex<Option<mpsc::Receiver<Event>>>,
}

impl Fake {
    fn new(read: Result<Read, OsError>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(16);
        Arc::new(Fake {
            read: Mutex::new(read),
            calls: Mutex::new(vec![]),
            tx,
            rx: Mutex::new(Some(rx)),
        })
    }
}

#[async_trait]
impl NetworkOs for Fake {
    async fn read(&self) -> Result<Read, OsError> {
        self.read.lock().unwrap().clone()
    }
    async fn call(&self, c: Call) -> Result<(), OsError> {
        self.calls.lock().unwrap().push(c);
        Ok(())
    }
    fn changes(&self) -> mpsc::Receiver<Event> {
        self.rx
            .lock()
            .unwrap()
            .take()
            .expect("changes() taken once")
    }
}

async fn op(k: &Arc<Kernel>, target: &str, payload: Value) -> Value {
    Arc::clone(k)
        .send(vec![Item::new(AgentId::from(target), payload)])
        .await
        .remove(0)
        .result
}

async fn kernel(fake: &Arc<Fake>) -> Arc<Kernel> {
    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        NetworkBundle::with_port(Arc::clone(fake) as Arc<dyn NetworkOs>),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "network", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    // the loop starts on boot
    let r = op(&k, "network", json!({"type": "boot"})).await;
    assert_eq!(r["running"], true, "{r}");
    k
}

/// Wait until `network` reports `available` as asked (its first read has landed).
async fn settled(k: &Arc<Kernel>, available: bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "network", json!({"type": "state"})).await;
        if s["available"] == json!(available) && s["why"] != json!("not read yet") {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("network never settled to available={available}");
}

/// A watcher: an inbox on `network`.
fn watcher(k: &Arc<Kernel>) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from("watcher");
    let rx = k.ensure_inbox(&w);
    let k2 = Arc::clone(k);
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { k2.watch(&AgentId::from("network"), w).await })
    });
    rx
}

async fn next_state(
    rx: &mut fantastic_kernel::inbox::InboxReceiver,
    within: Duration,
) -> Option<Value> {
    loop {
        match tokio::time::timeout(within, rx.recv()).await {
            Ok(Some(item)) if item.payload["type"] == "state" => {
                return Some(item.payload["state"].clone())
            }
            Ok(Some(_)) => continue,
            _ => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_verb_calls_the_port_and_replies_the_state() {
    let fake = Fake::new(Ok(read("10.0.2.15/24")));
    let k = kernel(&fake).await;
    let s = settled(&k, true).await;
    assert_eq!(s["state"], "connected_global");
    assert_eq!(s["devices"][0]["ip4"][0], "10.0.2.15/24");
    assert_eq!(s["access_points"][0]["ssid"], "home");
    // a saved network: activate, no password
    let r = op(&k, "network", json!({"type": "connect", "ssid": "work"})).await;
    assert_eq!(r["available"], true, "the reply is the state: {r}");
    assert_eq!(
        fake.calls.lock().unwrap()[0],
        Call::Activate {
            uuid: "uuid-work".into(),
            iface: "wlan0".into()
        }
    );
    // an unknown one: a new connection with the settings tree
    let r = op(
        &k,
        "network",
        json!({"type": "connect", "ssid": "home", "password": "correct horse"}),
    )
    .await;
    assert_eq!(r["available"], true, "{r}");
    assert!(
        !r.to_string().contains("correct horse"),
        "the reply never carries the password"
    );
    match &fake.calls.lock().unwrap()[1] {
        Call::ConnectNew {
            iface,
            ap,
            settings,
        } => {
            assert_eq!((iface.as_str(), ap.as_str()), ("wlan0", "/ap/1"));
            assert!(settings.0.contains_key("802-11-wireless-security"));
        }
        other => panic!("{other:?}"),
    }
    let r = op(
        &k,
        "network",
        json!({"type": "deactivate", "iface": "enp0s2"}),
    )
    .await;
    assert_eq!(r["available"], true, "{r}");
    let r = op(
        &k,
        "network",
        json!({"type": "set_wireless", "enabled": false}),
    )
    .await;
    assert_eq!(r["available"], true, "{r}");
    assert_eq!(
        fake.calls.lock().unwrap()[3],
        Call::SetWireless { enabled: false }
    );
    let r = op(&k, "network", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["id"].clone(), r["available"].clone()),
        (json!("network"), json!(true))
    );
    assert!(r["sentence"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connect_does_not_call_the_port() {
    let fake = Fake::new(Ok(read("10.0.2.15/24")));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let cases = [
        (
            json!({"type": "connect", "ssid": "corp", "password": "whatever1"}),
            "unsupported",
        ),
        (json!({"type": "connect", "ssid": "home"}), "bad_arg"),
        (
            json!({"type": "connect", "ssid": "home", "password": "short"}),
            "bad_arg",
        ),
        (json!({"type": "connect", "ssid": "nowhere"}), "not_found"),
        (json!({"type": "forget", "uuid": "nope"}), "not_found"),
        (json!({"type": "deactivate", "iface": "eth9"}), "not_found"),
    ];
    for (payload, reason) in cases {
        let r = op(&k, "network", payload.clone()).await;
        assert_eq!(r["reason"], reason, "{payload}: {r}");
    }
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_change_reaches_a_watcher_as_one_latest_state() {
    let fake = Fake::new(Ok(read("10.0.2.15/24")));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let mut rx = watcher(&k);
    // the boot's own emit may still be coming: drain it
    while next_state(&mut rx, Duration::from_millis(400))
        .await
        .is_some()
    {}
    // five reads 20 ms apart (inside the 100 ms window): at most two emits,
    // the last carrying the final address
    for n in 1..=5 {
        fake.tx
            .send(Event::Read(read(&format!("10.0.2.{n}/24"))))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut got = Vec::new();
    while let Some(s) = next_state(&mut rx, Duration::from_millis(500)).await {
        got.push(s);
    }
    assert!(
        !got.is_empty() && got.len() <= 2,
        "at most two emits for five changes: {}",
        got.len()
    );
    assert_eq!(
        got.last().unwrap()["devices"][0]["ip4"][0],
        "10.0.2.5/24",
        "the latest state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absent_service_is_unavailable_and_refuses_verbs() {
    let why = "org.freedesktop.NetworkManager is not on the system bus";
    let fake = Fake::new(Err(OsError::Unavailable(why.into())));
    let k = kernel(&fake).await;
    let s = settled(&k, false).await;
    assert_eq!(s["why"], why);
    let r = op(&k, "network", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["available"].clone(), r["why"].clone()),
        (json!(false), json!(why))
    );
    let r = op(&k, "network", json!({"type": "scan"})).await;
    assert_eq!(r["reason"], "unavailable", "{r}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_agent_stops_its_loop() {
    let fake = Fake::new(Ok(read("10.0.2.15/24")));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    assert!(!fake.tx.is_closed(), "the loop holds the changes receiver");
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "delete_agent", "id": "network"})).await;
    assert!(r.get("error").is_none(), "{r}");
    for _ in 0..100 {
        if fake.tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the loop kept running after on_delete");
}
