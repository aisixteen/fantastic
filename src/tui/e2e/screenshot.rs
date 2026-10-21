//! Headful PTY screenshot harness for TUI e2e. Spawns a TUI binary in a
//! pseudo-terminal, runs a **scenario script**, and writes the rendered vt100
//! cell grid to text "screenshots" — single frames (`shot`) or a stream of
//! frames every N ms (`stream`, ideal for animations). Colors don't survive the
//! text dump, but the LAYOUT + block glyphs (the `█` FANTASTIC title) do.
//!
//!   cargo run -q --example screenshot -p fantastic-tui -- <binary> <script> <out_dir>
//!
//! Script lines (`#` = comment):
//!   wait <ms>              sleep
//!   type <text…>           send literal text (rest of line)
//!   key  <name>            send a key: space|enter|esc|tab|backspace|
//!                          ctrl-c|ctrl-f|ctrl-q|up|down|left|right|<single char>
//!   shot <label>           capture one frame → <out>/NN_<label>.txt
//!   stream <ms> <count>    capture <count> frames every <ms> → NN_streamMM.txt
//!   exit <ms>              wait up to <ms> for the binary to exit ON ITS OWN
//!                          (after a quit gesture) and print `exit: <code>`;
//!                          `exit: timeout` if it did not
//!
//! The binary runs in a FRESH temp cwd, so `@sh`/`@ws`/brain history never touch
//! the repo; `FT_CWD=<dir>` reuses one instead (several runs over one store).
//! Screen size via FT_ROWS/FT_COLS (default 30×110).

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

fn key_bytes(name: &str) -> Vec<u8> {
    match name {
        "space" => vec![b' '],
        "enter" => vec![b'\r'],
        "esc" => vec![0x1b],
        "tab" => vec![b'\t'],
        "backtab" | "shift-tab" => vec![0x1b, b'[', b'Z'], // CSI Z → crossterm BackTab
        "backspace" => vec![0x7f],
        "ctrl-c" => vec![0x03],
        "ctrl-f" => vec![0x06],
        "ctrl-q" => vec![0x11],
        "ctrl-s" => vec![0x13], // the in-frame editor's save (28)
        "up" => vec![0x1b, b'[', b'A'],
        "down" => vec![0x1b, b'[', b'B'],
        "right" => vec![0x1b, b'[', b'C'],
        "left" => vec![0x1b, b'[', b'D'],
        s => s.bytes().collect(), // a single char / literal
    }
}

/// A shot waits for the screen to hold still this long...
const SETTLE_MS: u64 = 300;
/// ...but no longer than this (a clock, a spinner never holds still).
const SETTLE_MAX_MS: u64 = 3000;

fn settle(parser: &Mutex<vt100::Parser>) {
    let deadline = std::time::Instant::now() + Duration::from_millis(SETTLE_MAX_MS);
    let mut last = parser.lock().unwrap().screen().contents();
    let mut still = std::time::Instant::now();
    while std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
        let now = parser.lock().unwrap().screen().contents();
        if now != last {
            last = now;
            still = std::time::Instant::now();
        } else if still.elapsed() >= Duration::from_millis(SETTLE_MS) {
            return;
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let bin = args
        .next()
        .expect("usage: screenshot <binary> <script> <out_dir>");
    let script_path = args.next().expect("missing <script>");
    let out_dir = PathBuf::from(args.next().expect("missing <out_dir>"));
    fs::create_dir_all(&out_dir).expect("mkdir out_dir");

    let rows: u16 = std::env::var("FT_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let cols: u16 = std::env::var("FT_COLS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(110);

    // Fresh temp cwd so @sh/@ws/brain-history don't pollute the repo — unless
    // `FT_CWD` names one, which is how a test runs the shell several times over
    // ONE store (init, then rehydrate).
    let cwd = match std::env::var_os("FT_CWD") {
        Some(d) => PathBuf::from(d),
        None => std::env::temp_dir().join(format!("ft-e2e-run-{}", std::process::id())),
    };
    fs::create_dir_all(&cwd).ok();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");
    let mut cmd = CommandBuilder::new(&bin);
    // `FT_ARGS`: the shell's own arguments, space-separated (e.g. `tui web`).
    if let Ok(a) = std::env::var("FT_ARGS") {
        for w in a.split_whitespace() {
            cmd.arg(w);
        }
    }
    cmd.env("TERM", "xterm-256color");
    cmd.env("FANTASTIC_HOME", cwd.join("home")); // app state → temp, not the real home
    cmd.cwd(&cwd);
    let mut child = pair.slave.spawn_command(cmd).expect("spawn");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let mut writer = pair.master.take_writer().expect("writer");
    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
    let p2 = Arc::clone(&parser);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => p2.lock().unwrap().process(&buf[..n]),
            }
        }
    });

    let mut idx = 0usize;
    let mut shot = |label: &str| {
        let scr = parser.lock().unwrap();
        let body: String = scr
            .screen()
            .contents()
            .lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        let file = out_dir.join(format!("{idx:02}_{label}.txt"));
        fs::write(&file, format!("# {label}\n{body}\n")).ok();
        println!("  shot {idx:02}_{label}");
        idx += 1;
    };

    let script = fs::read_to_string(&script_path).expect("read script");
    for raw in script.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        match cmd {
            "wait" => std::thread::sleep(Duration::from_millis(rest.trim().parse().unwrap_or(0))),
            "type" => {
                writer.write_all(rest.as_bytes()).ok();
                writer.flush().ok();
            }
            "key" => {
                writer.write_all(&key_bytes(rest.trim())).ok();
                writer.flush().ok();
            }
            // `click X Y` (0-based cells): a left press + release as the SGR
            // mouse sequence a terminal sends once the app captures the mouse.
            "click" => {
                let mut it = rest
                    .split_whitespace()
                    .filter_map(|v| v.parse::<u16>().ok());
                if let (Some(x), Some(y)) = (it.next(), it.next()) {
                    let (cx, cy) = (x + 1, y + 1);
                    writer
                        .write_all(format!("\x1b[<0;{cx};{cy}M\x1b[<0;{cx};{cy}m").as_bytes())
                        .ok();
                    writer.flush().ok();
                }
            }
            // `until MS TEXT`: wait until the screen shows TEXT (at most MS) — a
            // step that waits on the app, not on a guessed time
            "until" => {
                let (ms, text) = rest
                    .trim()
                    .split_once(char::is_whitespace)
                    .unwrap_or((rest.trim(), ""));
                let ms: u64 = ms.parse().unwrap_or(10000);
                let deadline = std::time::Instant::now() + Duration::from_millis(ms);
                loop {
                    if parser.lock().unwrap().screen().contents().contains(text) {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        println!("  until: timeout {text:?}");
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            // a SETTLED screen: unchanged for SETTLE_MS (at most SETTLE_MAX_MS) — a
            // redraw arrives in pieces (ConPTY), and a shot mid-way caught half of it
            "shot" => {
                settle(&parser);
                shot(rest.trim());
            }
            "stream" => {
                let mut it = rest.split_whitespace();
                let ms: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(100);
                let count: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(10);
                for _ in 0..count {
                    std::thread::sleep(Duration::from_millis(ms));
                    shot("stream");
                }
            }
            "exit" => {
                let ms: u64 = rest.trim().parse().unwrap_or(5000);
                let until = std::time::Instant::now() + Duration::from_millis(ms);
                let mut said = false;
                while std::time::Instant::now() < until {
                    if let Ok(Some(status)) = child.try_wait() {
                        println!("exit: {}", status.exit_code());
                        said = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if !said {
                    println!("exit: timeout");
                }
            }
            other => eprintln!("  ? unknown script verb: {other}"),
        }
    }

    // Kill only what is still running: a binary that left on its own must have
    // done its own shutdown, and SIGKILL would hide whether it did.
    if matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    // A named cwd is the caller's store — never delete it.
    if std::env::var_os("FT_CWD").is_none() {
        let _ = fs::remove_dir_all(&cwd);
    }
    println!("frames → {}", out_dir.display());
}
