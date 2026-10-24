//! THE POWER AGENT's bundle: the verbs become intents of `PowerVm`, the driver
//! runs it against the `PowerOs` port. Battery and lid (UPower), power off,
//! reboot, suspend and hibernate (logind).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use fantastic_system::power::{Call, Config, Effect, Event, PowerVm};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::driver::{self, Driver, Machine, Port, Step};
use crate::sys::{self, PowerOs};

pub const HANDLER_MODULE: &str = "power.tools";
pub const ID: &str = "power";
pub const VIEW: &str = include_str!("views/power.html");

/// What the agent's view may call.
pub const VERBS: [&str; 7] = [
    "reflect",
    "state",
    "power_off",
    "reboot",
    "suspend",
    "hibernate",
    "boot",
];

const SENTENCE: &str = "The machine's battery, lid and power controls: UPower and logind. Read `state`; `power_off`, `reboot`, `suspend` and `hibernate` act, each only when logind says `yes`.";

impl Machine for PowerVm {
    type Event = Event;
    type Effect = Effect;

    fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        PowerVm::reduce(self, ev)
    }
    fn state(&self) -> Value {
        serde_json::to_value(PowerVm::state(self)).unwrap_or(Value::Null)
    }
    fn start() -> Event {
        Event::Changed
    }
    fn tick(now_ms: u64) -> Event {
        Event::Tick { now_ms }
    }
    fn intent(verb: &str, _args: &Value) -> Option<Event> {
        matches!(verb, "power_off" | "reboot" | "suspend" | "hibernate")
            .then(|| Event::Intent { verb: verb.into() })
    }
}

/// The VM's effects, run on the OS port.
struct PowerPort(Arc<dyn PowerOs>);

#[async_trait]
impl Port<PowerVm> for PowerPort {
    async fn run(&self, effect: Effect) -> Step<Event> {
        match effect {
            Effect::Read => match self.0.read().await {
                Ok(r) => Step::Events(vec![Event::Read(r)]),
                Err(e) => Step::Events(vec![Event::Unavailable {
                    why: e.message().into(),
                }]),
            },
            Effect::Call(c) => match self.0.call(c).await {
                Ok(()) => Step::Events(vec![Event::Changed]),
                Err(e) => Step::Refused {
                    reason: e.reason().into(),
                    message: format!("{}: {}", call_name(c), e.message()),
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

fn call_name(c: Call) -> &'static str {
    match c {
        Call::PowerOff => "power_off",
        Call::Reboot => "reboot",
        Call::Suspend => "suspend",
        Call::Hibernate => "hibernate",
    }
}

/// Where the OS port comes from.
enum Source {
    /// This machine's.
    Real,
    /// A given one (tests).
    Fixed(Arc<dyn PowerOs>),
}

/// A running agent: its driver and the loop's task.
type Running = (Arc<Driver<PowerVm>>, TaskId);

pub struct PowerBundle {
    source: Source,
    running: Mutex<HashMap<AgentId, Running>>,
}

impl PowerBundle {
    /// On this machine's OS port.
    pub fn new() -> Self {
        PowerBundle {
            source: Source::Real,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// On a given port.
    pub fn with_port(port: Arc<dyn PowerOs>) -> Self {
        PowerBundle {
            source: Source::Fixed(port),
            running: Mutex::new(HashMap::new()),
        }
    }

    fn driver(&self, id: &AgentId) -> Option<Arc<Driver<PowerVm>>> {
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
            Source::Real => sys::power().await,
            Source::Fixed(p) => Arc::clone(p),
        };
        let (drv, task) = Driver::start(
            kernel,
            id,
            "power_loop",
            PowerVm::new(Config::default()),
            Arc::new(PowerPort(port)),
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
            None => Machine::state(&PowerVm::new(Config::default())),
        }
    }
}

impl Default for PowerBundle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Bundle for PowerBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({
            "reflect": "No args. {id, sentence, available, why?}: `available` is false, with `why`, when UPower or logind is not on the system bus.",
            "state": "No args. {available, why?, on_battery, battery: {percent, state (unknown|charging|discharging|empty|fully_charged|pending_charge|pending_discharge), time_to_empty_s, time_to_full_s, warning (unknown|none|discharging|low|critical|action)}|null (no battery), lid_closed: bool|null (no lid), can: {power_off, reboot, suspend, hibernate} (each yes|no|challenge|na), sleeping}.",
            "power_off": "No args. Powers the machine off, only when `can.power_off` is `yes`. Refused: denied (needs authentication), unsupported, unavailable. Returns the state.",
            "reboot": "No args. Reboots the machine, only when `can.reboot` is `yes`. Refused: denied, unsupported, unavailable. Returns the state.",
            "suspend": "No args. Suspends the machine to RAM, only when `can.suspend` is `yes`. Refused: denied, unsupported, unavailable. Returns the state.",
            "hibernate": "No args. Hibernates the machine, only when `can.hibernate` is `yes`. Refused: denied, unsupported, unavailable. Returns the state.",
            "boot": "Kernel lifecycle — starts the agent's loop (reads UPower and logind, follows their changes). Returns {running}.",
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
            "power_off" | "reboot" | "suspend" | "hibernate" => match self.driver(agent_id) {
                Some(d) => d.intent(verb, payload).await,
                None => driver::refusal(
                    "unavailable",
                    &format!("{verb}: the power agent has not booted"),
                ),
            },
            _ => {
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!("power: unknown type {verb:?}"),
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
