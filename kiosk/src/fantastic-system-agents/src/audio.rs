//! THE AUDIO AGENT's bundle: the verbs become intents of `AudioVm`, the driver
//! runs it against the `AudioOs` port. PipeWire's sinks and sources, their
//! volume and mute, and the default of each (`pw-dump`, `wpctl`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel, TaskId};
use fantastic_system::audio::{AudioVm, Call, Config, Effect, Event};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::driver::{self, Driver, Machine, Port, Step};
use crate::sys::{self, AudioOs};

pub const HANDLER_MODULE: &str = "audio.tools";
pub const ID: &str = "audio";
pub const VIEW: &str = include_str!("views/audio.html");

/// What the agent's view may call.
pub const VERBS: [&str; 6] = [
    "reflect",
    "state",
    "set_volume",
    "set_mute",
    "set_default",
    "boot",
];

const SENTENCE: &str = "The machine's audio: PipeWire sinks and sources through pw-dump and wpctl. Read `state`; `set_volume`, `set_mute` and `set_default` act on a sink or source by `id`, the first two on the default sink when the id is left out.";

/// The verbs that are intents.
const ACTS: [&str; 3] = ["set_volume", "set_mute", "set_default"];

impl Machine for AudioVm {
    type Event = Event;
    type Effect = Effect;

    fn reduce(&mut self, ev: Event) -> Vec<Effect> {
        AudioVm::reduce(self, ev)
    }
    fn state(&self) -> Value {
        serde_json::to_value(AudioVm::state(self)).unwrap_or(Value::Null)
    }
    fn start() -> Event {
        Event::Start
    }
    fn tick(now_ms: u64) -> Event {
        Event::Tick { now_ms }
    }
    fn intent(verb: &str, args: &Value) -> Option<Event> {
        // an id that is no u32 is PipeWire's invalid id: no node has it
        let id = args.get("id").filter(|v| !v.is_null()).map(|v| {
            v.as_u64()
                .and_then(|i| u32::try_from(i).ok())
                .unwrap_or(u32::MAX)
        });
        ACTS.contains(&verb).then(|| Event::Intent {
            verb: verb.into(),
            id,
            volume: args.get("volume").and_then(Value::as_f64),
            muted: args.get("muted").and_then(Value::as_bool),
        })
    }
}

/// The VM's effects, run on the OS port.
struct AudioPort(Arc<dyn AudioOs>);

#[async_trait]
impl Port<AudioVm> for AudioPort {
    async fn run(&self, effect: Effect) -> Step<Event> {
        match effect {
            Effect::Read => match self.0.read().await {
                Ok(v) => Step::Events(vec![Event::Dump(v)]),
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
        Call::SetVolume { .. } => "set_volume",
        Call::SetMute { .. } => "set_mute",
        Call::SetDefault { .. } => "set_default",
    }
}

/// Where the OS port comes from.
enum Source {
    /// This machine's.
    Real,
    /// A given one (tests).
    Fixed(Arc<dyn AudioOs>),
}

/// A running agent: its driver and the loop's task.
type Running = (Arc<Driver<AudioVm>>, TaskId);

pub struct AudioBundle {
    source: Source,
    running: Mutex<HashMap<AgentId, Running>>,
}

impl AudioBundle {
    /// On this machine's OS port.
    pub fn new() -> Self {
        AudioBundle {
            source: Source::Real,
            running: Mutex::new(HashMap::new()),
        }
    }

    /// On a given port.
    pub fn with_port(port: Arc<dyn AudioOs>) -> Self {
        AudioBundle {
            source: Source::Fixed(port),
            running: Mutex::new(HashMap::new()),
        }
    }

    fn driver(&self, id: &AgentId) -> Option<Arc<Driver<AudioVm>>> {
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
            Source::Real => sys::audio().await,
            Source::Fixed(p) => Arc::clone(p),
        };
        let (drv, task) = Driver::start(
            kernel,
            id,
            "audio_loop",
            AudioVm::new(Config::default()),
            Arc::new(AudioPort(port)),
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
            None => Machine::state(&AudioVm::new(Config::default())),
        }
    }
}

impl Default for AudioBundle {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Bundle for AudioBundle {
    fn name(&self) -> &str {
        HANDLER_MODULE
    }

    fn verb_docs(&self) -> Value {
        json!({
            "reflect": "No args. {id, sentence, available, why?}: `available` is false, with `why`, when wpctl or pw-dump is not installed or PipeWire does not answer.",
            "state": "No args. {available, why?, sinks: [{id, name, description, volume (0.0..1.5, what wpctl shows, two places), muted, default}], sources: [the same]}, each list by id.",
            "set_volume": "args: volume (number, clamped to 0.0..1.5), id? (a sink or source; the default sink when left out). Refused: bad_arg (no number), not_found (no such id, or no default sink), unavailable, failed. Returns the state.",
            "set_mute": "args: muted (bool), id? (a sink or source; the default sink when left out). Refused: bad_arg, not_found, unavailable, failed. Returns the state.",
            "set_default": "args: id (a sink or source). Makes it the default of its kind. Refused: bad_arg (no id), not_found, unavailable, failed. Returns the state.",
            "boot": "Kernel lifecycle — starts the agent's loop (reads pw-dump, follows pw-dump --monitor). Returns {running}.",
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
                    &format!("{verb}: the audio agent has not booted"),
                ),
            },
            _ => {
                let mut r = fantastic_kernel::refusal(
                    fantastic_kernel::refusal::UNHANDLED,
                    format!("audio: unknown type {verb:?}"),
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
