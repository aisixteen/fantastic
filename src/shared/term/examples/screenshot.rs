//! Headful PTY screenshot harness for TUI e2e. Spawns a TUI binary in a
//! pseudo-terminal, runs a **scenario script**, and writes the rendered vt100
//! cell grid to text "screenshots" — single frames (`shot`) or a stream of
//! frames every N ms (`stream`, ideal for animations). Colors don't survive the
//! text dump, but the LAYOUT + block glyphs (the `█` FANTASTIC title) do.
//!
//!   cargo run -q --example screenshot -p fantastic-term -- <binary> <script> <out_dir>
//!
//! Script lines (`#` = comment):
//!   wait <ms>              sleep
//!   type <text…>           send literal text (rest of line)
//!   key  <name>            send a key: space|enter|esc|tab|backspace|
//!                          ctrl-c|ctrl-f|ctrl-q|up|down|left|right|<single char>
//!   shot <label>           capture one frame → <out>/NN_<label>.txt
//!   stream <ms> <count>    capture <count> frames every <ms> → NN_streamMM.txt
//!
//! The binary runs in a FRESH temp cwd, so `@sh`/`@ws`/brain history never touch
//! the repo. Screen size via FT_ROWS/FT_COLS (default 30×110).

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
        "up" => vec![0x1b, b'[', b'A'],
        "down" => vec![0x1b, b'[', b'B'],
        "right" => vec![0x1b, b'[', b'C'],
        "left" => vec![0x1b, b'[', b'D'],
        s => s.bytes().collect(), // a single char / literal
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

    // Fresh temp cwd so @sh/@ws/brain-history don't pollute the repo.
    let cwd = std::env::temp_dir().join(format!("ft-e2e-run-{}", std::process::id()));
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
            "shot" => shot(rest.trim()),
            "stream" => {
                let mut it = rest.split_whitespace();
                let ms: u64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(100);
                let count: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(10);
                for _ in 0..count {
                    std::thread::sleep(Duration::from_millis(ms));
                    shot("stream");
                }
            }
            other => eprintln!("  ? unknown script verb: {other}"),
        }
    }

    let _ = child.kill();
    let _ = fs::remove_dir_all(&cwd);
    println!("frames → {}", out_dir.display());
}
