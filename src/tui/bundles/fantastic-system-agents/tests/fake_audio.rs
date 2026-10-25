//! The audio agent against a real in-memory kernel, on a fake OS port: verb ->
//! port call -> reply, port change -> `state` event to a watcher, no tools ->
//! `available: false`, `on_delete` stops the loop.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system::audio::{Call, Event};
use fantastic_system_agents::audio::{AudioBundle, HANDLER_MODULE};
use fantastic_system_agents::sys::{AudioOs, OsError};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// A sink (52, the default) at a channel volume, and a source (54).
fn dump(channel: f64) -> Value {
    json!([
        {"id": 30, "type": "PipeWire:Interface:Metadata", "props": {"metadata.name": "default"},
         "metadata": [{"subject": 0, "key": "default.audio.sink", "value": {"name": "out"}}]},
        {"id": 52, "type": "PipeWire:Interface:Node", "info": {
            "props": {"media.class": "Audio/Sink", "node.name": "out", "node.description": "Speakers"},
            "params": {"Props": [{"mute": false, "channelVolumes": [channel, channel]}]}}},
        {"id": 54, "type": "PipeWire:Interface:Node", "info": {
            "props": {"media.class": "Audio/Source", "node.name": "in"},
            "params": {"Props": [{"mute": false, "channelVolumes": [1.0]}]}}}
    ])
}

/// A `--monitor` array: sink 52's volume changed.
fn delta(channel: f64) -> Value {
    json!([{"id": 52, "type": "PipeWire:Interface:Node", "info": {
        "params": {"Props": [{"mute": false, "channelVolumes": [channel, channel]}]}}}])
}

/// The fake: what it reads, what it was called with, and the channel its changes ride.
struct Fake {
    read: Mutex<Result<Value, OsError>>,
    calls: Mutex<Vec<Call>>,
    tx: mpsc::Sender<Event>,
    rx: Mutex<Option<mpsc::Receiver<Event>>>,
}

impl Fake {
    fn new(read: Result<Value, OsError>) -> Arc<Self> {
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
impl AudioOs for Fake {
    async fn read(&self) -> Result<Value, OsError> {
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
        AudioBundle::with_port(Arc::clone(fake) as Arc<dyn AudioOs>),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "audio", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    k
}

/// Wait until `audio` reports `available` as asked (its first read has landed).
async fn settled(k: &Arc<Kernel>, available: bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "audio", json!({"type": "state"})).await;
        if s["available"] == json!(available) && s["why"] != json!("not read yet") {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("audio never settled to available={available}");
}

/// A watcher: an inbox on `audio`.
fn watcher(k: &Arc<Kernel>) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from("watcher");
    let rx = k.ensure_inbox(&w);
    let k2 = Arc::clone(k);
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { k2.watch(&AgentId::from("audio"), w).await })
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
    let fake = Fake::new(Ok(dump(0.064)));
    let k = kernel(&fake).await;
    let s = settled(&k, true).await;
    assert_eq!(s["sinks"][0]["volume"], 0.4);
    assert_eq!(s["sinks"][0]["default"], true);
    let r = op(&k, "audio", json!({"type": "set_volume", "volume": 0.3})).await;
    assert_eq!(
        *fake.calls.lock().unwrap(),
        vec![Call::SetVolume {
            id: 52,
            volume: 0.3
        }]
    );
    assert_eq!(r["available"], true, "the reply is the state: {r}");
    let r = op(
        &k,
        "audio",
        json!({"type": "set_mute", "id": 54, "muted": true}),
    )
    .await;
    assert_eq!(r["available"], true, "{r}");
    assert_eq!(
        fake.calls.lock().unwrap()[1],
        Call::SetMute {
            id: 54,
            muted: true
        }
    );
    let r = op(&k, "audio", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["id"].clone(), r["available"].clone()),
        (json!("audio"), json!(true))
    );
    assert!(r["sentence"].as_str().is_some_and(|s| !s.is_empty()));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_id_is_refused_without_calling_the_port() {
    let fake = Fake::new(Ok(dump(0.064)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let r = op(
        &k,
        "audio",
        json!({"type": "set_volume", "id": 99, "volume": 0.5}),
    )
    .await;
    assert_eq!(r["reason"], "not_found", "{r}");
    // an id that is no u32 is no node either
    let r = op(&k, "audio", json!({"type": "set_default", "id": -1})).await;
    assert_eq!(r["reason"], "not_found", "{r}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_change_reaches_a_watcher_as_one_latest_state() {
    let fake = Fake::new(Ok(dump(0.064)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let mut rx = watcher(&k);
    // the boot's own emit may still be coming: drain it
    while next_state(&mut rx, Duration::from_millis(400))
        .await
        .is_some()
    {}
    // five deltas 20 ms apart (80 ms, inside the 100 ms window): at most two
    // emits, the last carrying the final volume (0.125 -> 0.50)
    for c in [0.9, 0.7, 0.5, 0.3, 0.125] {
        fake.tx.send(Event::Delta(delta(c))).await.unwrap();
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
        got.last().unwrap()["sinks"][0]["volume"],
        0.5,
        "the latest state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absent_service_is_unavailable_and_refuses_verbs() {
    let why = "wpctl is not installed";
    let fake = Fake::new(Err(OsError::Unavailable(why.into())));
    let k = kernel(&fake).await;
    let s = settled(&k, false).await;
    assert_eq!(s["why"], why);
    let r = op(&k, "audio", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["available"].clone(), r["why"].clone()),
        (json!(false), json!(why))
    );
    let r = op(&k, "audio", json!({"type": "set_volume", "volume": 0.5})).await;
    assert_eq!(r["reason"], "unavailable", "{r}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_agent_stops_its_loop() {
    let fake = Fake::new(Ok(dump(0.064)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    assert!(!fake.tx.is_closed(), "the loop holds the changes receiver");
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "delete_agent", "id": "audio"})).await;
    assert!(r.get("error").is_none(), "{r}");
    for _ in 0..100 {
        if fake.tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the loop kept running after on_delete");
}
