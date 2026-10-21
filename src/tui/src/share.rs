//! THE SHARED VIEW — one ViewModel, any number of terminals (owner,
//! 2026-10-02: "like tmux"; "two TUIs update like geminis").
//!
//! The process that owns the kernel also owns the ONE ViewModel — selection,
//! focus, scroll, panels, the composer, the seen set — and every frame it
//! renders goes to every attached View. A second `fantastic` in the directory
//! is a THIN CLIENT: it keeps no state, sends its keys and its size up
//! `.fantastic/cli/tui.sock`, and paints the frames that come down. Connecting
//! restores the exact picture; all attached terminals move in lockstep.
//!
//! VIEWS: every View gets the frame laid out for IT (`vm::Frames`, keyed by
//! View) — the same state, reflowed to its size, with its own scroll windows. Only a terminal panel's PTY has one size; it follows
//! the latest View to resize or type (tmux's `latest`, `vm::effective`).
//!
//! LEAVING: a thin client's quit gestures (Ctrl-C twice, Ctrl-A d, holding q)
//! DETACH only that client — the ViewModel reads them as `App::leave` from a
//! remote View. Recipes stay with the owner: a thin client takes no arguments.
//!
//! The wire: length-prefixed JSON messages ([`Up`], [`Down`]); a frame after
//! the first is the cells that changed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use futures_util::StreamExt;
use ratatui::buffer::{Buffer, Cell};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot, watch};

use crate::env::Paths;
use crate::vm::{Frame, Frames, ViewId, VmIn};

/// The socket, RELATIVE to the directory (every process runs in the directory
/// it serves; a relative path never meets the unix address-length limit).
pub const SOCKET: &str = ".fantastic/cli/tui.sock";

/// #210: said to a thin client when the kernel it attached to has no terminal
/// view at all — there is nothing to paint, so it is told why and exits.
pub const NO_VIEW: &str = "this kernel runs without a terminal view (canvas or headless) — run `fantastic` in a terminal to own the view, or open the canvas";

/// A thin client → the owner.
#[derive(Debug, Serialize, Deserialize)]
pub enum Up {
    Size { w: u16, h: u16 },
    Event(crossterm::event::Event),
}

/// The owner → a thin client.
#[derive(Debug, Serialize, Deserialize)]
pub enum Down {
    Hello {
        pid: u32,
    },
    Full {
        buf: Buffer,
        cursor: Option<(u16, u16)>,
        mouse: bool,
    },
    Diff {
        cells: Vec<(u16, u16, Cell)>,
        cursor: Option<(u16, u16)>,
        mouse: bool,
    },
    Bye(String),
}

async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, m: &T) -> std::io::Result<()> {
    let body = serde_json::to_vec(m).map_err(std::io::Error::other)?;
    w.write_all(&(body.len() as u32).to_be_bytes()).await?;
    w.write_all(&body).await?;
    w.flush().await
}

async fn read_msg<R: AsyncRead + Unpin, T: for<'de> Deserialize<'de>>(
    r: &mut R,
) -> std::io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut body).await?;
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(std::io::Error::other)
}

/// What one frame costs on the wire, given what this client already has.
pub fn delta(prev: Option<&Frame>, next: &Frame) -> Option<Down> {
    match prev {
        Some(p) if p.buf.area == next.buf.area => {
            let cells: Vec<(u16, u16, Cell)> = p
                .buf
                .diff(&next.buf)
                .into_iter()
                .map(|(x, y, c)| (x, y, c.clone()))
                .collect();
            if cells.is_empty() && p.cursor == next.cursor && p.mouse == next.mouse {
                return None;
            }
            Some(Down::Diff {
                cells,
                cursor: next.cursor,
                mouse: next.mouse,
            })
        }
        _ => Some(Down::Full {
            buf: next.buf.clone(),
            cursor: next.cursor,
            mouse: next.mouse,
        }),
    }
}

// ── the owner's side ────────────────────────────────────────────────────────

/// The attached thin clients, so the ViewModel can detach one.
#[derive(Default)]
pub struct Share {
    bye: Mutex<HashMap<ViewId, oneshot::Sender<String>>>,
}

static NEXT: AtomicU64 = AtomicU64::new(1);

impl Share {
    /// Detach one thin client: it is told why, and closes.
    pub fn detach(&self, id: ViewId, why: &str) {
        if let Some(tx) = self.bye.lock().expect("share poisoned").remove(&id) {
            let _ = tx.send(why.to_string());
        }
    }
}

/// #210: what an attaching client is told. With a terminal view, just the Hello;
/// without one, the Hello is followed by a Bye — a kernel with no view has no
/// frames to send, so the thin client is told so and ends instead of sitting on
/// a blank screen.
fn greeting(has_view: bool, pid: u32) -> Vec<Down> {
    if has_view {
        vec![Down::Hello { pid }]
    } else {
        vec![Down::Hello { pid }, Down::Bye(NO_VIEW.into())]
    }
}

/// Serve the socket. The caller holds the directory's lock, so a socket file
/// already there is stale. `has_view` (#210) is whether the owner has a terminal
/// View; without one every client is greeted with a Bye and served no frames.
pub fn listen(
    input: mpsc::Sender<VmIn>,
    frames: watch::Receiver<Option<Arc<Frames>>>,
    has_view: bool,
) -> Result<Arc<Share>, String> {
    let _ = std::fs::remove_file(SOCKET);
    let l = crate::sys::bind_local(SOCKET)
        .map_err(|e| format!("could not open the TUI socket `{SOCKET}`: {e}"))?;
    crate::sys::make_private(SOCKET)
        .map_err(|e| format!("could not make `{SOCKET}` private: {e}"))?;
    let share = Arc::new(Share::default());
    let s = Arc::clone(&share);
    tokio::spawn(async move {
        while let Ok((stream, _)) = l.accept().await {
            let id = NEXT.fetch_add(1, Ordering::SeqCst);
            let (bye_tx, bye_rx) = oneshot::channel();
            s.bye.lock().expect("share poisoned").insert(id, bye_tx);
            tokio::spawn(serve(
                id,
                stream,
                input.clone(),
                frames.clone(),
                bye_rx,
                Arc::clone(&s),
                has_view,
            ));
        }
    });
    Ok(share)
}

/// Remove the socket file, after the ViewModel has stopped.
pub fn close() {
    let _ = std::fs::remove_file(SOCKET);
}

async fn serve(
    id: ViewId,
    stream: crate::sys::LocalStream,
    input: mpsc::Sender<VmIn>,
    mut frames: watch::Receiver<Option<Arc<Frames>>>,
    mut bye: oneshot::Receiver<String>,
    share: Arc<Share>,
    has_view: bool,
) {
    let (mut r, mut w) = stream.into_split();
    // up: size and events, until the client goes
    let up_input = input.clone();
    let mut up = tokio::spawn(async move {
        loop {
            match read_msg::<_, Up>(&mut r).await {
                Ok(Some(Up::Size { w, h })) => {
                    let _ = up_input.send(VmIn::Resize(id, w, h)).await;
                }
                Ok(Some(Up::Event(e))) => {
                    let _ = up_input.send(VmIn::Event(id, e)).await;
                }
                Ok(None) | Err(_) => break,
            }
        }
    });
    // down: the greeting, then every frame as a delta, until the ViewModel ends,
    // the client goes, or the client is detached. #210: with no terminal view the
    // greeting ends with a Bye, so there is no frame loop at all.
    let greet = greeting(has_view, std::process::id());
    let no_view = matches!(greet.last(), Some(Down::Bye(_)));
    let mut greeted = true;
    for m in &greet {
        greeted = write_msg(&mut w, m).await.is_ok();
    }
    if greeted && !no_view {
        let mut sent: Option<Arc<Frame>> = None;
        loop {
            tokio::select! {
                changed = frames.changed() => {
                    if changed.is_err() {
                        let _ = write_msg(&mut w, &Down::Bye("the kernel's process ended".into())).await;
                        break;
                    }
                }
                why = &mut bye => {
                    let _ = write_msg(&mut w, &Down::Bye(why.unwrap_or_else(|_| "detached".into()))).await;
                    break;
                }
                _ = &mut up => break,
            }
            // this client's own frame, as a delta of what it already has
            let Some(f) = frames.borrow().as_ref().and_then(|fs| fs.for_view(id)) else {
                continue;
            };
            if let Some(m) = delta(sent.as_deref(), &f) {
                if write_msg(&mut w, &m).await.is_err() {
                    break;
                }
            }
            sent = Some(f);
        }
    }
    up.abort();
    share.bye.lock().expect("share poisoned").remove(&id);
    let _ = input.send(VmIn::Gone(id)).await;
}

// ── the thin client ─────────────────────────────────────────────────────────

/// ATTACH to the kernel `pid` already running in this directory: a View with
/// no state of its own. Returns when the owner ends or this client detaches;
/// the reason is said after the screen is restored.
pub async fn attach(paths: &Paths, pid: u32) -> Result<()> {
    let stream = crate::sys::connect_local(SOCKET).await.with_context(|| {
        format!("a kernel (pid {pid}) runs in this directory, but its terminal UI socket `{SOCKET}` does not answer (is it `fantastic mcp`?)")
    })?;
    let (mut r, mut w) = stream.into_split();
    match read_msg::<_, Down>(&mut r).await? {
        Some(Down::Hello { .. }) => {}
        other => anyhow::bail!("the TUI socket did not greet (got {other:?})"),
    }
    let _ = paths;
    let mut screen = crate::tty::open(std::path::Path::new(crate::sys::NULL_DEVICE))?;
    let why = run_client(&mut screen.term, &mut r, &mut w, pid).await;
    crate::tty::restore();
    match why {
        Ok(reason) => crate::tty::say(&format!("fantastic: thin client — {reason}")),
        Err(e) => crate::tty::say(&format!("fantastic: thin client — {e}")),
    }
    Ok(())
}

async fn run_client(
    term: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::fs::File>>,
    r: &mut crate::sys::LocalReadHalf,
    w: &mut crate::sys::LocalWriteHalf,
    pid: u32,
) -> Result<String> {
    let size = term.size()?;
    write_msg(
        w,
        &Up::Size {
            w: size.width,
            h: size.height,
        },
    )
    .await?;
    let mut keys = crossterm::event::EventStream::new();
    let mut buf: Option<Buffer> = None;
    let mut cursor: Option<(u16, u16)> = None;
    let mut captured = false;
    let reason = loop {
        tokio::select! {
            m = read_msg::<_, Down>(r) => match m {
                Ok(Some(Down::Full { buf: b, cursor: c, mouse })) => { buf = Some(b); cursor = c; captured = mouse_to(term, captured, mouse)?; }
                Ok(Some(Down::Diff { cells, cursor: c, mouse })) => {
                    if let Some(b) = buf.as_mut() {
                        for (x, y, cell) in cells {
                            if let Some(dst) = b.cell_mut((x, y)) {
                                *dst = cell;
                            }
                        }
                    }
                    cursor = c;
                    captured = mouse_to(term, captured, mouse)?;
                }
                Ok(Some(Down::Bye(why))) => break why,
                Ok(Some(Down::Hello { .. })) => {}
                Ok(None) | Err(_) => break format!("the kernel (pid {pid}) went away"),
            },
            k = keys.next() => match k {
                Some(Ok(crossterm::event::Event::Resize(cw, ch))) => write_msg(w, &Up::Size { w: cw, h: ch }).await?,
                Some(Ok(e)) => write_msg(w, &Up::Event(e)).await?,
                Some(Err(_)) | None => break "the terminal closed".into(),
            },
        }
        if let Some(b) = buf.as_ref() {
            paint(term, b, cursor, pid)?;
        }
    };
    if captured {
        let _ = crossterm::execute!(term.backend_mut(), crossterm::event::DisableMouseCapture);
    }
    Ok(reason)
}

fn mouse_to(
    term: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::fs::File>>,
    captured: bool,
    want: bool,
) -> Result<bool> {
    if want != captured {
        if want {
            crossterm::execute!(term.backend_mut(), crossterm::event::EnableMouseCapture)?;
        } else {
            crossterm::execute!(term.backend_mut(), crossterm::event::DisableMouseCapture)?;
        }
    }
    Ok(want)
}

/// The frame laid out for this terminal, then this client's badge.
fn paint(
    term: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::fs::File>>,
    frame: &Buffer,
    cursor: Option<(u16, u16)>,
    pid: u32,
) -> Result<()> {
    term.draw(|f| {
        let area = f.area();
        let out = f.buffer_mut();
        for y in 0..area.height.min(frame.area.height) {
            for x in 0..area.width.min(frame.area.width) {
                if let (Some(src), Some(dst)) = (frame.cell((x, y)), out.cell_mut((x, y))) {
                    *dst = src.clone();
                }
            }
        }
        let badge = badge(pid);
        let bw = badge.chars().count() as u16;
        if area.width > bw {
            out.set_string(
                area.width - bw,
                0,
                &badge,
                ratatui::style::Style::default()
                    .fg(crate::theme::ACCENT)
                    .bg(ratatui::style::Color::Black),
            );
        }
        if let Some((x, y)) = cursor.filter(|(x, y)| *x < area.width && *y < area.height) {
            f.set_cursor_position((x, y));
        }
    })?;
    Ok(())
}

/// The thin client's header.
pub fn badge(pid: u32) -> String {
    format!(" thin client · pid {pid} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    fn frame(w: u16, h: u16, text: &str) -> Frame {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        buf.set_string(0, 0, text, ratatui::style::Style::default());
        Frame {
            buf,
            cursor: None,
            mouse: false,
        }
    }

    #[test]
    fn the_first_frame_and_a_resize_go_whole_then_only_changed_cells() {
        let a = frame(10, 2, "hello");
        assert!(matches!(delta(None, &a), Some(Down::Full { .. })));
        let b = frame(10, 2, "hellO");
        match delta(Some(&a), &b) {
            Some(Down::Diff { cells, .. }) => assert_eq!(cells.len(), 1),
            other => panic!("{other:?}"),
        }
        assert!(
            delta(Some(&b), &b).is_none(),
            "nothing changed, nothing sent"
        );
        assert!(matches!(
            delta(Some(&b), &frame(12, 2, "x")),
            Some(Down::Full { .. })
        ));
    }

    #[test]
    fn messages_round_trip_the_wire() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let (mut a, mut b) = tokio::io::duplex(1 << 16);
            write_msg(&mut a, &Down::Full { buf: frame(4, 1, "ab").buf, cursor: Some((1, 0)), mouse: true }).await.unwrap();
            write_msg(&mut a, &Up::Event(crossterm::event::Event::Paste("hi".into()))).await.unwrap();
            match read_msg::<_, Down>(&mut b).await.unwrap() {
                Some(Down::Full { buf, cursor, mouse }) => {
                    assert_eq!(buf.cell((1, 0)).unwrap().symbol(), "b");
                    assert_eq!((cursor, mouse), (Some((1, 0)), true));
                }
                other => panic!("{other:?}"),
            }
            assert!(matches!(read_msg::<_, Up>(&mut b).await.unwrap(), Some(Up::Event(crossterm::event::Event::Paste(s))) if s == "hi"));
            drop(a);
            assert!(read_msg::<_, Up>(&mut b).await.unwrap().is_none(), "EOF is a clean end");
        });
    }

    #[test]
    fn a_view_owner_greets_with_hello_only() {
        let g = greeting(true, 7);
        assert_eq!(g.len(), 1, "a view owner gets no Bye");
        assert!(matches!(g[0], Down::Hello { pid: 7 }));
    }

    #[test]
    fn an_owner_without_a_view_says_so_and_ends() {
        let g = greeting(false, 7);
        assert_eq!(g.len(), 2, "the Hello is followed by a Bye");
        assert!(matches!(g[0], Down::Hello { pid: 7 }));
        match &g[1] {
            Down::Bye(why) => assert!(why.contains("without a terminal view"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_badge_says_thin_client_and_whose() {
        assert_eq!(badge(7), " thin client · pid 7 ");
    }
}
