//! #210: a thin client that attaches to a kernel without a terminal view is
//! told so and ends. These drive `share::listen` over the real socket and read
//! the wire, so they hold whatever the `share` module's internals look like.

use std::path::PathBuf;

use tokio::io::AsyncReadExt;
use tokio::sync::{mpsc, watch};

use crate::share::{self, Down, SOCKET};

async fn read_down<R: AsyncReadExt + Unpin>(r: &mut R) -> Option<Down> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await.ok()?;
    let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut body).await.ok()?;
    serde_json::from_slice(&body).ok()
}

/// Run `f` in a scratch directory: the socket is relative to the cwd.
fn in_scratch_dir(f: impl FnOnce()) {
    let original = std::env::current_dir().expect("cwd");
    let dir: PathBuf = std::env::temp_dir().join(format!("fantastic-share-test-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".fantastic/cli")).expect("scratch dir");
    std::env::set_current_dir(&dir).expect("enter scratch dir");
    f();
    share::close();
    std::env::set_current_dir(&original).expect("leave scratch dir");
}

#[test]
fn thin_clients_are_greeted_by_the_kernel_they_attach_to() {
    in_scratch_dir(|| {
        // A kernel with no terminal view: the client is told why, and ends.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(async {
            let (tx, _rx) = mpsc::channel(8);
            let (_ftx, frames) = watch::channel(None);
            let _owner = share::listen(tx, frames, false).expect("listen");
            let stream = crate::sys::connect_local(SOCKET).await.expect("connect");
            let (mut r, _w) = stream.into_split();
            assert!(matches!(read_down(&mut r).await, Some(Down::Hello { .. })), "greeted first");
            match read_down(&mut r).await {
                Some(Down::Bye(why)) => assert!(why.contains("without a terminal view"), "{why}"),
                other => panic!("expected a Bye, got {other:?}"),
            }
            assert!(read_down(&mut r).await.is_none(), "the client ends after the Bye");
        });
        drop(rt);
        share::close();

        // A kernel with a view: the client is greeted with a Hello, and no Bye.
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(async {
            let (tx, _rx) = mpsc::channel(8);
            let (_ftx, frames) = watch::channel(None);
            let _owner = share::listen(tx, frames, true).expect("listen");
            let stream = crate::sys::connect_local(SOCKET).await.expect("connect");
            let (mut r, _w) = stream.into_split();
            match read_down(&mut r).await {
                Some(Down::Hello { pid }) => assert_eq!(pid, std::process::id()),
                other => panic!("expected a Hello, got {other:?}"),
            }
        });
    });
}
