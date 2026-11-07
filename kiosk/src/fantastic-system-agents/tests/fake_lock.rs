//! The lock agent against a real in-memory kernel, on a fake OS port: verb ->
//! port call -> reply, port change -> `state` event to a watcher, no logind ->
//! `available: false`, `on_delete` stops the loop, and `user` is the only
//! caller the kernel lets through.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{
    bootstrap::{bootstrap, BootstrapOptions},
    AgentId, BundleRegistry, Item, Kernel,
};
use fantastic_system::lock::{Call, Event, Pin, PinChange, Read};
use fantastic_system_agents::lock::{LockBundle, HANDLER_MODULE, VERBS};
use fantastic_system_agents::sys::{LockOs, OsError};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// The PIN the fake has stored.
const PIN: &str = "1234";

fn read(auto_lock_min: u32) -> Read {
    Read {
        has_pin: true,
        auto_lock_min,
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
impl LockOs for Fake {
    async fn read(&self) -> Result<Read, OsError> {
        self.read.lock().unwrap().clone()
    }
    async fn check_pin(&self, pin: Pin) -> Result<bool, OsError> {
        Ok(pin.expose() == PIN)
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
        LockBundle::with_port(Arc::clone(fake) as Arc<dyn LockOs>),
    );
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "create_agent", "parent_id": root, "id": "lock", "handler_module": HANDLER_MODULE})).await;
    assert!(r.get("error").is_none(), "{r}");
    k
}

/// Wait until `lock` reports `available` as asked.
async fn settled(k: &Arc<Kernel>, available: bool) -> Value {
    for _ in 0..200 {
        let s = op(k, "lock", json!({"type": "state"})).await;
        if s["available"] == json!(available) && s["why"] != json!("not read yet") {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("lock never settled to available={available}");
}

/// A watcher: an inbox on `lock`.
fn watcher(k: &Arc<Kernel>) -> fantastic_kernel::inbox::InboxReceiver {
    let w = AgentId::from("watcher");
    let rx = k.ensure_inbox(&w);
    let k2 = Arc::clone(k);
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { k2.watch(&AgentId::from("lock"), w).await })
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
    let fake = Fake::new(Ok(read(5)));
    let k = kernel(&fake).await;
    let s = settled(&k, true).await;
    assert_eq!(
        s,
        json!({"available": true, "locked": false, "has_pin": true, "auto_lock_min": 5,
               "failures": 0, "retry_at_ms": null})
    );
    // a boot takes the sleep inhibitor and tells logind it is unlocked
    assert!(fake
        .calls
        .lock()
        .unwrap()
        .contains(&Call::HoldInhibitor { hold: true }));
    let r = op(&k, "lock", json!({"type": "lock"})).await;
    assert_eq!(r["locked"], true, "the reply is the state: {r}");
    assert_eq!(
        fake.calls.lock().unwrap().last().unwrap(),
        &Call::SetLockedHint { locked: true }
    );
    // a wrong PIN is refused and counted, the right one opens
    let r = op(&k, "lock", json!({"type": "unlock", "pin": "9999"})).await;
    assert_eq!(r["reason"], "wrong_pin", "{r}");
    let r = op(&k, "lock", json!({"type": "unlock", "pin": PIN})).await;
    assert_eq!(
        (r["locked"].clone(), r["failures"].clone()),
        (json!(false), json!(0)),
        "{r}"
    );
    assert_eq!(
        fake.calls.lock().unwrap().last().unwrap(),
        &Call::SetLockedHint { locked: false }
    );
    // a PIN given as a number is no PIN (leading zeros would be lost)
    op(&k, "lock", json!({"type": "lock"})).await;
    let r = op(&k, "lock", json!({"type": "unlock", "pin": 1234})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    op(&k, "lock", json!({"type": "unlock", "pin": PIN})).await;
    // settings: the PIN changes only with the old one, the minutes with no PIN
    let r = op(&k, "lock", json!({"type": "set_pin", "new": "4321"})).await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let r = op(
        &k,
        "lock",
        json!({"type": "set_pin", "old": PIN, "new": "4321"}),
    )
    .await;
    assert_eq!(r["has_pin"], true, "{r}");
    assert_eq!(
        fake.calls.lock().unwrap().last().unwrap(),
        &Call::Persist {
            pin: PinChange::Set(Pin::new("4321")),
            auto_lock_min: 5
        }
    );
    let r = op(&k, "lock", json!({"type": "set_auto_lock", "minutes": 0})).await;
    assert_eq!(r["auto_lock_min"], 0, "{r}");
    let r = op(
        &k,
        "lock",
        json!({"type": "set_auto_lock", "minutes": "soon"}),
    )
    .await;
    assert_eq!(r["reason"], "bad_arg", "{r}");
    let r = op(&k, "lock", json!({"type": "activity"})).await;
    assert_eq!(r["available"], true, "{r}");
    let r = op(&k, "lock", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["id"].clone(), r["available"].clone()),
        (json!("lock"), json!(true))
    );
    assert!(r["sentence"]
        .as_str()
        .is_some_and(|s| s.contains("not a security boundary")));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_port_change_reaches_a_watcher_as_one_latest_state() {
    let fake = Fake::new(Ok(read(5)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    let mut rx = watcher(&k);
    // the boot's own emit may still be coming: drain it
    while next_state(&mut rx, Duration::from_millis(400))
        .await
        .is_some()
    {}
    // logind locks, then four reads 20 ms apart (inside the 100 ms window):
    // at most two emits, the last carrying the final setting
    fake.tx.send(Event::LogindLock).await.unwrap();
    for n in 1..=4 {
        fake.tx.send(Event::Read(read(n))).await.unwrap();
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
    let last = got.last().unwrap();
    assert_eq!(
        (last["locked"].clone(), last["auto_lock_min"].clone()),
        (json!(true), json!(4))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_absent_service_is_unavailable_and_refuses_verbs() {
    let why = "org.freedesktop.login1 is not on the system bus";
    let fake = Fake::new(Err(OsError::Unavailable(why.into())));
    let k = kernel(&fake).await;
    let s = settled(&k, false).await;
    assert_eq!(s["why"], why);
    let r = op(&k, "lock", json!({"type": "reflect"})).await;
    assert_eq!(
        (r["available"].clone(), r["why"].clone()),
        (json!(false), json!(why))
    );
    for verb in [
        json!({"type": "lock"}),
        json!({"type": "set_auto_lock", "minutes": 3}),
    ] {
        let r = op(&k, "lock", verb.clone()).await;
        assert_eq!(r["reason"], "unavailable", "{verb}: {r}");
    }
    // only the boot's own calls (the inhibitor, the hint) ever reached the port
    assert!(fake.calls.lock().unwrap().iter().all(|c| matches!(
        c,
        Call::HoldInhibitor { .. } | Call::SetLockedHint { locked: false }
    )));
}

#[tokio::test(flavor = "multi_thread")]
async fn deleting_the_agent_stops_its_loop() {
    let fake = Fake::new(Ok(read(5)));
    let k = kernel(&fake).await;
    settled(&k, true).await;
    assert!(!fake.tx.is_closed(), "the loop holds the changes receiver");
    let root = k.root().unwrap().id.0.clone();
    let r = op(&k, &root, json!({"type": "delete_agent", "id": "lock"})).await;
    assert!(r.get("error").is_none(), "{r}");
    for _ in 0..100 {
        if fake.tx.is_closed() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the loop kept running after on_delete");
}

/// A caller: forwards `{target, payload}` as the agent it is made as.
struct Relay;

#[async_trait]
impl Bundle for Relay {
    fn name(&self) -> &str {
        "relay.tools"
    }
    async fn handle(
        &self,
        _agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let target = payload["target"].as_str().unwrap_or("lock");
        let item = Item::new(AgentId::from(target), payload["payload"].clone());
        Ok(Some(
            Arc::clone(kernel).send(vec![item]).await.remove(0).result,
        ))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn lock_is_reachable_by_user_only() {
    let fake = Fake::new(Ok(read(5)));
    let mut reg = BundleRegistry::new();
    reg.register(
        HANDLER_MODULE,
        LockBundle::with_port(Arc::clone(&fake) as Arc<dyn LockOs>),
    );
    reg.register("relay.tools", Relay);
    let k = Arc::clone(
        &bootstrap(reg, BootstrapOptions::in_memory())
            .expect("boots")
            .kernel,
    );
    let root = k.root().unwrap().id.0.clone();
    // what `kern/system` writes: the lock answers `user` and nobody else; `user`
    // and an AI both name it in their `can_call` (the AI's own egress is open)
    for (id, can_call) in [("user", json!({"lock": "*"})), ("ai", json!({"lock": "*"}))] {
        let r = op(
            &k,
            &root,
            json!({"type": "create_agent", "parent_id": root, "id": id,
                                     "handler_module": "relay.tools", "can_call": can_call}),
        )
        .await;
        assert!(r.get("error").is_none(), "{r}");
    }
    let r = op(
        &k,
        &root,
        json!({"type": "create_agent", "parent_id": "user", "id": "lock",
                                 "handler_module": HANDLER_MODULE, "callable_by": {"user": VERBS}}),
    )
    .await;
    assert!(r.get("error").is_none(), "{r}");
    settled(&k, true).await;
    let call = |verb: &str| json!({"type": "call", "target": "lock", "payload": {"type": verb}});
    // user: through
    let r = op(&k, "user", call("lock")).await;
    assert_eq!(r["locked"], true, "{r}");
    let r = op(&k, "user", call("state")).await;
    assert_eq!(r["available"], true, "{r}");
    // an AI that may call out to it: refused by the kernel, nothing reached the agent
    let before = fake.calls.lock().unwrap().len();
    for verb in ["unlock", "state", "clear_pin", "set_pin"] {
        let r = op(&k, "ai", call(verb)).await;
        assert!(r["error"].is_string(), "{verb}: {r}");
        assert_eq!(r["reason"], "unauthorized", "{verb}: {r}");
    }
    assert_eq!(fake.calls.lock().unwrap().len(), before);
    assert_eq!(
        op(&k, "user", call("state")).await["locked"],
        true,
        "still locked"
    );
}
