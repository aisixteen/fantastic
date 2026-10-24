//! The REAL engine. On macOS: the swift `FMCore` through the `FantasticFMBridge`
//! C ABI (`swift/Sources/FantasticFMBridge/fm_bridge.h`), linked
//! statically by `build.rs`. Everywhere else: a refusal naming the platform —
//! nothing is built, nothing falls back.

use crate::engine::{FmAvailability, FmEngine, FmRequest, FmStream};

#[cfg(target_os = "macos")]
pub use bridge::SystemEngine;

#[cfg(not(target_os = "macos"))]
pub use elsewhere::SystemEngine;

/// The refusing engine, compiled into the macOS test build too so its words are
/// pinned on the machine that runs the suite.
#[cfg(all(test, target_os = "macos"))]
pub(crate) use elsewhere::{platform_name, SystemEngine as ElsewhereEngine};

#[cfg(all(test, target_os = "macos"))]
pub(crate) use bridge::raw as bridge_raw;

#[cfg(target_os = "macos")]
mod bridge {
    //! The safe wrapper over the five C functions.
    //!
    //! OWNERSHIP (as `fm_bridge.h` states it):
    //!   * `fm_availability`'s string is ours: copied, then released with
    //!     `fm_free` — never `free`.
    //!   * `request_json` is borrowed for the `fm_stream` call only (the bridge
    //!     copies it before returning).
    //!   * `ctx` is a `Box<Ctx>` we leak into `fm_stream` and take back in
    //!     `on_done` — the bridge calls `on_done` EXACTLY ONCE, last, so that is
    //!     where it is released. If `fm_stream` returns 0 nothing is ever called
    //!     back, and the box is reclaimed here.
    //!   * `bytes` / `error` in the callbacks are borrowed for that call: copied.
    //!
    //! THREADING: the callbacks run on a Swift-concurrency pool thread, serially
    //! per handle, possibly before `fm_stream` returns. They only push onto an
    //! unbounded channel (never block, never touch a runtime) and never panic.

    use super::{FmAvailability, FmEngine, FmRequest, FmStream};
    use crate::engine::FmEvent;
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::sync::OnceLock;
    use tokio::sync::mpsc;

    type ChunkFn = unsafe extern "C" fn(ctx: *mut c_void, bytes: *const u8, len: usize);
    type DoneFn = unsafe extern "C" fn(ctx: *mut c_void, error: *const c_char, prompt_tokens: i64);

    extern "C" {
        fn fm_availability() -> *mut c_char;
        fn fm_context_size() -> i64;
        fn fm_stream(
            request_json: *const c_char,
            ctx: *mut c_void,
            on_chunk: Option<ChunkFn>,
            on_done: Option<DoneFn>,
        ) -> u64;
        fn fm_cancel(handle: u64);
        fn fm_free(p: *mut c_char);
    }

    /// What `ctx` points at: the sending half of the stream's channel.
    struct Ctx {
        tx: mpsc::UnboundedSender<FmEvent>,
    }

    /// `on_chunk`: one UTF-8 delta. A delta that is not UTF-8 breaks the
    /// contract; it is reported as such (the generation is cancelled when the
    /// consumer, having read that, drops the stream).
    unsafe extern "C" fn on_chunk(ctx: *mut c_void, bytes: *const u8, len: usize) {
        if ctx.is_null() || bytes.is_null() || len == 0 {
            return;
        }
        // SAFETY: `ctx` is the `Box<Ctx>` leaked in `stream`, alive until
        // `on_done` (which never runs concurrently with this, per the header);
        // `bytes` is valid for `len` bytes for the duration of this call.
        let ctx = unsafe { &*(ctx as *const Ctx) };
        let slice = unsafe { std::slice::from_raw_parts(bytes, len) };
        let ev = match std::str::from_utf8(slice) {
            Ok(s) => FmEvent::Text(s.to_owned()),
            Err(e) => FmEvent::Broken(format!(
                "the on-device stream sent a delta that is not UTF-8 ({e})"
            )),
        };
        // A closed receiver = the consumer is gone (and has cancelled).
        let _ = ctx.tx.send(ev);
    }

    /// `on_done`: the end, exactly once, last — releases `ctx`.
    unsafe extern "C" fn on_done(ctx: *mut c_void, error: *const c_char, prompt_tokens: i64) {
        if ctx.is_null() {
            return;
        }
        // SAFETY: the bridge calls `on_done` exactly once per handle, after every
        // `on_chunk`; this takes back the box `stream` leaked.
        let ctx = unsafe { Box::from_raw(ctx as *mut Ctx) };
        let error = if error.is_null() {
            None
        } else {
            // SAFETY: a NUL-terminated string, borrowed for this call.
            Some(
                unsafe { CStr::from_ptr(error) }
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        let _ = ctx.tx.send(FmEvent::Done {
            error,
            prompt_tokens,
        });
    }

    /// The system model through the bridge. The window is read once — it does
    /// not change while a process runs.
    #[derive(Default)]
    pub struct SystemEngine {
        window: OnceLock<i64>,
    }

    impl SystemEngine {
        /// The engine over the linked bridge.
        #[must_use]
        pub fn new() -> Self {
            Self::default()
        }

        /// Start a generation from a raw `request_json` — what crosses the ABI.
        /// [`FmEngine::stream`] serializes an [`FmRequest`] into it; the unit
        /// suite sends malformed documents through it to pin the bridge's own
        /// refusals.
        pub fn stream_json(&self, request_json: &str) -> FmStream {
            let Ok(json) = CString::new(request_json) else {
                return FmStream::failed(
                    "request_json carries a NUL byte — the C ABI takes a NUL-terminated string"
                        .to_string(),
                );
            };
            let (tx, rx) = mpsc::unbounded_channel();
            let ctx = Box::into_raw(Box::new(Ctx { tx })) as *mut c_void;
            // SAFETY: `json` outlives the call (the bridge copies it before
            // returning); `ctx` is released by `on_done`; both callbacks are
            // `extern "C"` and non-NULL.
            let handle = unsafe { fm_stream(json.as_ptr(), ctx, Some(on_chunk), Some(on_done)) };
            if handle == 0 {
                // Nothing will ever be called back: the box is still ours.
                // SAFETY: the pointer came from `Box::into_raw` above and the
                // bridge kept no copy.
                drop(unsafe { Box::from_raw(ctx as *mut Ctx) });
                return FmStream::failed(
                    "fm_stream refused to start (handle 0: a NULL callback)".to_string(),
                );
            }
            // SAFETY: cancelling any handle is allowed; a finished one is a no-op.
            FmStream::new(rx, move || unsafe { fm_cancel(handle) })
        }
    }

    impl FmEngine for SystemEngine {
        fn availability(&self) -> FmAvailability {
            // SAFETY: returns a heap string the caller owns, or NULL.
            let p = unsafe { fm_availability() };
            if p.is_null() {
                return FmAvailability::Unavailable("fm_availability returned NULL".to_string());
            }
            // SAFETY: a NUL-terminated string from the bridge; copied, then
            // released with the bridge's own `fm_free`.
            let raw = unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned();
            unsafe { fm_free(p) };
            FmAvailability::from_bridge_json(&raw)
        }

        fn context_size(&self) -> i64 {
            // SAFETY: no arguments, no ownership.
            *self.window.get_or_init(|| unsafe { fm_context_size() })
        }

        fn stream(&self, request: &FmRequest) -> FmStream {
            self.stream_json(&request.to_json())
        }
    }

    /// The raw C calls the hermetic FFI tests make where the safe wrapper never
    /// would (a NULL `fm_free`, an unknown handle, NULL callbacks).
    #[cfg(test)]
    pub(crate) mod raw {
        use std::ffi::{c_char, c_void};

        /// `fm_free(NULL)` — must be a no-op.
        pub fn free_null() {
            // SAFETY: NULL is documented as a no-op.
            unsafe { super::fm_free(std::ptr::null_mut::<c_char>()) }
        }

        /// `fm_cancel` on a handle that was never issued — must be a no-op.
        pub fn cancel_unknown(handle: u64) {
            // SAFETY: any handle may be cancelled.
            unsafe { super::fm_cancel(handle) }
        }

        /// `fm_stream` with NULL callbacks — must return 0 and call nothing.
        pub fn stream_without_callbacks() -> u64 {
            // SAFETY: NULL callbacks are documented to return 0 without a call.
            unsafe { super::fm_stream(c"{}".as_ptr(), std::ptr::null_mut::<c_void>(), None, None) }
        }
    }
}

#[cfg(any(not(target_os = "macos"), test))]
mod elsewhere {
    //! Not macOS: the bridge is not built and the model cannot run. Every answer
    //! is the refusal naming the platform — FMCore's own sentence.

    use super::{FmAvailability, FmEngine, FmRequest, FmStream};
    use crate::engine::{unavailable_message, FmReason, FM_DOCUMENTED_CONTEXT_SIZE};

    /// What `os` (`std::env::consts::OS`) is called in the refusal — swift
    /// `SystemFMEngine.platformName` for the platforms both runtimes build for,
    /// the OS's own name otherwise.
    pub(crate) fn platform_name(os: &str) -> &str {
        match os {
            "linux" => "Linux",
            "ios" => "iOS",
            "visionos" => "visionOS",
            "tvos" => "tvOS",
            "watchos" => "watchOS",
            "windows" => "Windows",
            other => other,
        }
    }

    /// The refusal for this build's platform.
    fn here() -> String {
        FmReason::platform(platform_name(std::env::consts::OS))
    }

    /// The refusing engine.
    #[derive(Default)]
    pub struct SystemEngine;

    impl SystemEngine {
        /// The refusing engine.
        #[must_use]
        pub fn new() -> Self {
            Self
        }
    }

    impl FmEngine for SystemEngine {
        fn availability(&self) -> FmAvailability {
            FmAvailability::Unavailable(here())
        }

        fn context_size(&self) -> i64 {
            FM_DOCUMENTED_CONTEXT_SIZE
        }

        fn stream(&self, _request: &FmRequest) -> FmStream {
            FmStream::failed(unavailable_message(&here()))
        }
    }
}
