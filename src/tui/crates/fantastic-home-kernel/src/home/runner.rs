//! THE RUNNER: the one `Home`, driven.
//!
//! ```text
//!   Event ──apply──▶ Home::reduce ──Effects──▶ one task each ──HomeOs──▶ Event ──apply──▶ …
//!                         │
//!                         └─ the state changed? emit it (seq + 1), and the menu spec if it changed;
//!                            pages or selection changed? keep them
//! ```
//!
//! `apply` reduces under the lock, emits and keeps, spawns the effects and
//! returns the state as it is right after the reduce — it never waits for an
//! effect, and never holds the lock across an `.await` (it has none).
//!
//! A LOOK runs from `Tick` to `Probed` (or `RegistryFailed`). One look is in
//! flight at a time: a tick that arrives during a look is dropped, and a look
//! asked for during one (after a start, a stop, a register or a remove) runs
//! once, right after it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use fantastic_home::menu::{Menu, MENU_SPEC};
use fantastic_home::{Config as HomeConfig, Demotion, Effect, Event, Home, Prober};
use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use super::os::HomeOs;
use super::wire;

/// How often the home looks again.
pub const POLL: Duration = Duration::from_secs(2);
/// How long a fantastic's kernel has to answer a switch or an unstore.
pub const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);
/// The type of the diagnostics event: a running row's demotion and its probe.
pub const DIAGNOSTICS: &str = "diagnostics";

/// Where the state goes once it changed (and `focus`): in the bundle, `home`'s emits.
pub type Emit = Box<dyn Fn(Value) + Send + Sync>;
/// Where the pages and selection go once they changed: in the bundle, `home`'s record.
pub type Keep = Box<dyn Fn(Vec<String>, Option<String>) + Send + Sync>;

struct Inner {
    home: Home,
    seq: u64,
    /// The last state emitted, its `seq` zeroed for comparing.
    last: Option<Value>,
    /// The last menu spec emitted.
    last_menu: Option<Menu>,
    /// The last pages and selection kept.
    kept: (Vec<String>, Option<String>),
    looking: bool,
    look_again: bool,
}

/// The home, its OS and its sinks.
pub struct Runner {
    os: Arc<dyn HomeOs>,
    inner: Mutex<Inner>,
    emit: Emit,
    keep: Keep,
    /// Effects spawned and not yet applied back.
    in_flight: AtomicUsize,
    idle: Notify,
    poll: Mutex<Option<JoinHandle<()>>>,
}

impl Runner {
    pub fn new(os: Arc<dyn HomeOs>, config: HomeConfig, emit: Emit, keep: Keep) -> Arc<Runner> {
        Arc::new(Runner {
            os,
            inner: Mutex::new(Inner {
                home: Home::new(config),
                seq: 0,
                last: None,
                last_menu: None,
                kept: (Vec::new(), None),
                looking: false,
                look_again: false,
            }),
            emit,
            keep,
            in_flight: AtomicUsize::new(0),
            idle: Notify::new(),
            poll: Mutex::new(None),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("the home's lock is poisoned")
    }

    /// The OS this runner asks.
    pub fn os(&self) -> &Arc<dyn HomeOs> {
        &self.os
    }

    /// Read the home as it is now.
    pub fn read<R>(&self, f: impl FnOnce(&Home) -> R) -> R {
        f(&self.lock().home)
    }

    /// The last pages and selection kept; at boot, the record's.
    pub fn set_kept(&self, pages: Vec<String>, selection: Option<String>) {
        self.lock().kept = (pages, selection);
    }

    /// The state now, emitted first if it changed.
    pub fn state(&self) -> Value {
        let mut g = self.lock();
        self.publish(&mut g)
    }

    /// `{"type":"focus"}` for the host shell, through the emitter; nothing changes.
    pub fn focus(&self) -> Value {
        (self.emit)(json!({"type": "focus"}));
        self.state()
    }

    /// `{"type":"menu_spec","seq":…,"spec":…}`: the menu spec now, `seq` the
    /// state's (the state emitted first if it changed).
    pub fn menu_spec(&self) -> Value {
        let mut g = self.lock();
        self.publish(&mut g);
        spec_payload(g.seq, &g.home.menu())
    }

    /// Apply one event (see the module docs). The state right after its reduce.
    pub fn apply(self: &Arc<Self>, event: Event) -> Value {
        let event = self.stamp(event);
        let forget = match &event {
            Event::Forget { id } => Some(id.clone()),
            _ => None,
        };
        let look_after = matches!(
            event,
            Event::Spawned { .. }
                | Event::Terminated { .. }
                | Event::StopFailed { .. }
                | Event::RegisterDone { .. }
                | Event::Forgot { .. }
        );
        let ends_look = matches!(event, Event::Probed { .. } | Event::RegistryFailed { .. });
        let (state, effects, again) = {
            let mut g = self.lock();
            if matches!(event, Event::Tick { .. }) {
                if g.looking {
                    return self.publish(&mut g);
                }
                g.looking = true;
            }
            let effects = g.home.reduce(event);
            let mut again = false;
            if ends_look {
                g.looking = false;
                again = std::mem::take(&mut g.look_again);
            }
            (self.publish(&mut g), effects, again)
        };
        for effect in effects {
            self.run(effect, forget.clone());
        }
        if again || look_after {
            self.look();
        }
        state
    }

    /// A look now, or right after the one in flight.
    pub fn look(self: &Arc<Self>) {
        {
            let mut g = self.lock();
            if g.looking {
                g.look_again = true;
                return;
            }
        }
        self.apply(Event::Tick { now_ms: 0 });
    }

    /// Wait until no effect is in flight.
    pub async fn settle(&self) {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            idle.await;
        }
    }

    /// Start the poll: a look at once, then every [`POLL`]. Idempotent.
    pub fn start_poll(self: &Arc<Self>) {
        let mut poll = self.poll.lock().expect("the poll's lock is poisoned");
        if poll.is_some() {
            return;
        }
        let weak: Weak<Runner> = Arc::downgrade(self);
        *poll = Some(tokio::spawn(async move {
            let mut every = tokio::time::interval(POLL);
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                every.tick().await;
                let Some(runner) = weak.upgrade() else { break };
                runner.apply(Event::Tick { now_ms: 0 });
            }
        }));
    }

    /// Stop the poll. Idempotent.
    pub fn stop_poll(&self) {
        if let Some(task) = self
            .poll
            .lock()
            .expect("the poll's lock is poisoned")
            .take()
        {
            task.abort();
        }
    }

    /// Is the poll running.
    pub fn polling(&self) -> bool {
        self.poll
            .lock()
            .expect("the poll's lock is poisoned")
            .is_some()
    }

    // ── inside ──────────────────────────────────────────────────────────────

    /// The time, on every event that carries it.
    fn stamp(&self, event: Event) -> Event {
        let now_ms = self.os.now_ms();
        match event {
            Event::Tick { .. } => Event::Tick { now_ms },
            Event::RegistryRead { entries, .. } => Event::RegistryRead { now_ms, entries },
            Event::Probed { results, .. } => Event::Probed { now_ms, results },
            Event::Start { id, .. } => Event::Start { id, now_ms },
            Event::Open { paths, .. } => Event::Open { paths, now_ms },
            other => other,
        }
    }

    /// The state of `g` now: emitted when it differs from the last one (its
    /// `seq` one higher), and the pages and selection kept when they changed
    /// once the home is listed — before that, its pages are not the kept ones yet.
    fn publish(&self, g: &mut Inner) -> Value {
        let os = &self.os;
        let mut state = wire::state(&g.home, os.now_ms(), 0, &|path| os.log_file(path));
        if g.last.as_ref() != Some(&state) {
            g.last = Some(state.clone());
            g.seq += 1;
            state["seq"] = json!(g.seq);
            (self.emit)(state.clone());
            let menu = g.home.menu();
            if g.last_menu.as_ref() != Some(&menu) {
                (self.emit)(spec_payload(g.seq, &menu));
                g.last_menu = Some(menu);
            }
        } else {
            state["seq"] = json!(g.seq);
        }
        let demotions = g.home.take_demotions();
        if !demotions.is_empty() {
            (self.emit)(diagnostics_payload(g.seq, &demotions));
        }
        if g.home.listed() {
            let now = (
                g.home.pages().to_vec(),
                g.home.selection().map(str::to_string),
            );
            if now != g.kept {
                g.kept = now.clone();
                (self.keep)(now.0, now.1);
            }
        }
        state
    }

    /// One task for `effect`; its event is applied when it is back.
    fn run(self: &Arc<Self>, effect: Effect, forget: Option<String>) {
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        let runner = Arc::clone(self);
        tokio::spawn(async move {
            let event = runner.event_of(effect, forget).await;
            runner.apply(event);
            if runner.in_flight.fetch_sub(1, Ordering::SeqCst) == 1 {
                runner.idle.notify_waiters();
            }
        });
    }

    /// Run `effect` on the OS; the event that reports it.
    async fn event_of(&self, effect: Effect, forget: Option<String>) -> Event {
        let os = &self.os;
        let switch = |value: bool| json!({"type": "register_home_folder", "value": value});
        let forgot = |result: Result<Value, String>| Event::Forgot {
            id: forget
                .clone()
                .expect("a SwitchOff or an Unstore comes only from a Forget"),
            result: result.map(|_| ()),
        };
        match effect {
            Effect::ReadRegistry => match os.read_registry().await {
                Ok(entries) => Event::RegistryRead { now_ms: 0, entries },
                Err(error) => Event::RegistryFailed { error },
            },
            Effect::Probe { jobs } => {
                let probes = jobs.into_iter().map(|job| {
                    let os = Arc::clone(os);
                    async move {
                        let mut prober = Prober::new(job.target, job.mode);
                        while let Some(ask) = prober.next() {
                            let answer = os.answer(&ask).await;
                            prober.answer(answer);
                        }
                        (job.key, prober.probe().clone())
                    }
                });
                Event::Probed {
                    now_ms: 0,
                    results: futures_util::future::join_all(probes).await,
                }
            }
            Effect::Spawn { path } => Event::Spawned {
                result: os.spawn(&path).await,
                path,
            },
            Effect::Terminate { path } => match os.terminate(&path).await {
                Ok(sent) => Event::Terminated { path, sent },
                Err(error) => Event::StopFailed { path, error },
            },
            Effect::Register { path, port } => Event::RegisterDone {
                result: os
                    .forward(port, "user_store", switch(true), FORWARD_TIMEOUT)
                    .await
                    .map(|_| ()),
                path,
            },
            Effect::SwitchOff { port } => forgot(
                os.forward(port, "user_store", switch(false), FORWARD_TIMEOUT)
                    .await,
            ),
            Effect::Unstore { port, guid } => forgot(
                os.forward(
                    port,
                    "user_store",
                    json!({"type": "unstore", "guid": guid}),
                    FORWARD_TIMEOUT,
                )
                .await,
            ),
        }
    }
}

/// The wire form of a menu spec (`fantastic_home::menu::MenuSpecMsg` plus its `type`).
fn spec_payload(seq: u64, menu: &Menu) -> Value {
    json!({"type": MENU_SPEC, "seq": seq, "spec": menu})
}

/// The wire form of the running rows demoted since the last emit, each with
/// the probe that demoted it (`seq` the state's). Diagnostics only: not in the state.
fn diagnostics_payload(seq: u64, demotions: &[Demotion]) -> Value {
    json!({"type": DIAGNOSTICS, "seq": seq, "demotions": demotions})
}

#[cfg(test)]
mod tests;
