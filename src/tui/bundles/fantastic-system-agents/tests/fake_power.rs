//! The power agent against a real in-memory kernel, on a fake OS port: verb ->
//! port call -> reply, port change -> `state` event to a watcher, no service ->
//! `available: false`, `on_delete` stops the loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system::power::{Call, Can, Event, Read};
use fantastic_system_agents::power::{PowerBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::{OsError, PowerOs};
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn read(percent: f64) -> Read {
    Read {
        on_battery: true,
        lid_present: false,
        lid_closed: false,
        display_present: true,
        display_type: 2,
        display_state: 2,
        percentage: percent,
        time_to_empty: 3600,
        time_to_full: 0,
        warning: 1,
        can: Can {
            power_off: "yes".into(),
            reboot: "yes".into(),
            suspend: "yes".into(),
            hibernate: "na".into(),
        },
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
impl PowerOs for Fake {
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
        PowerBundle::with_port(Arc::clone(fake) as Arc<dyn PowerOs>),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "power", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    k
}

/// Wait until `power` reports `available` as asked (its first read has landed).
async fn settled(k: &Arc<Kernel>, available: bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "power", json!({"type": "state"})).await;
        if s["available"] == json!(available) && s["why"] != json!("not read yet") {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("power never settled to available={available}");
}

/// A watcher: an inbox on `power`.
fn watcher(k: &Arc<Kernel>) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from("watcher");
    let rx = k.ensure_inbox(&w);
    let k2 = Arc::clone(k);
    futures_block(async move { k2.watch(&AgentId::from("power"), w).await });
    rx
}

fn futures_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
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
    let fake = Fake::new(Ok(read(81.0)));
    let k = kernel(&fake).await;
    let s = settled(&k, true).await;
    assert_eq!(s["battery"]["percent"], 81.0);
    let r = op(&k, "power", json!({"type": "suspend"})).await;
    assert_eq!(*fake.calls.lock().unwrap(), vec![Call::Suspend]);
    assert_eq!(r["available"], true, "the reply is the state: {r}");
    assert_eq!(r["can"]["suspend"], "yes");
    // hibernate is `na`: refused, and the port is not called
    let r = op(&k, "power", json!({"type": "hibernate"})).await;
    assert_eq!(r["reason"], "unsupported", "{r}");
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    let r = op(&k, "power", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["id"].clone(), r["available"].clone()),
        (json!("power"), json!(true))
    );
    assert!(r["sentence"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_change_reaches_a_watcher_as_one_latest_state() {
    let fake = Fake::new(Ok(read(81.0)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let mut rx = watcher(&k);
    // the boot's own emit may still be coming: drain it
    while next_state(&mut rx, Duration::from_millis(400))
        .await
        .is_some()
    {}
    // two changes within the debounce window: one emit, the latest
    *fake.read.lock().unwrap() = Ok(read(70.0));
    fake.tx.send(Event::Changed).await.unwrap();
    *fake.read.lock().unwrap() = Ok(read(60.0));
    fake.tx.send(Event::Changed).await.unwrap();
    let s = next_state(&mut rx, Duration::from_secs(2))
        .await
        .expect("a state event");
    assert_eq!(s["battery"]["percent"], 60.0, "the latest state: {s}");
    assert!(
        next_state(&mut rx, Duration::from_millis(400))
            .await
            .is_none(),
        "one emit for two changes"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absent_service_is_unavailable_and_refuses_verbs() {
    let why = "org.freedesktop.UPower is not on the system bus";
    let fake = Fake::new(Err(OsError::Unavailable(why.into())));
    let k = kernel(&fake).await;
    let s = settled(&k, false).await;
    assert_eq!(s["why"], why);
    let r = op(&k, "power", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["available"].clone(), r["why"].clone()),
        (json!(false), json!(why))
    );
    let r = op(&k, "power", json!({"type": "suspend"})).await;
    assert_eq!(r["reason"], "unavailable", "{r}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_agent_stops_its_loop() {
    let fake = Fake::new(Ok(read(81.0)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    assert!(!fake.tx.is_closed(), "the loop holds the changes receiver");
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "delete_agent", "id": "power"})).await;
    assert!(r.get("error").is_none(), "{r}");
    for _ in 0..100 {
        if fake.tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the loop kept running after on_delete");
}
