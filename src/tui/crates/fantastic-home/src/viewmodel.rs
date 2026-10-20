//! THE HOME: events in, effects out, rows to draw.
//!
//! ```text
//!   OS layer ──Event──▶ Home::reduce ──Vec<Effect>──▶ OS layer
//!                        Home::rows(now_ms) ──▶ the list to draw
//! ```
//!
//! The OS layer stamps every event that needs the time with `now_ms`, runs
//! each effect, and answers with the matching event:
//!
//! ```text
//!   ReadRegistry              → RegistryRead
//!   Probe { jobs }            → Probed (each job run with probe::Prober)
//!   Spawn { path }            → Spawned
//!   Terminate { path }        → Terminated
//!   Register { path, port }   → RegisterDone
//!   SwitchOff { port }        → Forgot
//!   Unstore { port, guid }    → Forgot
//! ```
//!
//! ENTRIES. One per registry entry, its id its guid, in registry order; then one
//! per folder being added, its id `"adding:" + path`, in the order they were
//! opened. A folder being added is one the home was asked to open but the
//! registry does not list yet: when its probe shows its canvas, the home asks
//! that canvas's own kernel to register it (`Register`), and the next registry
//! read lists it under its guid.
//!
//! LAUNCHES. Starting a fantastic records when it was started; for
//! `launch_grace_ms` its row shows `Starting` unless it is already shown
//! running. Once the grace has passed without its canvas, the home says so.
//!
//! PAGES. Every open entry has a page; `selection` is the page shown, `None`
//! being the home itself.
//!
//! In `Mode::Canvas` nothing is ever started or stopped: the sandboxed app may
//! only ask canvases.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::model::{last_component, registered, Canvas, Registered};
use crate::probe::{is_active, Mode, Probe, Status, Target};

/// The prefix of a folder-being-added's entry id.
const ADDING: &str = "adding:";

/// The missed canvas answer, in a row, at which a running row shows what the
/// probe found. Before it, a probe that finds the SAME run (holder, canvas)
/// whose canvas did not answer in time keeps the row running: misses 1 and 2
/// are ridden out, the 3rd shows — about 6 s at the 2 s poll. A loaded machine
/// misses answers (the canvas check's timeout), and every page would drop its
/// live canvas for each. A gone holder, another run or another canvas shows at
/// once; an answer resets the count.
const MISS_SHOWN: u32 = 3;

/// Why a folder being added cannot be added while it runs as it does.
const NO_CANVAS: &str =
    "it runs without a canvas (a terminal, or headless). Stop it there, then add it again.";

/// What the home may do, how long a start may take, and which folder is its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub mode: Mode,
    /// How long a started fantastic has to open its canvas.
    pub launch_grace_ms: u64,
    /// The home's own folder, as a kernel running there names it: a registry
    /// entry with this `path` is never listed.
    pub own_folder: Option<String>,
}

impl Config {
    /// The grace a start has unless the caller says otherwise.
    pub const DEFAULT_LAUNCH_GRACE_MS: u64 = 30_000;

    /// `mode` with the default launch grace.
    pub fn new(mode: Mode) -> Config {
        Config {
            mode,
            launch_grace_ms: Self::DEFAULT_LAUNCH_GRACE_MS,
            own_folder: None,
        }
    }

    /// This config, with `path` as the home's own folder.
    pub fn own_folder(self, path: String) -> Config {
        Config {
            own_folder: Some(path),
            ..self
        }
    }
}

/// One probe to run: `key` names the entry its result belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub key: String,
    pub target: Target,
    pub mode: Mode,
}

/// What happened, as the OS layer reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Time to look again.
    Tick { now_ms: u64 },
    /// The registry's `(folder, fantastic.yaml text)` entries.
    RegistryRead {
        now_ms: u64,
        entries: Vec<(String, String)>,
    },
    /// The probes of a `Probe` effect, by job key.
    Probed {
        now_ms: u64,
        results: Vec<(String, Probe)>,
    },
    /// The user starts an entry.
    Start { id: String, now_ms: u64 },
    /// A `Spawn` ran.
    Spawned {
        path: String,
        result: Result<(), String>,
    },
    /// The user stops an entry.
    Stop { id: String },
    /// A `Terminate` ran; `sent`: the fantastic was told to stop.
    Terminated { path: String, sent: bool },
    /// Open these folders (from the command line, a drop, a dialog).
    Open { paths: Vec<String>, now_ms: u64 },
    /// A `Register` ran.
    RegisterDone {
        path: String,
        result: Result<(), String>,
    },
    /// The user removes an entry from the list.
    Forget { id: String },
    /// A `SwitchOff` or `Unstore` ran.
    Forgot {
        id: String,
        result: Result<(), String>,
    },
    /// Show this entry's page; `None` shows the home.
    Select { id: Option<String> },
    /// Show the open page at this index (0-based).
    SelectPage { index: usize },
    /// Close this entry's page.
    ClosePage { id: String },
    /// The user has read the problem.
    Dismiss,
    /// A `ReadRegistry` failed.
    RegistryFailed { error: String },
    /// A `Terminate` could not ask the fantastic in `path` to stop.
    StopFailed { path: String, error: String },
    /// The pages and selection kept from an earlier run. Held until the first
    /// `RegistryRead`, the first moment a page's id can be checked.
    Restore {
        pages: Vec<String>,
        selection: Option<String>,
    },
}

/// What the OS layer is asked to do. All data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Read every `<home>/fantastics/<folder>/fantastic.yaml`.
    ReadRegistry,
    /// Run each job's probe.
    Probe { jobs: Vec<Job> },
    /// Start the fantastic in `path` with its canvas.
    Spawn { path: String },
    /// Tell the fantastic in `path` to stop.
    Terminate { path: String },
    /// Turn the home-folder switch of the fantastic in `path` on, through its
    /// own kernel behind the canvas on `port`.
    Register { path: String, port: u16 },
    /// Turn the home-folder switch off through the running kernel on `port`
    /// (the entry removes itself).
    SwitchOff { port: u16 },
    /// Remove the entry `guid` through the running kernel on `port`.
    Unstore { port: u16, guid: String },
}

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub id: String,
    pub guid: Option<String>,
    pub name: String,
    pub path: String,
    pub last_seen: Option<i64>,
    pub status: Status,
    pub pid: Option<u32>,
}

impl Row {
    /// What the user may do with this row in `mode`, in this order: `start` (Lock,
    /// stopped) or else `stop` (Lock, a process holds it, not already stopping);
    /// then `open` (always in Lock; in Canvas unless it is a stopped entry); then
    /// `remove` for a registry entry.
    pub fn actions(&self, mode: Mode) -> Vec<&'static str> {
        let lock = mode == Mode::Lock;
        let mut out = Vec::new();
        if lock && self.status == Status::Stopped {
            out.push("start");
        } else if lock && self.pid.is_some() && self.status != Status::Stopping {
            out.push("stop");
        }
        if lock || self.status != Status::Stopped || self.guid.is_none() {
            out.push("open");
        }
        if self.guid.is_some() {
            out.push("remove");
        }
        out
    }
}

/// A folder being added.
#[derive(Debug, Clone)]
struct Adding {
    path: String,
    since_ms: u64,
    /// The home started it.
    started: bool,
    /// Its probe since it began being added.
    probe: Option<Probe>,
    /// A `Register` is out and not answered yet.
    registering: bool,
    /// The last reason it could not be added.
    error: Option<String>,
}

/// The home's state.
#[derive(Debug, Clone)]
pub struct Home {
    config: Config,
    entries: Vec<Registered>,
    probes: BTreeMap<String, Probe>,
    adding: Vec<Adding>,
    /// The last probe of each folder ever being added, by path.
    folder_probes: BTreeMap<String, Probe>,
    launches: BTreeMap<String, u64>,
    stopping: BTreeSet<String>,
    verified: BTreeSet<String>,
    pages: Vec<String>,
    selection: Option<String>,
    problem: Option<String>,
    listed: bool,
    pending_opens: Vec<String>,
    /// The registry has been read at least once.
    read_once: bool,
    /// A `Restore` that arrived before the first registry read.
    pending_restore: Option<(Vec<String>, Option<String>)>,
    /// Missed canvas answers in a row, by guid, ridden out (`MISS_SHOWN`).
    missed: BTreeMap<String, u32>,
    now_ms: u64,
}

impl Home {
    /// An empty home: nothing listed yet.
    pub fn new(config: Config) -> Home {
        Home {
            config,
            entries: Vec::new(),
            probes: BTreeMap::new(),
            adding: Vec::new(),
            folder_probes: BTreeMap::new(),
            launches: BTreeMap::new(),
            stopping: BTreeSet::new(),
            verified: BTreeSet::new(),
            pages: Vec::new(),
            selection: None,
            problem: None,
            listed: false,
            pending_opens: Vec::new(),
            read_once: false,
            pending_restore: None,
            missed: BTreeMap::new(),
            now_ms: 0,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The open pages, in order, by entry id.
    pub fn pages(&self) -> &[String] {
        &self.pages
    }

    /// The page shown; `None` is the home.
    pub fn selection(&self) -> Option<&str> {
        self.selection.as_deref()
    }

    /// The last thing that went wrong, said for the user.
    pub fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The registry's entries, as last read.
    pub(crate) fn entries(&self) -> &[Registered] {
        &self.entries
    }

    /// The registry has been probed at least once.
    pub fn listed(&self) -> bool {
        self.listed
    }

    /// Apply one event; the effects for the OS layer to run.
    pub fn reduce(&mut self, event: Event) -> Vec<Effect> {
        let effects = match event {
            Event::Tick { now_ms } => {
                self.at(now_ms);
                vec![Effect::ReadRegistry]
            }
            Event::RegistryRead { now_ms, entries } => {
                self.at(now_ms);
                self.registry_read(entries)
            }
            Event::Probed { now_ms, results } => {
                self.at(now_ms);
                self.probed(now_ms, results)
            }
            Event::Start { id, now_ms } => {
                self.at(now_ms);
                match self.path_of(&id) {
                    Some(path) => self.start(&path, now_ms),
                    None => Vec::new(),
                }
            }
            Event::Spawned { path, result } => {
                if let Err(error) = result {
                    self.launches.remove(&path);
                    if self.adding.iter().any(|a| a.path == path) {
                        // The spawn is strict: the add ends now, with the cause.
                        self.fail_adding(&path, &format!("it did not start — {error}"));
                    } else {
                        self.problem = Some(format!(
                            "Could not start {}: {error}",
                            self.name_of_path(&path)
                        ));
                    }
                }
                Vec::new()
            }
            Event::Stop { id } => match self.path_of(&id) {
                Some(path) => self.stop(&path),
                None => Vec::new(),
            },
            Event::Terminated { path, sent } => {
                if sent {
                    self.launches.remove(&path);
                    self.stopping.insert(path);
                }
                Vec::new()
            }
            Event::Open { paths, now_ms } => {
                self.at(now_ms);
                self.open(paths, now_ms)
            }
            Event::RegisterDone { path, result } => {
                if let Some(adding) = self.adding.iter_mut().find(|a| a.path == path) {
                    adding.registering = false;
                    if let Err(error) = result {
                        adding.error = Some(error);
                    }
                }
                Vec::new()
            }
            Event::Forget { id } => self.forget(&id),
            Event::Forgot { id, result } => {
                match result {
                    Ok(()) => self.close_page(&id),
                    Err(error) => {
                        let name = self.name_of_id(&id);
                        self.problem = Some(format!("Could not remove {name}: {error}"));
                    }
                }
                Vec::new()
            }
            Event::Select { id } => {
                match id {
                    None => self.selection = None,
                    Some(id) => {
                        if self.has_entry(&id) {
                            self.open_page(&id);
                        }
                    }
                }
                Vec::new()
            }
            Event::SelectPage { index } => {
                if let Some(id) = self.pages.get(index) {
                    self.selection = Some(id.clone());
                }
                Vec::new()
            }
            Event::ClosePage { id } => {
                self.close_page(&id);
                Vec::new()
            }
            Event::Dismiss => {
                self.problem = None;
                Vec::new()
            }
            Event::RegistryFailed { error } => {
                self.problem = Some(format!("Could not read your fantastics: {error}"));
                Vec::new()
            }
            Event::StopFailed { path, error } => {
                self.problem = Some(format!(
                    "Could not stop {}: {error}",
                    self.name_of_path(&path)
                ));
                Vec::new()
            }
            Event::Restore { pages, selection } => {
                if self.read_once {
                    self.restore(pages, selection);
                } else {
                    self.pending_restore = Some((pages, selection));
                }
                Vec::new()
            }
        };
        self.close_orphan_pages();
        effects
    }

    /// The list to draw at `now_ms`: the registry's entries, then the folders
    /// being added.
    pub fn rows(&self, now_ms: u64) -> Vec<Row> {
        let registered = self.entries.iter().map(|reg| Row {
            id: reg.guid.clone(),
            guid: Some(reg.guid.clone()),
            name: reg.display_name(),
            path: reg.path.clone(),
            last_seen: Some(reg.last_seen),
            status: self.status_of(reg, now_ms),
            pid: self.probes.get(&reg.guid).and_then(|p| p.holder),
        });
        let adding = self.adding.iter().map(|a| Row {
            id: adding_id(&a.path),
            guid: None,
            name: last_component(&a.path).to_string(),
            path: a.path.clone(),
            last_seen: None,
            status: Status::Starting,
            pid: a.probe.as_ref().and_then(|p| p.holder),
        });
        registered.chain(adding).collect()
    }

    // ── events ──────────────────────────────────────────────────────────────

    fn at(&mut self, now_ms: u64) {
        self.now_ms = self.now_ms.max(now_ms);
    }

    fn registry_read(&mut self, entries: Vec<(String, String)>) -> Vec<Effect> {
        // The home's own folder is dropped before anything else reads the entries.
        let entries = match &self.config.own_folder {
            None => entries,
            Some(own) => entries
                .into_iter()
                .filter(|(folder, yaml)| {
                    Registered::parse(folder, yaml).is_none_or(|reg| &reg.path != own)
                })
                .collect(),
        };
        self.entries = registered(entries);
        self.read_once = true;
        if let Some((pages, selection)) = self.pending_restore.take() {
            self.restore(pages, selection);
        }
        let guids: BTreeSet<&str> = self.entries.iter().map(|r| r.guid.as_str()).collect();
        self.probes.retain(|guid, _| guids.contains(guid.as_str()));

        let mut listed_now = Vec::new();
        self.adding
            .retain(|a| match self.entries.iter().find(|r| r.path == a.path) {
                Some(reg) => {
                    listed_now.push((a.path.clone(), reg.guid.clone()));
                    false
                }
                None => true,
            });
        for (path, guid) in listed_now {
            self.swap_page(&adding_id(&path), &guid);
        }

        let mode = self.config.mode;
        let mut jobs: Vec<Job> = self
            .entries
            .iter()
            .map(|reg| Job {
                key: reg.guid.clone(),
                target: Target::Registered {
                    reg: reg.clone(),
                    verified: self.verified.contains(&reg.run_key()),
                },
                mode,
            })
            .collect();
        jobs.extend(self.adding.iter().map(|a| Job {
            key: adding_id(&a.path),
            target: Target::Adding {
                path: a.path.clone(),
            },
            mode,
        }));
        vec![Effect::Probe { jobs }]
    }

    /// The probe to keep for `guid`: the last one while a running row's run is
    /// unchanged and only its canvas missed an answer, until the `MISS_SHOWN`th
    /// polls in a row; else `probe`. A stopped holder, another run or another
    /// canvas shows at once.
    fn ride_out_a_miss(&mut self, guid: &str, probe: Probe) -> Probe {
        let running = |p: &Probe| {
            p.is_fantastic
                && p.holder.is_some()
                && p.owns_entry
                && p.canvas.is_some()
                && p.canvas_up
                && p.guid_matches
        };
        if running(&probe) {
            self.missed.remove(guid);
            return probe;
        }
        let Some(last) = self.probes.get(guid).filter(|l| running(l)) else {
            self.missed.remove(guid);
            return probe;
        };
        let same_run = probe.is_fantastic
            && probe.holder == last.holder
            && probe.owns_entry
            && probe.canvas == last.canvas
            && !probe.canvas_up;
        let misses = self.missed.get(guid).copied().unwrap_or(0) + 1;
        if same_run && misses < MISS_SHOWN {
            self.missed.insert(guid.to_string(), misses);
            return last.clone();
        }
        self.missed.remove(guid);
        probe
    }

    fn probed(&mut self, now_ms: u64, results: Vec<(String, Probe)>) -> Vec<Effect> {
        for (key, probe) in results {
            if let Some(path) = key.strip_prefix(ADDING) {
                if let Some(adding) = self.adding.iter_mut().find(|a| a.path == path) {
                    adding.probe = Some(probe.clone());
                    self.folder_probes.insert(path.to_string(), probe);
                }
            } else if self.entries.iter().any(|r| r.guid == key) {
                let probe = self.ride_out_a_miss(&key, probe);
                self.probes.insert(key, probe);
            }
        }

        self.verified = self
            .entries
            .iter()
            .filter(|reg| {
                self.verified.contains(&reg.run_key())
                    || self.probes.get(&reg.guid).is_some_and(is_active)
            })
            .map(Registered::run_key)
            .collect();

        let grace = self.config.launch_grace_ms;
        for i in 0..self.entries.len() {
            let reg = &self.entries[i];
            let Some(probe) = self.probes.get(&reg.guid) else {
                continue;
            };
            let path = reg.path.clone();
            let name = reg.display_name();
            let active = is_active(probe);
            let no_holder = probe.holder.is_none();
            let mut launching = false;
            if let Some(&started) = self.launches.get(&path) {
                if now_ms.saturating_sub(started) < grace {
                    launching = true;
                } else {
                    self.launches.remove(&path);
                    if !active {
                        self.problem = Some(format!("{name} did not open its canvas."));
                    }
                }
            }
            if no_holder {
                self.stopping.remove(&path);
            }
            let probe = &self.probes[&self.entries[i].guid];
            if matches!(
                Status::of(probe, launching, self.stopping.contains(&path)),
                Status::Running(_)
            ) {
                self.launches.remove(&path);
            }
        }

        let mut effects = Vec::new();
        let mut expired = Vec::new();
        for adding in &mut self.adding {
            if now_ms.saturating_sub(adding.since_ms) >= grace {
                expired.push(adding.path.clone());
                continue;
            }
            let Some(probe) = &adding.probe else {
                continue;
            };
            let shown =
                probe.canvas_up && (self.config.mode == Mode::Canvas || probe.holder.is_some());
            match &probe.canvas {
                Some(canvas) if shown => {
                    if !adding.registering {
                        adding.registering = true;
                        effects.push(Effect::Register {
                            path: adding.path.clone(),
                            port: canvas.port,
                        });
                    }
                }
                None if probe.holder.is_some() && !adding.started => {
                    adding.error = Some(NO_CANVAS.to_string());
                }
                _ => {}
            }
        }
        for path in expired {
            let reason = self.adding.iter().find(|a| a.path == path).map(|a| {
                a.error
                    .clone()
                    .unwrap_or_else(|| "it did not start.".to_string())
            });
            if let Some(reason) = reason {
                self.fail_adding(&path, &reason);
            }
        }

        if !self.listed {
            self.listed = true;
            let pending = std::mem::take(&mut self.pending_opens);
            if !pending.is_empty() {
                effects.extend(self.open(pending, now_ms));
            }
        }
        effects
    }

    fn start(&mut self, path: &str, now_ms: u64) -> Vec<Effect> {
        if self.config.mode == Mode::Canvas {
            self.problem = Some(format!(
                "{} can only be started from the full app.",
                self.name_of_path(path)
            ));
            return Vec::new();
        }
        self.stopping.remove(path);
        self.launches.insert(path.to_string(), now_ms);
        vec![Effect::Spawn {
            path: path.to_string(),
        }]
    }

    fn stop(&mut self, path: &str) -> Vec<Effect> {
        if self.config.mode == Mode::Canvas {
            self.problem = Some(format!(
                "{} can only be stopped from the full app.",
                self.name_of_path(path)
            ));
            return Vec::new();
        }
        vec![Effect::Terminate {
            path: path.to_string(),
        }]
    }

    fn open(&mut self, paths: Vec<String>, now_ms: u64) -> Vec<Effect> {
        if !self.listed {
            self.pending_opens.extend(paths);
            return Vec::new();
        }
        let lock = self.config.mode == Mode::Lock;
        let mut effects = Vec::new();
        for path in paths {
            if let Some(reg) = self.entries.iter().find(|r| r.path == path) {
                let id = reg.guid.clone();
                let name = reg.display_name();
                let stopped = self.status_of(reg, now_ms) == Status::Stopped;
                if lock || !stopped {
                    self.open_page(&id);
                    if lock && stopped {
                        effects.extend(self.start(&path, now_ms));
                    }
                } else {
                    self.problem = Some(format!("{name} isn't running"));
                }
            } else if self.adding.iter().any(|a| a.path == path) {
                self.open_page(&adding_id(&path));
            } else if lock {
                let held = self
                    .folder_probes
                    .get(&path)
                    .is_some_and(|p| p.holder.is_some());
                if held {
                    self.begin_adding(&path, now_ms, false);
                } else {
                    effects.extend(self.start(&path, now_ms));
                    self.begin_adding(&path, now_ms, true);
                }
            } else {
                self.begin_adding(&path, now_ms, false);
            }
        }
        effects
    }

    /// The add of `path` ends: its launch is dropped and `reason` is the problem.
    fn fail_adding(&mut self, path: &str, reason: &str) {
        self.adding.retain(|a| a.path != path);
        self.launches.remove(path);
        self.problem = Some(format!("Could not add {}: {reason}", last_component(path)));
    }

    fn begin_adding(&mut self, path: &str, now_ms: u64, started: bool) {
        self.adding.push(Adding {
            path: path.to_string(),
            since_ms: now_ms,
            started,
            probe: None,
            registering: false,
            error: None,
        });
        self.open_page(&adding_id(path));
    }

    fn forget(&mut self, id: &str) -> Vec<Effect> {
        let Some(reg) = self.entries.iter().find(|r| r.guid == id) else {
            return Vec::new();
        };
        if let Status::Running(canvas) = self.status_of(reg, self.now_ms) {
            return vec![Effect::SwitchOff { port: canvas.port }];
        }
        let running: Option<Canvas> =
            self.entries
                .iter()
                .find_map(|r| match self.status_of(r, self.now_ms) {
                    Status::Running(canvas) => Some(canvas),
                    _ => None,
                });
        match running {
            Some(canvas) => vec![Effect::Unstore {
                port: canvas.port,
                guid: reg.guid.clone(),
            }],
            None => {
                self.problem = Some(format!(
                    "Start any fantastic to remove {} — only a running one can change the list.",
                    reg.display_name()
                ));
                Vec::new()
            }
        }
    }

    // ── pages ───────────────────────────────────────────────────────────────

    /// The kept pages that still name a registry entry, in order; the selection
    /// only when it is one of them.
    fn restore(&mut self, pages: Vec<String>, selection: Option<String>) {
        let mut kept: Vec<String> = Vec::new();
        for id in pages {
            let listed = !id.starts_with(ADDING) && self.entries.iter().any(|r| r.guid == id);
            if listed && !kept.contains(&id) {
                kept.push(id);
            }
        }
        self.selection = selection.filter(|s| kept.contains(s));
        self.pages = kept;
    }

    fn open_page(&mut self, id: &str) {
        if !self.pages.iter().any(|p| p == id) {
            self.pages.push(id.to_string());
        }
        self.selection = Some(id.to_string());
    }

    fn close_page(&mut self, id: &str) {
        let Some(index) = self.pages.iter().position(|p| p == id) else {
            return;
        };
        self.pages.remove(index);
        if self.selection.as_deref() == Some(id) {
            let neighbour = self
                .pages
                .get(index)
                .or_else(|| index.checked_sub(1).and_then(|i| self.pages.get(i)));
            self.selection = neighbour.cloned();
        }
    }

    /// The adding page `from` becomes the page `to` in place; when `to` already
    /// has a page, `from` is removed instead. The selection follows.
    fn swap_page(&mut self, from: &str, to: &str) {
        let Some(index) = self.pages.iter().position(|p| p == from) else {
            return;
        };
        if self.pages.iter().any(|p| p == to) {
            self.pages.remove(index);
        } else {
            self.pages[index] = to.to_string();
        }
        if self.selection.as_deref() == Some(from) {
            self.selection = Some(to.to_string());
        }
    }

    fn close_orphan_pages(&mut self) {
        let orphans: Vec<String> = self
            .pages
            .iter()
            .filter(|id| !self.has_entry(id))
            .cloned()
            .collect();
        for id in orphans {
            self.close_page(&id);
        }
    }

    // ── lookups ─────────────────────────────────────────────────────────────

    fn status_of(&self, reg: &Registered, now_ms: u64) -> Status {
        let Some(probe) = self.probes.get(&reg.guid) else {
            return Status::Starting;
        };
        let launching = self
            .launches
            .get(&reg.path)
            .is_some_and(|&started| now_ms.saturating_sub(started) < self.config.launch_grace_ms);
        Status::of(probe, launching, self.stopping.contains(&reg.path))
    }

    fn has_entry(&self, id: &str) -> bool {
        match id.strip_prefix(ADDING) {
            Some(path) => self.adding.iter().any(|a| a.path == path),
            None => self.entries.iter().any(|r| r.guid == id),
        }
    }

    fn path_of(&self, id: &str) -> Option<String> {
        match id.strip_prefix(ADDING) {
            Some(path) => self
                .adding
                .iter()
                .find(|a| a.path == path)
                .map(|a| a.path.clone()),
            None => self
                .entries
                .iter()
                .find(|r| r.guid == id)
                .map(|r| r.path.clone()),
        }
    }

    fn name_of_path(&self, path: &str) -> String {
        match self.entries.iter().find(|r| r.path == path) {
            Some(reg) => reg.display_name(),
            None => last_component(path).to_string(),
        }
    }

    /// An entry's name by its id; an unknown id is itself.
    pub(crate) fn name_of_id(&self, id: &str) -> String {
        match id.strip_prefix(ADDING) {
            Some(path) => last_component(path).to_string(),
            None => match self.entries.iter().find(|r| r.guid == id) {
                Some(reg) => reg.display_name(),
                None => id.to_string(),
            },
        }
    }
}

fn adding_id(path: &str) -> String {
    format!("{ADDING}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Answer, Ask, Prober};

    const GRACE: u64 = 30_000;

    fn home(mode: Mode) -> Home {
        Home::new(Config {
            mode,
            launch_grace_ms: GRACE,
            own_folder: None,
        })
    }

    fn canvas(port: u16) -> Canvas {
        Canvas {
            url: format!("http://127.0.0.1:{port}/"),
            port,
        }
    }

    /// A registry entry's `(folder, yaml)`.
    fn entry(
        guid: &str,
        path: &str,
        pid: Option<u32>,
        port: Option<u16>,
        stored_at: i64,
    ) -> (String, String) {
        let mut yaml = format!("guid: {guid}\npath: {path}\nstored_at: {stored_at}\n");
        if let Some(pid) = pid {
            yaml.push_str(&format!("pid: {pid}\n"));
        }
        if let Some(port) = port {
            yaml.push_str(&format!("canvas: http://127.0.0.1:{port}/\n"));
        }
        (format!("folder-{guid}"), yaml)
    }

    fn stopped() -> Probe {
        Probe {
            is_fantastic: true,
            holder: None,
            owns_entry: false,
            canvas: None,
            canvas_up: false,
            guid_matches: false,
        }
    }

    fn running(pid: u32, port: u16) -> Probe {
        Probe {
            is_fantastic: true,
            holder: Some(pid),
            owns_entry: true,
            canvas: Some(canvas(port)),
            canvas_up: true,
            guid_matches: true,
        }
    }

    /// An added folder's probe: held by `pid`, its canvas on `port` if any.
    fn held(pid: u32, port: Option<u16>) -> Probe {
        Probe {
            is_fantastic: true,
            holder: Some(pid),
            owns_entry: true,
            canvas: port.map(canvas),
            canvas_up: port.is_some(),
            guid_matches: true,
        }
    }

    fn jobs(effects: Vec<Effect>) -> Vec<Job> {
        match effects.as_slice() {
            [Effect::Probe { jobs }] => jobs.clone(),
            other => panic!("expected one Probe effect, got {other:?}"),
        }
    }

    fn read(home: &mut Home, now_ms: u64, entries: Vec<(String, String)>) -> Vec<Job> {
        jobs(home.reduce(Event::RegistryRead { now_ms, entries }))
    }

    fn probed(home: &mut Home, now_ms: u64, results: Vec<(&str, Probe)>) -> Vec<Effect> {
        let results = results
            .into_iter()
            .map(|(k, p)| (k.to_string(), p))
            .collect();
        home.reduce(Event::Probed { now_ms, results })
    }

    fn status(home: &Home, id: &str, now_ms: u64) -> Status {
        home.rows(now_ms)
            .into_iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no row {id}"))
            .status
    }

    fn ids(home: &Home) -> Vec<String> {
        home.rows(0).into_iter().map(|r| r.id).collect()
    }

    #[test]
    fn a_tick_reads_the_registry() {
        let mut h = home(Mode::Lock);
        assert_eq!(
            h.reduce(Event::Tick { now_ms: 1 }),
            vec![Effect::ReadRegistry]
        );
    }

    #[test]
    fn an_entry_not_probed_yet_is_starting() {
        let mut h = home(Mode::Lock);
        let jobs = read(&mut h, 0, vec![entry("g1", "/w/a", None, None, 0)]);
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, "g1");
        assert_eq!(jobs[0].mode, Mode::Lock);
        let row = &h.rows(0)[0];
        assert_eq!(
            (row.name.as_str(), row.status.clone(), row.last_seen),
            ("a", Status::Starting, Some(0))
        );
    }

    #[test]
    fn a_launch_is_starting_within_its_grace_then_a_problem() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![entry("g1", "/w/a", None, None, 0)]);
        probed(&mut h, 0, vec![("g1", stopped())]);
        assert_eq!(status(&h, "g1", 0), Status::Stopped);

        let effects = h.reduce(Event::Start {
            id: "g1".into(),
            now_ms: 1_000,
        });
        assert_eq!(
            effects,
            vec![Effect::Spawn {
                path: "/w/a".into()
            }]
        );
        assert_eq!(status(&h, "g1", 1_000), Status::Starting);

        probed(&mut h, 20_000, vec![("g1", stopped())]);
        assert_eq!(status(&h, "g1", 20_000), Status::Starting);
        assert_eq!(h.problem(), None);

        probed(&mut h, 31_000, vec![("g1", stopped())]);
        assert_eq!(status(&h, "g1", 31_000), Status::Stopped);
        assert_eq!(h.problem(), Some("a did not open its canvas."));
    }

    #[test]
    fn a_launch_that_runs_is_dropped() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![entry("g1", "/w/a", None, None, 0)]);
        probed(&mut h, 0, vec![("g1", stopped())]);
        h.reduce(Event::Start {
            id: "g1".into(),
            now_ms: 0,
        });
        read(
            &mut h,
            2_000,
            vec![entry("g1", "/w/a", Some(7), Some(8100), 0)],
        );
        probed(&mut h, 2_000, vec![("g1", running(7, 8100))]);
        assert_eq!(status(&h, "g1", 2_000), Status::Running(canvas(8100)));
        // The launch is gone: a stop now shows at once, not as Starting.
        probed(&mut h, 3_000, vec![("g1", stopped())]);
        assert_eq!(status(&h, "g1", 3_000), Status::Stopped);
        assert_eq!(h.problem(), None);
    }

    #[test]
    fn a_failed_spawn_of_an_add_says_why_at_once() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![]);
        probed(&mut h, 0, vec![]);
        h.reduce(Event::Open {
            paths: vec!["/w/alpha".into()],
            now_ms: 0,
        });
        h.reduce(Event::Spawned {
            path: "/w/alpha".into(),
            result: Err("no login environment: SHELL is not set".into()),
        });
        assert_eq!(
            h.problem(),
            Some("Could not add alpha: it did not start — no login environment: SHELL is not set")
        );
        assert!(ids(&h).is_empty());
    }

    #[test]
    fn a_failed_spawn_is_a_problem() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![entry("g1", "/w/a", None, None, 0)]);
        probed(&mut h, 0, vec![("g1", stopped())]);
        h.reduce(Event::Start {
            id: "g1".into(),
            now_ms: 0,
        });
        h.reduce(Event::Spawned {
            path: "/w/a".into(),
            result: Err("no such binary".into()),
        });
        assert_eq!(status(&h, "g1", 1), Status::Stopped);
        assert_eq!(h.problem(), Some("Could not start a: no such binary"));
    }

    #[test]
    fn stop_then_stopping_then_stopped() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![entry("g1", "/w/a", Some(7), Some(8100), 0)]);
        probed(&mut h, 0, vec![("g1", running(7, 8100))]);
        assert_eq!(
            h.reduce(Event::Stop { id: "g1".into() }),
            vec![Effect::Terminate {
                path: "/w/a".into()
            }]
        );
        h.reduce(Event::Terminated {
            path: "/w/a".into(),
            sent: true,
        });
        assert_eq!(status(&h, "g1", 1), Status::Stopping);
        probed(&mut h, 2, vec![("g1", running(7, 8100))]);
        assert_eq!(status(&h, "g1", 2), Status::Stopping);
        probed(&mut h, 3, vec![("g1", stopped())]);
        assert_eq!(status(&h, "g1", 3), Status::Stopped);
        // A later start is not shown as stopping.
        probed(&mut h, 4, vec![("g1", running(7, 8100))]);
        assert_eq!(status(&h, "g1", 4), Status::Running(canvas(8100)));
    }

    #[test]
    fn a_running_row_rides_out_two_missed_canvas_answers_and_shows_the_third() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![entry("g1", "/w/a", Some(7), Some(8100), 0)]);
        probed(&mut h, 0, vec![("g1", running(7, 8100))]);
        let up = Status::Running(canvas(8100));
        // The same run, its canvas late.
        let mut late = running(7, 8100);
        late.canvas_up = false;
        late.guid_matches = false;
        // Misses 1 and 2: still running, on its canvas.
        probed(&mut h, 2_000, vec![("g1", late.clone())]);
        assert_eq!(status(&h, "g1", 2_000), up);
        probed(&mut h, 4_000, vec![("g1", late.clone())]);
        assert_eq!(status(&h, "g1", 4_000), up);
        // The 3rd in a row: what the probe found.
        probed(&mut h, 6_000, vec![("g1", late.clone())]);
        assert_eq!(status(&h, "g1", 6_000), Status::Unverified);
        // An answer resets the count: two misses are ridden out again.
        probed(&mut h, 8_000, vec![("g1", running(7, 8100))]);
        probed(&mut h, 10_000, vec![("g1", late.clone())]);
        probed(&mut h, 12_000, vec![("g1", late)]);
        assert_eq!(status(&h, "g1", 12_000), up);
        // A gone holder, or another canvas, shows at once.
        probed(&mut h, 14_000, vec![("g1", running(7, 8100))]);
        probed(&mut h, 16_000, vec![("g1", stopped())]);
        assert_eq!(status(&h, "g1", 16_000), Status::Stopped);
        probed(&mut h, 18_000, vec![("g1", running(7, 8100))]);
        let mut moved = running(7, 8200);
        moved.canvas_up = false;
        moved.guid_matches = false;
        probed(&mut h, 20_000, vec![("g1", moved)]);
        assert_eq!(status(&h, "g1", 20_000), Status::Unverified);
    }

    #[test]
    fn a_terminate_not_sent_changes_nothing() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![entry("g1", "/w/a", Some(7), Some(8100), 0)]);
        probed(&mut h, 0, vec![("g1", running(7, 8100))]);
        h.reduce(Event::Terminated {
            path: "/w/a".into(),
            sent: false,
        });
        assert_eq!(status(&h, "g1", 1), Status::Running(canvas(8100)));
    }

    #[test]
    fn adding_a_running_folder_registers_it_and_swaps_its_page() {
        let mut h = home(Mode::Canvas);
        read(&mut h, 0, vec![]);
        probed(&mut h, 0, vec![]);
        let effects = h.reduce(Event::Open {
            paths: vec!["/w/new".into()],
            now_ms: 100,
        });
        assert_eq!(effects, vec![], "the sandboxed app starts nothing");
        assert_eq!(h.pages(), ["adding:/w/new"]);
        assert_eq!(h.selection(), Some("adding:/w/new"));
        let row = &h.rows(100)[0];
        assert_eq!(
            (row.name.as_str(), row.status.clone(), row.guid.clone()),
            ("new", Status::Starting, None)
        );

        let jobs = read(&mut h, 200, vec![]);
        assert_eq!(
            jobs,
            vec![Job {
                key: "adding:/w/new".into(),
                target: Target::Adding {
                    path: "/w/new".into()
                },
                mode: Mode::Canvas
            }]
        );
        let effects = probed(&mut h, 200, vec![("adding:/w/new", held(9, Some(8200)))]);
        assert_eq!(
            effects,
            vec![Effect::Register {
                path: "/w/new".into(),
                port: 8200
            }]
        );
        // Not asked again while the first is out.
        assert_eq!(
            probed(&mut h, 300, vec![("adding:/w/new", held(9, Some(8200)))]),
            vec![]
        );
        h.reduce(Event::RegisterDone {
            path: "/w/new".into(),
            result: Ok(()),
        });

        let jobs = read(
            &mut h,
            400,
            vec![entry("g9", "/w/new", Some(9), Some(8200), 5)],
        );
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].key, "g9");
        assert_eq!(h.pages(), ["g9"]);
        assert_eq!(h.selection(), Some("g9"));
        assert_eq!(ids(&h), ["g9"]);
    }

    #[test]
    fn adding_in_lock_mode_starts_it_and_registers_its_canvas() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![]);
        probed(&mut h, 0, vec![]);
        let effects = h.reduce(Event::Open {
            paths: vec!["/w/new".into()],
            now_ms: 0,
        });
        assert_eq!(
            effects,
            vec![Effect::Spawn {
                path: "/w/new".into()
            }]
        );
        read(&mut h, 1_000, vec![]);
        // Held, but its canvas not up yet: wait.
        let mut not_up = held(9, Some(8200));
        not_up.canvas_up = false;
        assert_eq!(
            probed(&mut h, 1_000, vec![("adding:/w/new", not_up)]),
            vec![]
        );
        // Started by us and no canvas yet: not a problem to remember.
        assert_eq!(
            probed(&mut h, 1_500, vec![("adding:/w/new", held(9, None))]),
            vec![]
        );
        let effects = probed(&mut h, 2_000, vec![("adding:/w/new", held(9, Some(8200)))]);
        assert_eq!(
            effects,
            vec![Effect::Register {
                path: "/w/new".into(),
                port: 8200
            }]
        );
    }

    #[test]
    fn a_failed_register_is_the_deadline_reason() {
        let mut h = home(Mode::Canvas);
        read(&mut h, 0, vec![]);
        probed(&mut h, 0, vec![]);
        h.reduce(Event::Open {
            paths: vec!["/w/new".into()],
            now_ms: 0,
        });
        probed(&mut h, 10, vec![("adding:/w/new", held(9, Some(8200)))]);
        h.reduce(Event::RegisterDone {
            path: "/w/new".into(),
            result: Err("the kernel refused".into()),
        });
        // Asked again after a failure.
        let effects = probed(&mut h, 20, vec![("adding:/w/new", held(9, Some(8200)))]);
        assert_eq!(
            effects,
            vec![Effect::Register {
                path: "/w/new".into(),
                port: 8200
            }]
        );
        h.reduce(Event::RegisterDone {
            path: "/w/new".into(),
            result: Err("the kernel refused".into()),
        });
        probed(&mut h, GRACE, vec![]);
        assert_eq!(h.problem(), Some("Could not add new: the kernel refused"));
        assert!(ids(&h).is_empty());
        assert!(h.pages().is_empty());
        assert_eq!(h.selection(), None);
    }

    #[test]
    fn the_adding_deadline_and_a_folder_without_a_canvas() {
        let mut h = home(Mode::Lock);
        read(&mut h, 0, vec![]);
        probed(&mut h, 0, vec![]);
        // First open: nothing holds it as far as the home knows, so it starts it.
        let effects = h.reduce(Event::Open {
            paths: vec!["/w/term".into()],
            now_ms: 0,
        });
        assert_eq!(
            effects,
            vec![Effect::Spawn {
                path: "/w/term".into()
            }]
        );
        probed(&mut h, 1_000, vec![("adding:/w/term", held(5, None))]);
        probed(&mut h, GRACE, vec![]);
        assert_eq!(h.problem(), Some("Could not add term: it did not start."));
        assert!(ids(&h).is_empty());

        // Opened again: its last probe shows a holder, so it is not started.
        let effects = h.reduce(Event::Open {
            paths: vec!["/w/term".into()],
            now_ms: 40_000,
        });
        assert_eq!(effects, vec![]);
        assert_eq!(h.selection(), Some("adding:/w/term"));
        probed(&mut h, 41_000, vec![("adding:/w/term", held(5, None))]);
        probed(&mut h, 40_000 + GRACE, vec![]);
        assert_eq!(
            h.problem(),
            Some("Could not add term: it runs without a canvas (a terminal, or headless). Stop it there, then add it again.")
        );
    }

    #[test]
    fn a_verified_run_is_not_asked_its_guid_again() {
        let mut h = home(Mode::Lock);
        let jobs = read(&mut h, 0, vec![entry("g1", "/w/a", Some(7), Some(8100), 0)]);
        assert!(matches!(
            &jobs[0].target,
            Target::Registered {
                verified: false,
                ..
            }
        ));
        probed(&mut h, 0, vec![("g1", running(7, 8100))]);

        let jobs = read(&mut h, 1, vec![entry("g1", "/w/a", Some(7), Some(8100), 0)]);
        let job = jobs[0].clone();
        assert!(matches!(
            &job.target,
            Target::Registered { verified: true, .. }
        ));
        let mut asked = Vec::new();
        let probe = Prober::new(job.target, job.mode).run(|ask| {
            asked.push(ask.clone());
            match ask {
                Ask::IsFantastic { .. } => Answer::IsFantastic(true),
                Ask::LockHolder { .. } => Answer::LockHolder(Some(7)),
                Ask::CanvasUp { .. } => Answer::CanvasUp(true),
                other => panic!("asked {other:?}"),
            }
        });
        assert!(is_active(&probe));
        assert!(!asked.iter().any(|a| matches!(a, Ask::Guid { .. })));

        // Still verified after a probe that is not active: the same run.
        probed(&mut h, 2, vec![("g1", stopped())]);
        let jobs = read(&mut h, 3, vec![entry("g1", "/w/a", Some(7), Some(8100), 0)]);
        assert!(matches!(
            &jobs[0].target,
            Target::Registered { verified: true, .. }
        ));

        // A new run (another pid) is asked again.
        let jobs = read(&mut h, 4, vec![entry("g1", "/w/a", Some(8), Some(8100), 0)]);
        assert!(matches!(
            &jobs[0].target,
            Target::Registered {
                verified: false,
                ..
            }
        ));
        probed(&mut h, 5, vec![("g1", stopped())]);
        let jobs = read(&mut h, 6, vec![entry("g1", "/w/a", Some(7), Some(8100), 0)]);
        assert!(matches!(
            &jobs[0].target,
            Target::Registered {
                verified: false,
                ..
            }
        ));
    }

    #[test]
    fn canvas_mode_never_starts_or_stops() {
        let mut h = home(Mode::Canvas);
        read(
            &mut h,
            0,
            vec![
                entry("g1", "/w/a", Some(7), Some(8100), 0),
                entry("g2", "/w/b", None, None, 1),
            ],
        );
        probed(&mut h, 0, vec![("g1", running(7, 8100)), ("g2", stopped())]);

        assert_eq!(
            h.reduce(Event::Start {
                id: "g2".into(),
                now_ms: 1
            }),
            vec![]
        );
        assert_eq!(
            h.problem(),
            Some("b can only be started from the full app.")
        );
        assert_eq!(h.reduce(Event::Stop { id: "g1".into() }), vec![]);
        assert_eq!(
            h.problem(),
            Some("a can only be stopped from the full app.")
        );

        let effects = h.reduce(Event::Open {
            paths: vec!["/w/b".into(), "/w/a".into(), "/w/c".into()],
            now_ms: 2,
        });
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Spawn { .. } | Effect::Terminate { .. })),
            "{effects:?}"
        );
        assert_eq!(h.problem(), Some("b isn't running"));
        assert_eq!(h.pages(), ["g1", "adding:/w/c"]);
        assert_eq!(h.selection(), Some("adding:/w/c"));
    }

    #[test]
    fn lock_mode_opens_and_starts_a_stopped_entry() {
        let mut h = home(Mode::Lock);
        read(
            &mut h,
            0,
            vec![
                entry("g1", "/w/a", None, None, 0),
                entry("g2", "/w/b", Some(7), Some(8100), 1),
            ],
        );
        probed(&mut h, 0, vec![("g1", stopped()), ("g2", running(7, 8100))]);
        let effects = h.reduce(Event::Open {
            paths: vec!["/w/a".into(), "/w/b".into()],
            now_ms: 5,
        });
        assert_eq!(
            effects,
            vec![Effect::Spawn {
                path: "/w/a".into()
            }]
        );
        assert_eq!(h.pages(), ["g1", "g2"]);
        assert_eq!(h.selection(), Some("g2"));
        assert_eq!(status(&h, "g1", 5), Status::Starting);
    }

    #[test]
    fn opens_before_the_first_listing_wait_for_it() {
        let mut h = home(Mode::Lock);
        assert_eq!(
            h.reduce(Event::Open {
                paths: vec!["/w/a".into()],
                now_ms: 0
            }),
            vec![]
        );
        assert!(h.pages().is_empty());
        assert!(!h.listed());
        read(&mut h, 1, vec![entry("g1", "/w/a", None, None, 0)]);
        let effects = probed(&mut h, 1, vec![("g1", stopped())]);
        assert!(h.listed());
        assert_eq!(
            effects,
            vec![Effect::Spawn {
                path: "/w/a".into()
            }]
        );
        assert_eq!(h.pages(), ["g1"]);
        // Only the first listing opens what waited.
        assert_eq!(probed(&mut h, 2, vec![("g1", stopped())]), vec![]);
    }

    #[test]
    fn forget_routes_through_a_running_kernel() {
        let mut h = home(Mode::Lock);
        let all = vec![
            entry("g1", "/w/a", Some(7), Some(8100), 0),
            entry("g2", "/w/b", None, None, 1),
        ];
        read(&mut h, 0, all.clone());
        probed(&mut h, 0, vec![("g1", running(7, 8100)), ("g2", stopped())]);

        assert_eq!(
            h.reduce(Event::Forget { id: "g1".into() }),
            vec![Effect::SwitchOff { port: 8100 }]
        );
        assert_eq!(
            h.reduce(Event::Forget { id: "g2".into() }),
            vec![Effect::Unstore {
                port: 8100,
                guid: "g2".into()
            }]
        );

        probed(&mut h, 1, vec![("g1", stopped()), ("g2", stopped())]);
        assert_eq!(h.reduce(Event::Forget { id: "g2".into() }), vec![]);
        assert_eq!(
            h.problem(),
            Some("Start any fantastic to remove b — only a running one can change the list.")
        );

        h.reduce(Event::Select {
            id: Some("g2".into()),
        });
        h.reduce(Event::Forgot {
            id: "g2".into(),
            result: Err("not stored".into()),
        });
        assert_eq!(h.problem(), Some("Could not remove b: not stored"));
        assert_eq!(h.pages(), ["g2"]);
        h.reduce(Event::Forgot {
            id: "g2".into(),
            result: Ok(()),
        });
        assert!(h.pages().is_empty());
        assert_eq!(h.selection(), None);
    }

    #[test]
    fn page_bookkeeping() {
        let mut h = home(Mode::Lock);
        let all = vec![
            entry("g1", "/w/a", None, None, 0),
            entry("g2", "/w/b", None, None, 1),
            entry("g3", "/w/c", None, None, 2),
        ];
        read(&mut h, 0, all.clone());
        for id in ["g1", "g2", "g3", "g2"] {
            h.reduce(Event::Select {
                id: Some(id.into()),
            });
        }
        assert_eq!(h.pages(), ["g1", "g2", "g3"]);
        assert_eq!(h.selection(), Some("g2"));

        h.reduce(Event::Select {
            id: Some("nope".into()),
        });
        assert_eq!(h.pages(), ["g1", "g2", "g3"]);
        h.reduce(Event::SelectPage { index: 7 });
        assert_eq!(h.selection(), Some("g2"));
        h.reduce(Event::SelectPage { index: 0 });
        assert_eq!(h.selection(), Some("g1"));
        h.reduce(Event::Select { id: None });
        assert_eq!(h.selection(), None);

        // Closing the selected page selects the next, else the previous, else Home.
        h.reduce(Event::SelectPage { index: 1 });
        h.reduce(Event::ClosePage { id: "g2".into() });
        assert_eq!(
            (h.pages(), h.selection()),
            (&["g1".to_string(), "g3".to_string()][..], Some("g3"))
        );
        h.reduce(Event::ClosePage { id: "g3".into() });
        assert_eq!(h.selection(), Some("g1"));
        // Closing a page that is not shown keeps the selection.
        h.reduce(Event::Select {
            id: Some("g3".into()),
        });
        h.reduce(Event::ClosePage { id: "g1".into() });
        assert_eq!(
            (h.pages(), h.selection()),
            (&["g3".to_string()][..], Some("g3"))
        );
        h.reduce(Event::ClosePage { id: "g3".into() });
        assert_eq!(h.selection(), None);

        // A page whose entry leaves the registry closes.
        h.reduce(Event::Select {
            id: Some("g1".into()),
        });
        h.reduce(Event::Select {
            id: Some("g3".into()),
        });
        read(&mut h, 1, vec![all[0].clone(), all[1].clone()]);
        assert_eq!(
            (h.pages(), h.selection()),
            (&["g1".to_string()][..], Some("g1"))
        );
    }

    #[test]
    fn an_adding_page_whose_guid_has_a_page_is_removed() {
        let mut h = home(Mode::Canvas);
        read(
            &mut h,
            0,
            vec![entry("g1", "/w/old", Some(7), Some(8100), 0)],
        );
        probed(&mut h, 0, vec![("g1", running(7, 8100))]);
        h.reduce(Event::Select {
            id: Some("g1".into()),
        });
        h.reduce(Event::Open {
            paths: vec!["/w/new".into()],
            now_ms: 1,
        });
        assert_eq!(h.pages(), ["g1", "adding:/w/new"]);
        read(
            &mut h,
            2,
            vec![entry("g1", "/w/new", Some(7), Some(8100), 0)],
        );
        assert_eq!(h.pages(), ["g1"]);
        assert_eq!(h.selection(), Some("g1"));
    }

    #[test]
    fn rows_list_the_registry_then_the_folders_being_added() {
        let mut h = home(Mode::Canvas);
        read(
            &mut h,
            0,
            vec![
                entry("g2", "/w/b", Some(7), Some(8100), 5),
                entry("g1", "/w/a", None, None, 1),
            ],
        );
        probed(&mut h, 0, vec![("g2", running(7, 8100))]);
        h.reduce(Event::Open {
            paths: vec!["/w/z/".into()],
            now_ms: 0,
        });
        let rows = h.rows(0);
        let got: Vec<(&str, &str, Option<u32>)> = rows
            .iter()
            .map(|r| (r.id.as_str(), r.name.as_str(), r.pid))
            .collect();
        assert_eq!(
            got,
            vec![
                ("g1", "a", None),
                ("g2", "b", Some(7)),
                ("adding:/w/z/", "z", None)
            ]
        );
    }

    #[test]
    fn dismiss_clears_the_problem() {
        let mut h = home(Mode::Lock);
        h.reduce(Event::RegistryFailed {
            error: "denied".into(),
        });
        assert!(h.problem().is_some());
        assert_eq!(h.reduce(Event::Dismiss), vec![]);
        assert_eq!(h.problem(), None);
        // Nothing to dismiss is still nothing.
        assert_eq!(h.reduce(Event::Dismiss), vec![]);
        assert_eq!(h.problem(), None);
    }

    #[test]
    fn a_registry_that_cannot_be_read_is_a_problem() {
        let mut h = home(Mode::Lock);
        assert_eq!(
            h.reduce(Event::RegistryFailed {
                error: "/h/fantastics: permission denied".into()
            }),
            vec![]
        );
        assert_eq!(
            h.problem(),
            Some("Could not read your fantastics: /h/fantastics: permission denied")
        );
        assert!(!h.listed());
    }

    #[test]
    fn a_stop_that_cannot_be_sent_is_a_problem() {
        let mut h = home(Mode::Lock);
        let mut notes = entry("g1", "/w/a", Some(7), Some(8100), 0);
        notes.1.push_str("name: Notes\n");
        read(&mut h, 0, vec![notes]);
        probed(&mut h, 0, vec![("g1", running(7, 8100))]);
        h.reduce(Event::Stop { id: "g1".into() });
        assert_eq!(
            h.reduce(Event::StopFailed {
                path: "/w/a".into(),
                error: "not yours to stop".into()
            }),
            vec![]
        );
        assert_eq!(h.problem(), Some("Could not stop Notes: not yours to stop"));
        assert_eq!(status(&h, "g1", 1), Status::Running(canvas(8100)));
        // A folder the registry does not list is named by its last component.
        h.reduce(Event::StopFailed {
            path: "/w/other/".into(),
            error: "e".into(),
        });
        assert_eq!(h.problem(), Some("Could not stop other: e"));
    }

    #[test]
    fn row_actions_by_status_and_mode() {
        let row = |status: Status, pid: Option<u32>, guid: bool| Row {
            id: "x".into(),
            guid: guid.then(|| "g".to_string()),
            name: "x".into(),
            path: "/w/x".into(),
            last_seen: None,
            status,
            pid,
        };
        let up = Status::Running(canvas(8100));
        let table: Vec<(Row, Vec<&str>, Vec<&str>)> = vec![
            // (row, Lock, Canvas)
            (
                row(Status::Stopped, None, true),
                vec!["start", "open", "remove"],
                vec!["remove"],
            ),
            (
                row(Status::Stopped, None, false),
                vec!["start", "open"],
                vec!["open"],
            ),
            (
                row(Status::Starting, None, true),
                vec!["open", "remove"],
                vec!["open", "remove"],
            ),
            (
                row(Status::Starting, Some(7), true),
                vec!["stop", "open", "remove"],
                vec!["open", "remove"],
            ),
            (
                row(Status::Starting, None, false),
                vec!["open"],
                vec!["open"],
            ),
            (
                row(Status::Starting, Some(7), false),
                vec!["stop", "open"],
                vec!["open"],
            ),
            (
                row(Status::Stopping, Some(7), true),
                vec!["open", "remove"],
                vec!["open", "remove"],
            ),
            (
                row(up, Some(7), true),
                vec!["stop", "open", "remove"],
                vec!["open", "remove"],
            ),
            (
                row(Status::NoCanvas, Some(7), true),
                vec!["stop", "open", "remove"],
                vec!["open", "remove"],
            ),
            (
                row(Status::Unverified, Some(7), true),
                vec!["stop", "open", "remove"],
                vec!["open", "remove"],
            ),
            (
                row(Status::Missing, None, true),
                vec!["open", "remove"],
                vec!["open", "remove"],
            ),
        ];
        for (r, lock, canvas_mode) in table {
            assert_eq!(
                r.actions(Mode::Lock),
                lock,
                "{:?} pid {:?} guid {:?} in Lock",
                r.status,
                r.pid,
                r.guid
            );
            assert_eq!(
                r.actions(Mode::Canvas),
                canvas_mode,
                "{:?} pid {:?} guid {:?} in Canvas",
                r.status,
                r.pid,
                r.guid
            );
        }
    }

    #[test]
    fn restore_waits_for_the_first_registry_read_and_keeps_only_listed_pages() {
        let mut h = home(Mode::Lock);
        let restore = Event::Restore {
            pages: vec![
                "g2".into(),
                "gone".into(),
                "adding:/w/c".into(),
                "g1".into(),
            ],
            selection: Some("g1".into()),
        };
        assert_eq!(h.reduce(restore), vec![]);
        // Held: no entry can be checked yet, so nothing is open.
        assert!(h.pages().is_empty());
        assert_eq!(h.selection(), None);
        h.reduce(Event::Tick { now_ms: 1 });
        assert!(h.pages().is_empty());
        read(
            &mut h,
            1,
            vec![
                entry("g1", "/w/a", None, None, 0),
                entry("g2", "/w/b", None, None, 1),
            ],
        );
        assert_eq!(h.pages(), ["g2", "g1"]);
        assert_eq!(h.selection(), Some("g1"));
        // Applied once: a later read does not bring back what was dropped.
        h.reduce(Event::ClosePage { id: "g2".into() });
        read(
            &mut h,
            2,
            vec![
                entry("g1", "/w/a", None, None, 0),
                entry("g2", "/w/b", None, None, 1),
            ],
        );
        assert_eq!(h.pages(), ["g1"]);
    }

    #[test]
    fn restore_after_the_first_read_applies_at_once() {
        let mut h = home(Mode::Lock);
        read(
            &mut h,
            0,
            vec![
                entry("g1", "/w/a", None, None, 0),
                entry("g2", "/w/b", None, None, 1),
            ],
        );
        h.reduce(Event::Select {
            id: Some("g1".into()),
        });
        h.reduce(Event::Restore {
            pages: vec!["g2".into(), "nope".into()],
            selection: Some("g2".into()),
        });
        assert_eq!(h.pages(), ["g2"]);
        assert_eq!(h.selection(), Some("g2"));
    }

    #[test]
    fn a_restored_selection_that_is_not_a_kept_page_is_the_home() {
        let mut h = home(Mode::Lock);
        h.reduce(Event::Restore {
            pages: vec!["g1".into()],
            selection: Some("gone".into()),
        });
        read(&mut h, 0, vec![entry("g1", "/w/a", None, None, 0)]);
        assert_eq!(h.pages(), ["g1"]);
        assert_eq!(h.selection(), None);

        // A selection of an entry that is listed but not among the kept pages.
        let mut h = home(Mode::Lock);
        h.reduce(Event::Restore {
            pages: vec!["g1".into()],
            selection: Some("g2".into()),
        });
        read(
            &mut h,
            0,
            vec![
                entry("g1", "/w/a", None, None, 0),
                entry("g2", "/w/b", None, None, 1),
            ],
        );
        assert_eq!(h.pages(), ["g1"]);
        assert_eq!(h.selection(), None);

        // No selection kept is the home too.
        let mut h = home(Mode::Lock);
        h.reduce(Event::Restore {
            pages: vec!["g1".into()],
            selection: None,
        });
        read(&mut h, 0, vec![entry("g1", "/w/a", None, None, 0)]);
        assert_eq!(h.selection(), None);
    }

    #[test]
    fn an_open_queued_before_the_first_look_wins_the_selection_over_a_restore() {
        let mut h = home(Mode::Lock);
        h.reduce(Event::Restore {
            pages: vec!["g1".into()],
            selection: Some("g1".into()),
        });
        h.reduce(Event::Open {
            paths: vec!["/w/b".into()],
            now_ms: 0,
        });
        read(
            &mut h,
            1,
            vec![
                entry("g1", "/w/a", None, None, 0),
                entry("g2", "/w/b", None, None, 1),
            ],
        );
        // The restore is in; the open waits for the first look to finish.
        assert_eq!(h.pages(), ["g1"]);
        assert_eq!(h.selection(), Some("g1"));
        probed(&mut h, 1, vec![("g1", stopped()), ("g2", running(7, 8100))]);
        assert_eq!(h.pages(), ["g1", "g2"]);
        assert_eq!(h.selection(), Some("g2"));
    }

    #[test]
    fn the_own_folder_is_never_a_row() {
        let mut h = Home::new(Config::new(Mode::Lock).own_folder("/h".into()));
        let jobs = read(
            &mut h,
            0,
            vec![
                entry("own", "/h", Some(7), Some(8100), 0),
                entry("g1", "/w/a", None, None, 1),
            ],
        );
        assert_eq!(
            jobs.iter().map(|j| j.key.as_str()).collect::<Vec<_>>(),
            ["g1"]
        );
        probed(&mut h, 0, vec![("g1", stopped())]);
        assert_eq!(ids(&h), ["g1"]);
        // A page of it cannot be restored either.
        h.reduce(Event::Restore {
            pages: vec!["own".into()],
            selection: Some("own".into()),
        });
        assert!(h.pages().is_empty());
        assert_eq!(h.selection(), None);
    }
}
