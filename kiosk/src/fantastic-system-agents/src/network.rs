//! THE NETWORK AGENT's bundle: the verbs become intents of `NetworkVm`, the driver
//! runs it against the `NetworkOs` port. NetworkManager's devices, Wi-Fi
//! networks and saved connections. The password of `connect` goes into the
//! intent and nowhere else.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use fantastic_system::network::{Args, Call, Config, Effect, Event, NetworkVm, Secret};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::driver::{self, Driver, Machine, Port, Step};
use crate::sys::{self, NetworkOs};

pub const HANDLER_MODULE: &str = "network.tools";
pub const ID: &str = "network";
pub const VIEW: &str = include_str!("views/network.html");

/// What the agent's view may call.
pub const VERBS: [&str; 9] = [
    "reflect",
    "state",
    "scan",
    "connect",
    "activate",
    "deactivate",
    "forget",
    "set_wireless",
    "boot",
];

const SENTENCE: &str = "The machine's network: NetworkManager's devices, Wi-Fi networks and saved connections. Read `state`; `scan`, `connect`, `activate`, `deactivate`, `forget` and `set_wireless` act on them.";

/// The verbs that are intents.
const ACTS: [&str; 6] = [
    "scan",
    "connect",
    "activate",
    "deactivate",
    "forget",
    "set_wireless",
];

impl Machine for NetworkVm {
    type Event = Event;
    type Effect = Effect;

    fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        NetworkVm::reduce(self, ev)
    }
    fn state(&self) -> Value {
        serde_json::to_value(NetworkVm::state(self)).unwrap_or(Value::Null)
    }
    fn start() -> Event {
        Event::Changed
    }
    fn tick(now_ms: u64) -> Event {
        Event::Tick { now_ms }
    }
    fn intent(verb: &str, args: &Value) -> Option<Event> {
        let text = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
        ACTS.contains(&verb).then(|| Event::Intent {
            verb: verb.into(),
            args: Args {
                ssid: text("ssid"),
                password: text("password").map(Secret::new),
                uuid: text("uuid"),
                iface: text("iface"),
                enabled: args.get("enabled").and_then(Value::as_bool),
            },
        })
    }
}

/// The VM's effects, run on the OS port.
struct NetworkPort(Arc<dyn NetworkOs>);

#[async_trait]
impl Port<NetworkVm> for NetworkPort {
    async fn run(&self, effect: Effect) -> Step<Event> {
        match effect {
            Effect::Read => match self.0.read().await {
                Ok(r) => Step::Events(vec![Event::Read(r)]),
                Err(e) => Step::Events(vec![Event::Unavailable {
                    why: e.message().into(),
                }]),
            },
            Effect::Call(c) => match self.0.call(c.clone()).await {
                Ok(()) => Step::Events(vec![Event::Changed]),
                Err(e) => Step::Refused {
                    reason: e.reason().into(),
                    message: format!("{}: {}", call_name(&c), e.message()),
                },
            },
            Effect::Refuse {
                reason, message, ..
            } => Step::Refused { reason, message },
        }
    }
    fn changes(&self) -> mpsc::Receiver<Event> {
        self.0.changes()
    }
}

fn call_name(c: &Call) -> &'static str {
    match c {
        Call::Scan => "scan",
        Call::Activate { .. } => "activate",
        Call::ConnectNew { .. } => "connect",
        Call::Deactivate { .. } => "deactivate",
        Call::Forget { .. } => "forget",
        Call::SetWireless { .. } => "set_wireless",
    }
}

/// Where the OS port comes from.
enum Source {
    /// This machine's.
    Real,
    /// A given one (tests).
    Fixed(Arc<dyn NetworkOs>),
}

/// A running agent: its driver and the loop's task.
type Running = (Arc<Driver<NetworkVm>>, TaskId);

pub struct NetworkBundle {
    source: Source,
    running: Mutex<HashMap<AgentId, Running>>,
}

impl NetworkBundle {
    /// On this machine's OS port.
    pub fn new() -> Self {
        NetworkBundle {
            source: Source::Real,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// On a given port.
    pub fn with_port(port: Arc<dyn NetworkOs>) -> Self {
        NetworkBundle {
            source: Source::Fixed(port),
            running: Mutex::new(HashMap::new()),
        }
    }

    fn driver(&self, id: &AgentId) -> Option<Arc<Driver<NetworkVm>>> {
        self.running
            .lock()
            .expect("running poisoned")
            .get(id)
            .map(|(d, _)| Arc::clone(d))
    }

    async fn boot(&self, id: &AgentId, kernel: &Arc<Kernel>) -> Value {
        if self.driver(id).is_some() {
            return json!({"running": true});
        }
        let port = match &self.source {
            Source::Real => sys::network().await,
            Source::Fixed(p) => Arc::clone(p),
        };
        let (drv, task) = Driver::start(
            kernel,
            id,
            "network_loop",
            NetworkVm::new(Config {
                user: sys::user_name(),
            }),
            Arc::new(NetworkPort(port)),
        );
        self.running
            .lock()
            .expect("running poisoned")
            .insert(id.clone(), (drv, task));
        json!({"running": true})
    }

    async fn state(&self, id: &AgentId) -> Value {
        match self.driver(id) {
            Some(d) => d.state().await,
            None => Machine::state(&NetworkVm::new(Config {
                user: sys::user_name(),
            })),
        }
    }
}

impl Default for NetworkBundle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Bundle for NetworkBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({
            "reflect": "No args. {id, sentence, available, why?}: `available` is false, with `why`, when NetworkManager is not on the system bus.",
            "state": "No args. {available, why?, state (unknown|asleep|disconnected|disconnecting|connecting|connected_local|connected_site|connected_global), connectivity (unknown|none|portal|limited|full), wireless: {enabled, hardware_enabled}, devices: [{iface, kind (ethernet|wifi|other), state (NetworkManager's device state, e.g. activated), connection, ip4: [\"10.0.2.15/24\"]}], access_points: [{ssid (0x<hex> when not UTF-8), strength (0-100), security (open|wpa-psk|sae|wep|enterprise), frequency_mhz, active, known}] (one per ssid, the strongest first), known: [{uuid, id, kind, autoconnect}], busy}.",
            "scan": "No args. Asks every Wi-Fi device to scan; the list follows in `state` events. Refused: unavailable (wireless off), not_found (no Wi-Fi device). Returns the state.",
            "connect": "args: ssid (as listed in `access_points`), password? (WPA: 8-63 characters or 64 hex digits; never kept). A saved connection with that ssid is activated; otherwise a new one is made. Refused: bad_arg (no ssid, no password, bad length), not_found (no such network), unsupported (enterprise, wep), unavailable, denied, failed. Returns the state.",
            "activate": "args: uuid (a saved connection). Activates it on the first device of its kind. Refused: bad_arg, not_found, unavailable, denied, failed. Returns the state.",
            "deactivate": "args: iface (a device with an active connection). Refused: bad_arg, not_found, unavailable, denied, failed. Returns the state.",
            "forget": "args: uuid (a saved connection). Deletes it. Refused: bad_arg, not_found, unavailable, denied, failed. Returns the state.",
            "set_wireless": "args: enabled (bool). Turns the Wi-Fi radio on or off. Refused: bad_arg, unsupported (the hardware switch is off), unavailable, denied, failed. Returns the state.",
            "boot": "Kernel lifecycle — starts the agent's loop (reads NetworkManager, follows its signals). Returns {running}.",
        })
    }

    fn emits_doc(&self) -> Option<Value> {
        Some(
            json!({"state": "{type: \"state\", state}: to the agent's watchers after every change of the state, at most one per 100 ms, the latest."}),
        )
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        payload: &Value,
        kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        let reply = match verb {
            "boot" => self.boot(agent_id, kernel).await,
            "state" => self.state(agent_id).await,
            "reflect" => {
                let s = self.state(agent_id).await;
                let mut r =
                    json!({"id": agent_id.0, "sentence": SENTENCE, "available": s["available"]});
                if let Some(why) = s.get("why").filter(|w| !w.is_null()) {
                    r["why"] = why.clone();
                }
                r
            }
            v if ACTS.contains(&v) => match self.driver(agent_id) {
                Some(d) => d.intent(verb, payload).await,
                None => driver::refusal(
                    "unavailable",
                    &format!("{verb}: the network agent has not booted"),
                ),
            },
            _ => {
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!("network: unknown type {verb:?}"),
                );
                fantastic_kernel::refusal::with(&mut r, "verb", json!(verb));
                r
            }
        };
        Ok(Some(reply))
    }

    /// The loop dies with the agent.
    async fn on_delete(&self, agent_id: &AgentId, kernel: &Arc<Kernel>) -> Result<(), BundleError> {
        let gone = self
            .running
            .lock()
            .expect("running poisoned")
            .remove(agent_id);
        if let Some((_, task)) = gone {
            kernel.cancel_task(&task).await;
        }
        Ok(())
    }
}
