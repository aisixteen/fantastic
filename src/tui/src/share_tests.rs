//! #210: a thin client that attaches to a kernel without a terminal view is
//! told so and ends. These drive `share::listen` over the real socket and read
//! the wire, so they hold whatever the `share` module's internals look like.

use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use crate::local::TUI;
use crate::share::{self, Down};

async fn read_down<R: AsyncReadExt + Unpin>(r: &mut R) -> Option<Down> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.ok()?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Run `f` with a scratch `cli` folder: the channel's endpoint file lives there.
fn in_scratch_dir(f: impl FnOnce(&Path)) {
    let dir: PathBuf = std::env::temp_dir().join(format!("fantastic-share-test-{}", std::process::id()));
    let cli = dir.join(".fantastic/cli");
    std::fs::create_dir_all(&cli).expect("scratch dir");
    f(&cli);
    share::close(&cli);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn thin_clients_are_greeted_by_the_kernel_they_attach_to() {
    in_scratch_dir(|cli| {
        // A kernel with no terminal view: the client is told why, and ends.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(async {
            let (tx, _rx) = mpsc::channel(8);
            let (_ftx, frames) = watch::channel(None);
            let _owner = share::listen(tx, frames, false, cli).expect("listen");
            let stream = crate::local::connect(cli, TUI).await.expect("connect");
            let (mut r, _w) = stream.into_split();
            assert!(matches!(read_down(&mut r).await, Some(Down::Hello { .. })), "greeted first");
            match read_down(&mut r).await {
                Some(Down::Bye(why)) => assert!(why.contains("without a terminal view"), "{why}"),
                other => panic!("expected a Bye, got {other:?}"),
            }
            assert!(read_down(&mut r).await.is_none(), "the client ends after the Bye");
        });
        drop(rt);
        share::close(cli);

        // A kernel with a view: the client is greeted with a Hello, and no Bye.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(async {
            let (tx, _rx) = mpsc::channel(8);
            let (_ftx, frames) = watch::channel(None);
            let _owner = share::listen(tx, frames, true, cli).expect("listen");
            let stream = crate::local::connect(cli, TUI).await.expect("connect");
            let (mut r, _w) = stream.into_split();
            match read_down(&mut r).await {
                Some(Down::Hello { pid }) => assert_eq!(pid, std::process::id()),
                other => panic!("expected a Hello, got {other:?}"),
            }
        });
    });
}
