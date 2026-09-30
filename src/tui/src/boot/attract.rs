//! ATTRACT — the one screen that predates the kernel.
//!
//! The rule (user, non-negotiable): it is shown ONCE PER DIRECTORY, when no
//! `.fantastic` exists there, BEFORE anything is written. The key press is what
//! authorises the first write; a directory that already has a `.fantastic` goes
//! straight in and never sees it again. A new directory sees it again, because
//! it is a new kernel.
//!
//! Nothing here talks to a kernel: the screen exists so that the first thing
//! the shell does to your disk is something you asked for.

use std::time::{Duration, Instant};

use ratatui::layout::Rect;
use ratatui::Frame;

use crate::theme;
use crate::ui::chrome;

use super::bg;

/// Seconds the title takes to tween from big-centred to small-docked.
pub const MINIMIZE_SECS: f32 = 1.0;
/// Seconds the block title takes to wipe in, top to bottom.
pub const REVEAL_SECS: f32 = 1.5;
/// Two Ctrl-C inside this window exit. One is a cancel, two is a quit.
pub const DOUBLE_PRESS_MS: u64 = 400;
/// Key auto-repeat arrives faster than this; a slower `q` restarts the streak.
pub const Q_REPEAT_MS: u64 = 220;
/// Hold `q` this many repeats to quit.
pub const Q_HOLD_STREAK: u8 = 12;

/// Idle this long on the attract screen and the INTRO MOVIE plays itself. The
/// attract screen is not a splash to click past — left alone it explains the
/// system, which is the whole point of an attract mode.
pub const ATTRACT_IDLE_SECS: f32 = 10.0;

/// Where the shell is in its bring-up. `Live` is everything after the title has
/// docked; the panes hang off it in later steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No `.fantastic` here yet: the title, the sky, "press any key".
    Attract,
    /// A key was pressed: the title tweens to its docked slot over ~1s.
    Minimizing,
    /// Docked. From here the kernel is attached or spawned (step 5).
    Live,
}

/// True when this Ctrl-C should exit: the previous one landed inside the
/// double-press window. Pure — the caller owns the clock and the last stamp.
pub fn ctrl_c_exits(last: Option<Instant>, now: Instant) -> bool {
    last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(DOUBLE_PRESS_MS))
}

/// The next `q`-hold streak, given the previous streak and the last `q`. A press
/// within `Q_REPEAT_MS` extends the run (auto-repeat means the key is held); a
/// slower press restarts at 1. The caller exits once this reaches
/// `Q_HOLD_STREAK`. Pure.
pub fn q_hold_streak(prev: u8, last: Option<Instant>, now: Instant) -> u8 {
    let fast = last.is_some_and(|t| now.duration_since(t) < Duration::from_millis(Q_REPEAT_MS));
    if fast {
        prev.saturating_add(1)
    } else {
        1
    }
}

/// Fraction of the reveal wipe completed, clamped.
pub fn reveal(since: Instant, now: Instant) -> f32 {
    (now.duration_since(since).as_secs_f32() / REVEAL_SECS).clamp(0.0, 1.0)
}

/// Eased tween position for the minimize, clamped. Ease-out: it leaves fast and
/// settles into the dock.
pub fn minimize_t(since: Instant, now: Instant) -> f32 {
    let raw = (now.duration_since(since).as_secs_f32() / MINIMIZE_SECS).clamp(0.0, 1.0);
    1.0 - (1.0 - raw) * (1.0 - raw)
}

/// ATTRACT: the block title wiping in over the drifting sky, and a blinking
/// prompt two rows under it.
pub fn render_attract(f: &mut Frame, area: Rect, clock: f32, reveal: f32) {
    let buf = f.buffer_mut();
    let title_bottom = bg::render_title(buf, area, reveal);
    if chrome::blink(clock, 1.2) {
        chrome::text_center(
            buf,
            area,
            title_bottom + 2,
            "PRESS ANY KEY TO CONTINUE",
            theme::prompt(),
        );
    }
}

/// MINIMIZING: the title lets go WHERE IT STANDS. It does not travel or dock —
/// a permanent banner costs rows the work needs (design decision: the docked
/// title is gone). It thins out on a fixed cell pattern as `t` goes 0 → 1, so
/// every frame is a subset of the one before.
pub fn render_minimizing(f: &mut Frame, area: Rect, t: f32) {
    let mut ghost = ratatui::buffer::Buffer::empty(area);
    bg::draw_title(&mut ghost, area, bg::title_geometry(area), 1.0);
    let t = t.clamp(0.0, 1.0);
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &ghost[(x, y)];
            if cell.symbol() == " " {
                continue;
            }
            if dissolve_keeps(x, y, t) {
                buf[(x, y)] = cell.clone();
            }
        }
    }
}

/// The dissolve mask: a cell survives while its fixed rank is above `t`.
fn dissolve_keeps(x: u16, y: u16, t: f32) -> bool {
    ((u32::from(x) * 7 + u32::from(y) * 13) % 16) as f32 / 16.0 >= t
}

#[cfg(test)]
mod dissolve_tests {
    use super::dissolve_keeps;

    #[test]
    fn the_title_is_whole_at_zero_and_gone_at_one_and_only_ever_shrinks() {
        let cells: Vec<(u16, u16)> = (0..40).flat_map(|x| (0..10).map(move |y| (x, y))).collect();
        assert!(cells.iter().all(|&(x, y)| dissolve_keeps(x, y, 0.0)), "whole at t=0");
        assert!(cells.iter().all(|&(x, y)| !dissolve_keeps(x, y, 1.0)), "gone at t=1");
        for w in [0.0f32, 0.25, 0.5, 0.75].windows(2) {
            for &(x, y) in &cells {
                assert!(!dissolve_keeps(x, y, w[1]) || dissolve_keeps(x, y, w[0]), "a later frame kept a cell an earlier one dropped");
            }
        }
    }
}

/// The attract state-machine decision for one tick. Pure + unit-tested.
#[derive(Debug, PartialEq, Eq)]
pub enum AttractTick {
    /// On the attract screen and idle >= 10s -> kick off the auto-demo.
    StartIntro,
    /// The auto-demo finished one full pass -> drop back to attract (it loops).
    EndIntroToAttract,
    /// Nothing to do this tick.
    Nothing,
}

/// Decide what the attract loop should do this tick. Only acts while the shell
/// has not `started` (the human is on the attract screen, not in the shell):
/// - not started, not playing, idle >= 10s -> `StartIntro`.
/// - not started, playing, the demo ran its full length -> `EndIntroToAttract`.
/// - otherwise `Nothing`.
///
/// Once `started`, this is always `Nothing`.
pub fn attract_tick(
    started: bool,
    intro_playing: bool,
    idle_secs: f32,
    intro_elapsed: f32,
    movie_total: f32,
) -> AttractTick {
    if started {
        return AttractTick::Nothing;
    }
    if intro_playing {
        if intro_elapsed >= movie_total {
            AttractTick::EndIntroToAttract
        } else {
            AttractTick::Nothing
        }
    } else if idle_secs >= ATTRACT_IDLE_SECS {
        AttractTick::StartIntro
    } else {
        AttractTick::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_exits_only_inside_the_window() {
        let now = Instant::now();
        assert!(!ctrl_c_exits(None, now));
        let just_now = now - Duration::from_millis(DOUBLE_PRESS_MS / 2);
        assert!(ctrl_c_exits(Some(just_now), now));
        let long_ago = now - Duration::from_millis(DOUBLE_PRESS_MS + 50);
        assert!(!ctrl_c_exits(Some(long_ago), now));
    }

    #[test]
    fn q_hold_streak_counts_fast_repeats() {
        let now = Instant::now();
        assert_eq!(q_hold_streak(0, None, now), 1);
        let fast = now - Duration::from_millis(Q_REPEAT_MS / 2);
        assert_eq!(q_hold_streak(3, Some(fast), now), 4);
        let slow = now - Duration::from_millis(Q_REPEAT_MS * 3);
        assert_eq!(q_hold_streak(9, Some(slow), now), 1);
    }

    #[test]
    fn minimize_is_eased_and_clamped() {
        let now = Instant::now();
        let start = now - Duration::from_secs_f32(MINIMIZE_SECS * 2.0);
        assert_eq!(minimize_t(start, now), 1.0);
        let half = now - Duration::from_secs_f32(MINIMIZE_SECS / 2.0);
        let t = minimize_t(half, now);
        assert!(t > 0.5 && t < 1.0, "ease-out is ahead of linear: {t}");
    }

    #[test]
    fn reveal_is_clamped() {
        let now = Instant::now();
        assert_eq!(reveal(now, now), 0.0);
        assert_eq!(reveal(now - Duration::from_secs(9), now), 1.0);
    }

    #[test]
    fn attract_idle_10s_starts_intro() {
        // Not started, not playing: 9.9s idle waits; 10.0s kicks the demo.
        assert_eq!(attract_tick(false, false, 9.9, 0.0, 27.4), AttractTick::Nothing);
        assert_eq!(attract_tick(false, false, 10.0, 0.0, 27.4), AttractTick::StartIntro);
    }

    #[test]
    fn attract_demo_end_returns_to_attract() {
        // Not started, playing: under the movie total keeps playing; at/over it
        // loops back to attract.
        assert_eq!(attract_tick(false, true, 0.0, 27.3, 27.4), AttractTick::Nothing);
        assert_eq!(
            attract_tick(false, true, 0.0, 27.4, 27.4),
            AttractTick::EndIntroToAttract
        );
        assert_eq!(
            attract_tick(false, true, 0.0, 30.0, 27.4),
            AttractTick::EndIntroToAttract
        );
    }

    #[test]
    fn attract_does_nothing_once_started() {
        // In the shell (`started`): the attract machine never fires, regardless of
        // idle time.
        assert_eq!(attract_tick(true, false, 999.0, 0.0, 27.4), AttractTick::Nothing);
        assert_eq!(attract_tick(true, true, 0.0, 999.0, 27.4), AttractTick::Nothing);
    }
}
