//! A scripted machine for the tests: the registry's text, per-path answers,
//! spawn and terminate results, recorded `forward` calls,
//! a clock, and a count of each kind of question.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use fantastic_home::{Answer, Ask, Canvas, CanvasRun};
use serde_json::Value;

use super::os::HomeOs;

/// One folder as the fake machine answers for it.
#[derive(Debug, Clone, Default)]
pub struct Folder {
    pub is_fantastic: bool,
    pub holder: Option<u32>,
    pub run: Option<CanvasRun>,
}

/// One `forward` the home made.
#[derive(Debug, Clone, PartialEq)]
pub struct Forwarded {
    pub port: u16,
    pub target: String,
    pub payload: Value,
}

struct State {
    now_ms: u64,
    registry: Result<Vec<(String, String)>, String>,
    folders: BTreeMap<String, Folder>,
    canvases: BTreeMap<u16, bool>,
    guids: BTreeMap<u16, String>,
    spawn: BTreeMap<String, Result<(), String>>,
    terminate: BTreeMap<String, Result<bool, String>>,
    forward_replies: BTreeMap<(u16, String), Result<Value, String>>,
    forwarded: Vec<Forwarded>,
    spawned: Vec<String>,
    terminated: Vec<String>,
    asked: BTreeMap<&'static str, usize>,
    /// `folder()`: a path → the folder it names, or why it is not one.
    resolve: BTreeMap<String, Result<String, String>>,
    logs: BTreeMap<String, (String, String)>,
    /// While set, `read_registry` waits for `release` before it answers.
    hold_registry: bool,
}

/// The fake. Every setter takes `&self`, so a test scripts it while the runner holds it.
pub struct FakeOs {
    state: Mutex<State>,
    released: tokio::sync::Notify,
}

impl FakeOs {
    pub fn new() -> FakeOs {
        FakeOs {
            state: Mutex::new(State {
                now_ms: 0,
                registry: Ok(Vec::new()),
                folders: BTreeMap::new(),
                canvases: BTreeMap::new(),
                guids: BTreeMap::new(),
                spawn: BTreeMap::new(),
                terminate: BTreeMap::new(),
                forward_replies: BTreeMap::new(),
                forwarded: Vec::new(),
                spawned: Vec::new(),
                terminated: Vec::new(),
                asked: BTreeMap::new(),
                resolve: BTreeMap::new(),
                logs: BTreeMap::new(),
                hold_registry: false,
            }),
            released: tokio::sync::Notify::new(),
        }
    }

    /// The machine a contract fixture describes (`contract/home/v1/cases.json`,
    /// `fixture`). Every key is read strictly: an unknown one panics, naming it.
    ///
    /// ```text
    ///   now_ms     the clock
    ///   registry   {folder: fantastic.yaml text}, in registry order
    ///   folders    {path: {fantastic, holder, run?: {pid, port, url}, canvas_up?, guid?}};
    ///              each path resolves to itself; canvas_up and guid answer for the run's port
    ///   spawn      {path: "ok" | why it failed}
    ///   forward    {"<port> <target>": reply}
    ///   logs       {path: {file, tail}}
    /// ```
    ///
    /// `own_folder` is the runner's config, not the machine's: it is skipped here.
    pub fn from_fixture(fixture: &Value) -> FakeOs {
        let os = FakeOs::new();
        let obj = |v: &Value, at: &str| -> serde_json::Map<String, Value> {
            v.as_object()
                .unwrap_or_else(|| panic!("fixture: {at} must be an object, got {v}"))
                .clone()
        };
        let str_of = |v: &Value, at: &str| -> String {
            v.as_str()
                .unwrap_or_else(|| panic!("fixture: {at} must be a string, got {v}"))
                .to_string()
        };
        for (key, value) in obj(fixture, "the fixture") {
            match key.as_str() {
                "own_folder" => {}
                "now_ms" => os.set_now(
                    value
                        .as_u64()
                        .unwrap_or_else(|| panic!("fixture: now_ms must be an integer")),
                ),
                "registry" => os.set_registry(
                    obj(&value, "registry")
                        .into_iter()
                        .map(|(folder, text)| {
                            let text = str_of(&text, &format!("registry.{folder}"));
                            (folder, text)
                        })
                        .collect(),
                ),
                "folders" => {
                    for (path, f) in obj(&value, "folders") {
                        let at = format!("folders.{path}");
                        let mut folder = Folder::default();
                        let (mut up, mut guid) = (None, None);
                        for (k, v) in obj(&f, &at) {
                            match k.as_str() {
                                "fantastic" => {
                                    folder.is_fantastic = v
                                        .as_bool()
                                        .unwrap_or_else(|| panic!("{at}.fantastic: a bool"))
                                }
                                "holder" => {
                                    folder.holder = match &v {
                                        Value::Null => None,
                                        v => Some(
                                            v.as_u64()
                                                .and_then(|p| u32::try_from(p).ok())
                                                .unwrap_or_else(|| panic!("{at}.holder: a pid")),
                                        ),
                                    }
                                }
                                "run" => {
                                    let r = obj(&v, &format!("{at}.run"));
                                    let num = |k: &str| {
                                        r.get(k)
                                            .and_then(Value::as_u64)
                                            .unwrap_or_else(|| panic!("{at}.run.{k}: an integer"))
                                    };
                                    let url = str_of(&r["url"], &format!("{at}.run.url"));
                                    let canvas = Canvas::local(&url)
                                        .unwrap_or_else(|| panic!("{at}.run.url: not local"));
                                    folder.run = Some(CanvasRun {
                                        pid: u32::try_from(num("pid")).expect("a pid"),
                                        port: u16::try_from(num("port")).expect("a port"),
                                        canvas,
                                    });
                                }
                                "canvas_up" => {
                                    up = Some(
                                        v.as_bool()
                                            .unwrap_or_else(|| panic!("{at}.canvas_up: a bool")),
                                    )
                                }
                                "guid" => guid = Some(str_of(&v, &format!("{at}.guid"))),
                                other => panic!("fixture: {at}: unknown key `{other}`"),
                            }
                        }
                        if up.is_some() || guid.is_some() {
                            let run = folder.run.as_ref().unwrap_or_else(|| {
                                panic!("{at}: canvas_up and guid answer for its run: give `run`")
                            });
                            if let Some(up) = up {
                                os.set_canvas_up(run.port, up);
                            }
                            if let Some(guid) = &guid {
                                os.set_guid(run.port, guid);
                            }
                        }
                        os.set_resolve(&path, Ok(path.clone()));
                        os.set_folder(&path, folder);
                    }
                }
                "spawn" => {
                    for (path, result) in obj(&value, "spawn") {
                        let result = str_of(&result, &format!("spawn.{path}"));
                        os.set_spawn(&path, if result == "ok" { Ok(()) } else { Err(result) });
                    }
                }
                "forward" => {
                    for (key, reply) in obj(&value, "forward") {
                        let (port, target) = key
                            .split_once(' ')
                            .and_then(|(p, t)| Some((p.parse::<u16>().ok()?, t)))
                            .unwrap_or_else(|| {
                                panic!("fixture: forward.{key}: \"<port> <target>\"")
                            });
                        os.set_forward_reply(port, target, Ok(reply));
                    }
                }
                "logs" => {
                    for (path, log) in obj(&value, "logs") {
                        let at = format!("logs.{path}");
                        let log = obj(&log, &at);
                        os.set_log(
                            &path,
                            &str_of(&log["file"], &format!("{at}.file")),
                            &str_of(&log["tail"], &format!("{at}.tail")),
                        );
                    }
                }
                other => panic!("fixture: unknown key `{other}`"),
            }
        }
        os
    }

    fn s(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("the fake is poisoned")
    }

    pub fn set_registry(&self, entries: Vec<(String, String)>) {
        self.s().registry = Ok(entries);
    }
    pub fn set_folder(&self, path: &str, folder: Folder) {
        self.s().folders.insert(path.to_string(), folder);
    }
    pub fn set_canvas_up(&self, port: u16, up: bool) {
        self.s().canvases.insert(port, up);
    }
    pub fn set_guid(&self, port: u16, guid: &str) {
        self.s().guids.insert(port, guid.to_string());
    }
    pub fn set_spawn(&self, path: &str, result: Result<(), String>) {
        self.s().spawn.insert(path.to_string(), result);
    }
    pub fn set_terminate(&self, path: &str, result: Result<bool, String>) {
        self.s().terminate.insert(path.to_string(), result);
    }
    pub fn set_resolve(&self, path: &str, folder: Result<String, String>) {
        self.s().resolve.insert(path.to_string(), folder);
    }
    pub fn set_now(&self, now_ms: u64) {
        self.s().now_ms = now_ms;
    }
    pub fn set_forward_reply(&self, port: u16, target: &str, reply: Result<Value, String>) {
        self.s()
            .forward_replies
            .insert((port, target.to_string()), reply);
    }
    pub fn set_log(&self, path: &str, file: &str, tail: &str) {
        self.s()
            .logs
            .insert(path.to_string(), (file.to_string(), tail.to_string()));
    }
    /// `read_registry` waits from now on, until [`Self::release`].
    pub fn hold_registry(&self) {
        self.s().hold_registry = true;
    }
    pub fn release(&self) {
        self.s().hold_registry = false;
        self.released.notify_waiters();
    }

    pub fn forwarded(&self) -> Vec<Forwarded> {
        self.s().forwarded.clone()
    }
    pub fn spawned(&self) -> Vec<String> {
        self.s().spawned.clone()
    }
    pub fn terminated(&self) -> Vec<String> {
        self.s().terminated.clone()
    }
    /// How many questions of `kind` were asked: `registry`, `is_fantastic`,
    /// `holder`, `run_file`, `canvas_up`, `guid`.
    pub fn asked(&self, kind: &str) -> usize {
        self.s().asked.get(kind).copied().unwrap_or(0)
    }

    fn count(&self, kind: &'static str) {
        *self.s().asked.entry(kind).or_insert(0) += 1;
    }
}

#[async_trait::async_trait]
impl HomeOs for FakeOs {
    fn now_ms(&self) -> u64 {
        self.s().now_ms
    }

    async fn read_registry(&self) -> Result<Vec<(String, String)>, String> {
        self.count("registry");
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            if !self.s().hold_registry {
                break;
            }
            released.await;
        }
        self.s().registry.clone()
    }

    async fn answer(&self, ask: &Ask) -> Answer {
        let folder = |path: &str| self.s().folders.get(path).cloned().unwrap_or_default();
        match ask {
            Ask::IsFantastic { path } => {
                self.count("is_fantastic");
                Answer::IsFantastic(folder(path).is_fantastic)
            }
            Ask::LockHolder { path } => {
                self.count("holder");
                Answer::LockHolder(folder(path).holder)
            }
            Ask::RunFile { path } => {
                self.count("run_file");
                Answer::RunFile(folder(path).run)
            }
            Ask::CanvasUp { canvas } => {
                self.count("canvas_up");
                let Canvas { port, .. } = canvas;
                Answer::CanvasUp(self.s().canvases.get(port).copied().unwrap_or(false))
            }
            Ask::Guid { port } => {
                self.count("guid");
                Answer::Guid(self.s().guids.get(port).cloned())
            }
        }
    }

    async fn spawn(&self, path: &str) -> Result<(), String> {
        let mut s = self.s();
        s.spawned.push(path.to_string());
        s.spawn.get(path).cloned().unwrap_or(Ok(()))
    }

    async fn terminate(&self, path: &str) -> Result<bool, String> {
        let mut s = self.s();
        s.terminated.push(path.to_string());
        s.terminate.get(path).cloned().unwrap_or(Ok(true))
    }

    async fn forward(
        &self,
        port: u16,
        target: &str,
        payload: Value,
        _timeout: Duration,
    ) -> Result<Value, String> {
        let mut s = self.s();
        s.forwarded.push(Forwarded {
            port,
            target: target.to_string(),
            payload,
        });
        s.forward_replies
            .get(&(port, target.to_string()))
            .cloned()
            .unwrap_or_else(|| Ok(serde_json::json!({})))
    }

    fn folder(&self, path: &str) -> Result<String, String> {
        self.s()
            .resolve
            .get(path)
            .cloned()
            .unwrap_or_else(|| Err(format!("{path}: no such folder")))
    }

    fn log_file(&self, path: &str) -> Option<String> {
        self.s().logs.get(path).map(|(file, _)| file.clone())
    }

    fn log_tail(&self, path: &str) -> Option<String> {
        self.s().logs.get(path).map(|(_, tail)| tail.clone())
    }
}

/// A run file for a canvas on `port`, held by `pid`.
pub fn run(pid: u32, port: u16) -> CanvasRun {
    CanvasRun {
        pid,
        port,
        canvas: canvas(port),
    }
}

pub fn canvas(port: u16) -> Canvas {
    Canvas {
        url: format!("http://127.0.0.1:{port}/canvas_page/file/index.html"),
        port,
    }
}
