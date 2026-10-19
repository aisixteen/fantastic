//! TEST-ONLY. A process that hosts the home kernel in-process, for the e2e
//! (`e2e/tests/scenario/E24_home`) and the CDP probes. No release ships it and
//! the app never builds it (`required-features = ["host"]`): the desktop app is
//! the home's only host.
//!
//! `fantastic-home-host --fantastic <path> [--port N]`: boots the home of
//! `home_dir_from_env()`, prints `fantastic-home-host: the home: <url>`, and
//! serves until SIGTERM or SIGINT (Ctrl-Break or Ctrl-C on Windows), then shuts
//! down in order.

use std::io::Write;
use std::path::PathBuf;

use fantastic_home_kernel::{home::sys, home_dir_from_env, running, Config, HomeKernel};

const USAGE: &str = "fantastic-home-host: usage: fantastic-home-host --fantastic <path> [--port N]";

fn usage() -> ! {
    eprintln!("{USAGE}");
    std::process::exit(2)
}

fn parse(args: &[String]) -> (PathBuf, Option<u16>) {
    let mut bin = None;
    let mut port = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--fantastic" if bin.is_none() => {
                bin = Some(PathBuf::from(it.next().unwrap_or_else(|| usage())))
            }
            "--port" if port.is_none() => match it.next().map(|p| p.parse::<u16>()) {
                Some(Ok(p)) if p != 0 => port = Some(p),
                _ => usage(),
            },
            _ => usage(),
        }
    }
    (bin.unwrap_or_else(|| usage()), port)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (fantastic_bin, port) = parse(&args);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("fantastic-home-host: {e}");
            std::process::exit(1)
        }
    };
    let code = runtime.block_on(async move {
        tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .init();
        let home = match home_dir_from_env() {
            Ok(h) => h,
            Err(e) => {
                eprintln!("fantastic-home-host: {e}");
                return 2;
            }
        };
        match running(&home).await {
            Ok(Some(r)) => {
                eprintln!(
                    "fantastic-home-host: already running (pid {}) — {}",
                    r.pid, r.url
                );
                return 2;
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("fantastic-home-host: {e}");
                return 1;
            }
        }
        let host = match HomeKernel::boot(Config {
            home,
            port,
            fantastic_bin,
        })
        .await
        {
            Ok(h) => h,
            Err(e) => {
                eprintln!("fantastic-home-host: {e}");
                return 1;
            }
        };
        println!("fantastic-home-host: the home: {}", host.url());
        let _ = std::io::stdout().flush();
        if let Err(e) = sys::stopped().await {
            eprintln!("fantastic-home-host: {e}");
        }
        match host.shutdown().await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("fantastic-home-host: {e}");
                1
            }
        }
    });
    std::process::exit(code)
}
