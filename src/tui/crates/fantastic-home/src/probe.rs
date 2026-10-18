//! WHAT ONE FANTASTIC LOOKS LIKE, asked one question at a time.
//!
//! A `Prober` holds one procedure: `next` is the question it needs answered
//! (`None` once it is done), `answer` feeds it the OS layer's answer, `probe` is
//! what is known so far. The questions:
//!
//! ```text
//!   IsFantastic { path }   does path/.fantastic/ exist as a directory
//!   LockHolder  { path }   the pid holding the folder's lock, if any
//!   RunFile     { path }   the folder's run file, parsed (CanvasRun::parse)
//!   CanvasUp    { canvas } does the page answer HTTP 2xx/3xx
//!   Guid        { port }   the kernel's guid via `reflect` on ws://127.0.0.1:<port>/user/ws
//! ```
//!
//! TWO MODES. `Lock` is the full app: it reads folders and locks, so a fantastic
//! is running when its folder's lock holder is the registered pid and its
//! canvas answers with its guid. `Canvas` is a sandboxed app: it may only ask
//! canvases, so it trusts the registry for the rest.
//!
//! An answer of the wrong kind for the pending question is a programming error
//! of the OS layer, and panics naming both.
//!
//! `Status::of` turns a probe, and whether the home is starting or stopping
//! that fantastic, into the one status its row shows.

use serde::{Deserialize, Serialize};

use crate::model::{Canvas, CanvasRun, Registered};

/// What is known of one fantastic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Its folder has a `.fantastic/` directory.
    pub is_fantastic: bool,
    /// The pid holding its folder's lock.
    pub holder: Option<u32>,
    /// The lock holder is the run the registry entry describes.
    pub owns_entry: bool,
    /// The canvas it serves.
    pub canvas: Option<Canvas>,
    /// That canvas answers.
    pub canvas_up: bool,
    /// The kernel behind that canvas has the registered guid.
    pub guid_matches: bool,
}

/// A question for the OS layer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ask {
    IsFantastic { path: String },
    LockHolder { path: String },
    RunFile { path: String },
    CanvasUp { canvas: Canvas },
    Guid { port: u16 },
}

/// The OS layer's answer to the matching `Ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    IsFantastic(bool),
    LockHolder(Option<u32>),
    RunFile(Option<CanvasRun>),
    CanvasUp(bool),
    Guid(Option<String>),
}

/// What the app may look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The full app: reads folders and locks.
    Lock,
    /// A sandboxed app: may only ask canvases.
    Canvas,
}

/// What is probed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A registry entry. `verified`: this run already answered with this guid,
    /// so the socket is not asked again.
    Registered { reg: Registered, verified: bool },
    /// A folder not in the registry yet.
    Adding { path: String },
}

/// The question a procedure is at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    IsFantastic,
    LockHolder,
    RunFile,
    CanvasUp,
    Guid,
    Done,
}

/// One probe procedure, driven by answers.
#[derive(Debug, Clone)]
pub struct Prober {
    target: Target,
    mode: Mode,
    probe: Probe,
    step: Step,
    run: Option<CanvasRun>,
}

impl Prober {
    /// The procedure for `target` in `mode`, before its first question.
    pub fn new(target: Target, mode: Mode) -> Prober {
        let (probe, step) = match (&target, mode) {
            (Target::Registered { .. }, Mode::Lock) => {
                (start(false, false, None, false), Step::IsFantastic)
            }
            (Target::Adding { .. }, Mode::Lock) => {
                (start(false, true, None, true), Step::IsFantastic)
            }
            (Target::Registered { reg, .. }, Mode::Canvas) => {
                let step = if reg.pid.is_some() && reg.canvas.is_some() {
                    Step::CanvasUp
                } else {
                    Step::Done
                };
                (start(true, true, reg.canvas.clone(), false), step)
            }
            (Target::Adding { .. }, Mode::Canvas) => (start(true, true, None, true), Step::RunFile),
        };
        Prober {
            target,
            mode,
            probe,
            step,
            run: None,
        }
    }

    /// The question to answer next; `None` once the procedure is done.
    pub fn next(&self) -> Option<Ask> {
        match self.step {
            Step::IsFantastic => Some(Ask::IsFantastic {
                path: self.path().to_string(),
            }),
            Step::LockHolder => Some(Ask::LockHolder {
                path: self.path().to_string(),
            }),
            Step::RunFile => Some(Ask::RunFile {
                path: self.path().to_string(),
            }),
            Step::CanvasUp => Some(Ask::CanvasUp {
                canvas: self.canvas().clone(),
            }),
            Step::Guid => Some(Ask::Guid {
                port: self.canvas().port,
            }),
            Step::Done => None,
        }
    }

    /// Feed the answer to the pending question.
    ///
    /// # Panics
    ///
    /// When no question is pending, or the answer is of another kind.
    pub fn answer(&mut self, answer: Answer) {
        match (self.step, answer) {
            (Step::IsFantastic, Answer::IsFantastic(is)) => {
                self.probe.is_fantastic = is;
                self.step = if is { Step::LockHolder } else { Step::Done };
            }
            (Step::LockHolder, Answer::LockHolder(holder)) => self.on_holder(holder),
            (Step::RunFile, Answer::RunFile(run)) => self.on_run(run),
            (Step::CanvasUp, Answer::CanvasUp(up)) => self.on_canvas_up(up),
            (Step::Guid, Answer::Guid(guid)) => {
                let matches = match &self.target {
                    Target::Registered { reg, .. } => guid.as_deref() == Some(reg.guid.as_str()),
                    Target::Adding { .. } => {
                        unreachable!("an added folder is never asked its guid")
                    }
                };
                self.probe.guid_matches = matches;
                if matches && self.mode == Mode::Canvas {
                    self.probe.holder = self.reg_pid();
                }
                self.step = Step::Done;
            }
            (Step::Done, answer) => panic!("probe answered {answer:?} with no question pending"),
            (step, answer) => panic!("probe answered {answer:?} to a {step:?} question"),
        }
    }

    /// What is known so far.
    pub fn probe(&self) -> &Probe {
        &self.probe
    }

    /// Run the procedure to its end, each question answered by `ask`.
    pub fn run(mut self, mut ask: impl FnMut(&Ask) -> Answer) -> Probe {
        while let Some(question) = self.next() {
            let answer = ask(&question);
            self.answer(answer);
        }
        self.probe
    }

    fn on_holder(&mut self, holder: Option<u32>) {
        let Some(h) = holder else {
            self.step = Step::Done;
            return;
        };
        self.probe.holder = Some(h);
        self.step = match &self.target {
            Target::Registered { reg, .. } => {
                self.probe.owns_entry = reg.pid == Some(h);
                match (&reg.canvas, self.probe.owns_entry) {
                    (Some(canvas), true) => {
                        self.probe.canvas = Some(canvas.clone());
                        Step::CanvasUp
                    }
                    _ => Step::Done,
                }
            }
            Target::Adding { .. } => Step::RunFile,
        };
    }

    fn on_run(&mut self, run: Option<CanvasRun>) {
        let Some(run) = run else {
            self.step = Step::Done;
            return;
        };
        if self.mode == Mode::Lock && self.probe.holder != Some(run.pid) {
            self.step = Step::Done;
            return;
        }
        self.probe.canvas = Some(run.canvas.clone());
        self.run = Some(run);
        self.step = Step::CanvasUp;
    }

    fn on_canvas_up(&mut self, up: bool) {
        self.probe.canvas_up = up;
        self.step = Step::Done;
        match &self.target {
            Target::Registered { verified, .. } => {
                if !up {
                    return;
                }
                if *verified {
                    self.probe.guid_matches = true;
                    if self.mode == Mode::Canvas {
                        self.probe.holder = self.reg_pid();
                    }
                } else {
                    self.step = Step::Guid;
                }
            }
            Target::Adding { .. } => {
                if up && self.mode == Mode::Canvas {
                    self.probe.holder = self.run.as_ref().map(|r| r.pid);
                }
            }
        }
    }

    fn path(&self) -> &str {
        match &self.target {
            Target::Registered { reg, .. } => &reg.path,
            Target::Adding { path } => path,
        }
    }

    fn reg_pid(&self) -> Option<u32> {
        match &self.target {
            Target::Registered { reg, .. } => reg.pid,
            Target::Adding { .. } => None,
        }
    }

    fn canvas(&self) -> &Canvas {
        self.probe
            .canvas
            .as_ref()
            .expect("a canvas question is asked only once the canvas is known")
    }
}

fn start(
    is_fantastic: bool,
    owns_entry: bool,
    canvas: Option<Canvas>,
    guid_matches: bool,
) -> Probe {
    Probe {
        is_fantastic,
        holder: None,
        owns_entry,
        canvas,
        canvas_up: false,
        guid_matches,
    }
}

/// The one status a fantastic's row shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Stopped,
    Starting,
    Stopping,
    Running(Canvas),
    /// It runs, but serves no canvas (a terminal, or headless).
    NoCanvas,
    /// Something holds its folder, but it is not shown to be this entry's run.
    Unverified,
    /// Its folder is not a fantastic (moved, or deleted).
    Missing,
}

impl Status {
    /// The status of `probe`, while the home is `launching` or `stopping` it.
    pub fn of(probe: &Probe, launching: bool, stopping: bool) -> Status {
        let waiting = |otherwise: Status| {
            if launching {
                Status::Starting
            } else {
                otherwise
            }
        };
        if !probe.is_fantastic {
            return waiting(Status::Missing);
        }
        if probe.holder.is_none() {
            return waiting(Status::Stopped);
        }
        if stopping {
            return Status::Stopping;
        }
        if !probe.owns_entry {
            return waiting(Status::Unverified);
        }
        let Some(canvas) = &probe.canvas else {
            return waiting(Status::NoCanvas);
        };
        if probe.canvas_up && probe.guid_matches {
            return Status::Running(canvas.clone());
        }
        waiting(Status::Unverified)
    }
}

/// The probe shows its fantastic running.
pub fn is_active(probe: &Probe) -> bool {
    matches!(Status::of(probe, false, false), Status::Running(_))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas(port: u16) -> Canvas {
        Canvas {
            url: format!("http://127.0.0.1:{port}/"),
            port,
        }
    }

    fn reg(pid: Option<u32>, canvas: Option<Canvas>) -> Registered {
        Registered {
            folder: "f".into(),
            guid: "g-1".into(),
            name: None,
            path: "/w/notes".into(),
            pid,
            canvas,
            stored_at: 0,
            last_seen: 0,
        }
    }

    fn run_file(pid: u32, port: u16) -> CanvasRun {
        CanvasRun {
            pid,
            port,
            canvas: canvas(port),
        }
    }

    /// Drive `target` in `mode` with `answers` in order; every answer must be
    /// used. Returns the questions asked and the final probe.
    fn drive(target: Target, mode: Mode, answers: Vec<Answer>) -> (Vec<Ask>, Probe) {
        let mut asked = Vec::new();
        let mut answers = answers.into_iter();
        let probe = Prober::new(target, mode).run(|ask| {
            asked.push(ask.clone());
            answers
                .next()
                .expect("a scripted answer for every question")
        });
        assert_eq!(answers.next(), None, "every scripted answer is used");
        (asked, probe)
    }

    fn p(
        is_fantastic: bool,
        holder: Option<u32>,
        owns_entry: bool,
        canvas: Option<Canvas>,
        canvas_up: bool,
        guid_matches: bool,
    ) -> Probe {
        Probe {
            is_fantastic,
            holder,
            owns_entry,
            canvas,
            canvas_up,
            guid_matches,
        }
    }

    fn path_asks(kinds: &[&str]) -> Vec<Ask> {
        kinds
            .iter()
            .map(|k| match *k {
                "is" => Ask::IsFantastic {
                    path: "/w/notes".into(),
                },
                "lock" => Ask::LockHolder {
                    path: "/w/notes".into(),
                },
                "run" => Ask::RunFile {
                    path: "/w/notes".into(),
                },
                other => panic!("no such ask {other}"),
            })
            .collect()
    }

    // ── Lock / Registered ───────────────────────────────────────────────────

    #[test]
    fn lock_registered_not_a_fantastic() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let (asked, probe) = drive(target, Mode::Lock, vec![Answer::IsFantastic(false)]);
        assert_eq!(asked, path_asks(&["is"]));
        assert_eq!(probe, p(false, None, false, None, false, false));
    }

    #[test]
    fn lock_registered_no_holder() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let (asked, probe) = drive(
            target,
            Mode::Lock,
            vec![Answer::IsFantastic(true), Answer::LockHolder(None)],
        );
        assert_eq!(asked, path_asks(&["is", "lock"]));
        assert_eq!(probe, p(true, None, false, None, false, false));
    }

    #[test]
    fn lock_registered_another_holder() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let (asked, probe) = drive(
            target,
            Mode::Lock,
            vec![Answer::IsFantastic(true), Answer::LockHolder(Some(6))],
        );
        assert_eq!(asked, path_asks(&["is", "lock"]));
        assert_eq!(probe, p(true, Some(6), false, None, false, false));
    }

    #[test]
    fn lock_registered_no_canvas() {
        let target = Target::Registered {
            reg: reg(Some(5), None),
            verified: false,
        };
        let (asked, probe) = drive(
            target,
            Mode::Lock,
            vec![Answer::IsFantastic(true), Answer::LockHolder(Some(5))],
        );
        assert_eq!(asked, path_asks(&["is", "lock"]));
        assert_eq!(probe, p(true, Some(5), true, None, false, false));
    }

    #[test]
    fn lock_registered_canvas_down() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let answers = vec![
            Answer::IsFantastic(true),
            Answer::LockHolder(Some(5)),
            Answer::CanvasUp(false),
        ];
        let (asked, probe) = drive(target, Mode::Lock, answers);
        let mut want = path_asks(&["is", "lock"]);
        want.push(Ask::CanvasUp {
            canvas: canvas(8000),
        });
        assert_eq!(asked, want);
        assert_eq!(
            probe,
            p(true, Some(5), true, Some(canvas(8000)), false, false)
        );
    }

    #[test]
    fn lock_registered_guid_asked_and_matched() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let answers = vec![
            Answer::IsFantastic(true),
            Answer::LockHolder(Some(5)),
            Answer::CanvasUp(true),
            Answer::Guid(Some("g-1".into())),
        ];
        let (asked, probe) = drive(target, Mode::Lock, answers);
        let mut want = path_asks(&["is", "lock"]);
        want.push(Ask::CanvasUp {
            canvas: canvas(8000),
        });
        want.push(Ask::Guid { port: 8000 });
        assert_eq!(asked, want);
        assert_eq!(
            probe,
            p(true, Some(5), true, Some(canvas(8000)), true, true)
        );
        assert!(is_active(&probe));
    }

    #[test]
    fn lock_registered_guid_mismatch_or_failure() {
        for guid in [Some("other".to_string()), None] {
            let target = Target::Registered {
                reg: reg(Some(5), Some(canvas(8000))),
                verified: false,
            };
            let answers = vec![
                Answer::IsFantastic(true),
                Answer::LockHolder(Some(5)),
                Answer::CanvasUp(true),
                Answer::Guid(guid),
            ];
            let (_, probe) = drive(target, Mode::Lock, answers);
            assert_eq!(
                probe,
                p(true, Some(5), true, Some(canvas(8000)), true, false)
            );
        }
    }

    #[test]
    fn lock_registered_verified_skips_the_guid() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: true,
        };
        let answers = vec![
            Answer::IsFantastic(true),
            Answer::LockHolder(Some(5)),
            Answer::CanvasUp(true),
        ];
        let (asked, probe) = drive(target, Mode::Lock, answers);
        assert!(!asked.iter().any(|a| matches!(a, Ask::Guid { .. })));
        assert_eq!(
            probe,
            p(true, Some(5), true, Some(canvas(8000)), true, true)
        );
    }

    // ── Lock / Adding ───────────────────────────────────────────────────────

    fn adding() -> Target {
        Target::Adding {
            path: "/w/notes".into(),
        }
    }

    #[test]
    fn lock_adding_not_a_fantastic() {
        let (asked, probe) = drive(adding(), Mode::Lock, vec![Answer::IsFantastic(false)]);
        assert_eq!(asked, path_asks(&["is"]));
        assert_eq!(probe, p(false, None, true, None, false, true));
    }

    #[test]
    fn lock_adding_no_holder() {
        let (asked, probe) = drive(
            adding(),
            Mode::Lock,
            vec![Answer::IsFantastic(true), Answer::LockHolder(None)],
        );
        assert_eq!(asked, path_asks(&["is", "lock"]));
        assert_eq!(probe, p(true, None, true, None, false, true));
    }

    #[test]
    fn lock_adding_no_run_file() {
        let answers = vec![
            Answer::IsFantastic(true),
            Answer::LockHolder(Some(9)),
            Answer::RunFile(None),
        ];
        let (asked, probe) = drive(adding(), Mode::Lock, answers);
        assert_eq!(asked, path_asks(&["is", "lock", "run"]));
        assert_eq!(probe, p(true, Some(9), true, None, false, true));
    }

    #[test]
    fn lock_adding_run_of_another_pid() {
        let answers = vec![
            Answer::IsFantastic(true),
            Answer::LockHolder(Some(9)),
            Answer::RunFile(Some(run_file(10, 8001))),
        ];
        let (asked, probe) = drive(adding(), Mode::Lock, answers);
        assert_eq!(asked, path_asks(&["is", "lock", "run"]));
        assert_eq!(probe, p(true, Some(9), true, None, false, true));
    }

    #[test]
    fn lock_adding_running_canvas() {
        for up in [true, false] {
            let answers = vec![
                Answer::IsFantastic(true),
                Answer::LockHolder(Some(9)),
                Answer::RunFile(Some(run_file(9, 8001))),
                Answer::CanvasUp(up),
            ];
            let (asked, probe) = drive(adding(), Mode::Lock, answers);
            let mut want = path_asks(&["is", "lock", "run"]);
            want.push(Ask::CanvasUp {
                canvas: canvas(8001),
            });
            assert_eq!(asked, want);
            assert_eq!(probe, p(true, Some(9), true, Some(canvas(8001)), up, true));
        }
    }

    // ── Canvas / Registered ─────────────────────────────────────────────────

    #[test]
    fn canvas_registered_without_pid_or_canvas_asks_nothing() {
        for (pid, c) in [(None, Some(canvas(8000))), (Some(5), None), (None, None)] {
            let target = Target::Registered {
                reg: reg(pid, c.clone()),
                verified: false,
            };
            let (asked, probe) = drive(target, Mode::Canvas, vec![]);
            assert!(asked.is_empty());
            assert_eq!(probe, p(true, None, true, c, false, false));
        }
    }

    #[test]
    fn canvas_registered_down() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let (asked, probe) = drive(target, Mode::Canvas, vec![Answer::CanvasUp(false)]);
        assert_eq!(
            asked,
            vec![Ask::CanvasUp {
                canvas: canvas(8000)
            }]
        );
        assert_eq!(probe, p(true, None, true, Some(canvas(8000)), false, false));
    }

    #[test]
    fn canvas_registered_guid_matched() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let answers = vec![Answer::CanvasUp(true), Answer::Guid(Some("g-1".into()))];
        let (asked, probe) = drive(target, Mode::Canvas, answers);
        assert_eq!(
            asked,
            vec![
                Ask::CanvasUp {
                    canvas: canvas(8000)
                },
                Ask::Guid { port: 8000 }
            ]
        );
        assert_eq!(
            probe,
            p(true, Some(5), true, Some(canvas(8000)), true, true)
        );
        assert!(is_active(&probe));
    }

    #[test]
    fn canvas_registered_guid_mismatch() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: false,
        };
        let answers = vec![Answer::CanvasUp(true), Answer::Guid(Some("g-2".into()))];
        let (_, probe) = drive(target, Mode::Canvas, answers);
        assert_eq!(probe, p(true, None, true, Some(canvas(8000)), true, false));
        assert_eq!(Status::of(&probe, false, false), Status::Stopped);
    }

    #[test]
    fn canvas_registered_verified_skips_the_guid() {
        let target = Target::Registered {
            reg: reg(Some(5), Some(canvas(8000))),
            verified: true,
        };
        let (asked, probe) = drive(target, Mode::Canvas, vec![Answer::CanvasUp(true)]);
        assert_eq!(
            asked,
            vec![Ask::CanvasUp {
                canvas: canvas(8000)
            }]
        );
        assert_eq!(
            probe,
            p(true, Some(5), true, Some(canvas(8000)), true, true)
        );
    }

    // ── Canvas / Adding ─────────────────────────────────────────────────────

    #[test]
    fn canvas_adding_no_run_file() {
        let (asked, probe) = drive(adding(), Mode::Canvas, vec![Answer::RunFile(None)]);
        assert_eq!(asked, path_asks(&["run"]));
        assert_eq!(probe, p(true, None, true, None, false, true));
    }

    #[test]
    fn canvas_adding_up_and_down() {
        let answers = vec![
            Answer::RunFile(Some(run_file(9, 8001))),
            Answer::CanvasUp(true),
        ];
        let (asked, probe) = drive(adding(), Mode::Canvas, answers);
        let mut want = path_asks(&["run"]);
        want.push(Ask::CanvasUp {
            canvas: canvas(8001),
        });
        assert_eq!(asked, want);
        assert_eq!(
            probe,
            p(true, Some(9), true, Some(canvas(8001)), true, true)
        );

        let answers = vec![
            Answer::RunFile(Some(run_file(9, 8001))),
            Answer::CanvasUp(false),
        ];
        let (_, probe) = drive(adding(), Mode::Canvas, answers);
        assert_eq!(probe, p(true, None, true, Some(canvas(8001)), false, true));
    }

    // ── answers of the wrong kind ───────────────────────────────────────────

    #[test]
    #[should_panic(expected = "to a IsFantastic question")]
    fn a_wrong_answer_panics() {
        let mut prober = Prober::new(adding(), Mode::Lock);
        prober.answer(Answer::CanvasUp(true));
    }

    #[test]
    #[should_panic(expected = "no question pending")]
    fn an_answer_after_done_panics() {
        let target = Target::Registered {
            reg: reg(None, None),
            verified: false,
        };
        let mut prober = Prober::new(target, Mode::Canvas);
        assert_eq!(prober.next(), None);
        prober.answer(Answer::CanvasUp(true));
    }

    // ── Status::of ──────────────────────────────────────────────────────────

    #[test]
    fn status_of_every_branch() {
        let c = canvas(8000);
        let running = p(true, Some(5), true, Some(c.clone()), true, true);
        let table: Vec<(Probe, bool, bool, Status)> = vec![
            // not a fantastic
            (
                p(false, Some(5), true, Some(c.clone()), true, true),
                false,
                false,
                Status::Missing,
            ),
            (
                p(false, None, true, None, false, false),
                true,
                false,
                Status::Starting,
            ),
            (
                p(false, None, true, None, false, false),
                false,
                true,
                Status::Missing,
            ),
            // no holder
            (
                p(true, None, true, Some(c.clone()), true, true),
                false,
                false,
                Status::Stopped,
            ),
            (
                p(true, None, true, None, false, false),
                true,
                false,
                Status::Starting,
            ),
            (
                p(true, None, true, None, false, false),
                false,
                true,
                Status::Stopped,
            ),
            // stopping wins over everything below, launching included
            (running.clone(), false, true, Status::Stopping),
            (running.clone(), true, true, Status::Stopping),
            // not this entry's run
            (
                p(true, Some(6), false, None, false, false),
                false,
                false,
                Status::Unverified,
            ),
            (
                p(true, Some(6), false, None, false, false),
                true,
                false,
                Status::Starting,
            ),
            // no canvas
            (
                p(true, Some(5), true, None, false, false),
                false,
                false,
                Status::NoCanvas,
            ),
            (
                p(true, Some(5), true, None, false, false),
                true,
                false,
                Status::Starting,
            ),
            // running, launching or not
            (running.clone(), false, false, Status::Running(c.clone())),
            (running.clone(), true, false, Status::Running(c.clone())),
            // canvas down, or another guid
            (
                p(true, Some(5), true, Some(c.clone()), false, true),
                false,
                false,
                Status::Unverified,
            ),
            (
                p(true, Some(5), true, Some(c.clone()), true, false),
                false,
                false,
                Status::Unverified,
            ),
            (
                p(true, Some(5), true, Some(c.clone()), false, true),
                true,
                false,
                Status::Starting,
            ),
        ];
        for (probe, launching, stopping, want) in table {
            assert_eq!(
                Status::of(&probe, launching, stopping),
                want,
                "{probe:?} launching={launching} stopping={stopping}"
            );
        }
        assert!(is_active(&running));
        assert!(!is_active(&p(true, Some(5), true, Some(c), false, true)));
    }

    #[test]
    fn status_serializes_snake_case() {
        let running = serde_yaml::to_string(&Status::Running(canvas(8000))).expect("serializes");
        assert!(running.contains("running"), "{running}");
        assert!(running.contains("port: 8000"), "{running}");
        assert_eq!(
            serde_yaml::to_string(&Status::NoCanvas)
                .expect("serializes")
                .trim(),
            "no_canvas"
        );
    }
}
