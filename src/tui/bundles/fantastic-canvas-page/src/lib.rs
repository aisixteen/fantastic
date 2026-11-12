//! THE CANVAS PAGE, as an ordinary bundle built outside the kernel tree.
//!
//! A `canvas_page` agent is a child of a `web` host.
//! It serves the canvas app from the binary itself: `canvas.zip` (packed by
//! `build.rs`, embedded byte-for-byte) plus a one-tag `index.html` that loads the
//! zip's `bundle.min.js`, `home.html` (the same bundle, booting the Home) and
//! `list.html` (the example client of the `home` protocol). The host's generic
//! `GET /<id>/file/<path>` route pipes it, because this agent answers the SOURCE
//! verb `read_stream` and declares it on its record (`verbs`). Read-only: there
//! is nothing to write.
//!
//! Its verbs are declared in [`verbs`] (`reflect` and `docs` render them); its record
//! in [`record`].

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use fantastic_kernel::bundle::{BundleError, Handler, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

pub mod record;
pub mod verbs;

/// What the bundle is for: its one short man text; how to call it is rendered from
/// its declared verbs (`docs`, `reflect`).
const MAN: &str = "The canvas page's files, served read-only: the canvas app's pack embedded in the binary, streamed to the web host's file route.";

/// What a `canvas_page` record's `verbs` field carries so the web host serves it:
/// the `read_stream` verb as it is declared (its argument schema).
#[must_use]
pub fn read_stream_declared() -> Value {
    let verb = verbs::VERBS.iter().find(|v| v.name == "read_stream").expect("read_stream is declared");
    json!({"read_stream": verb.schema()})
}

/// The `handler_module` this bundle registers under.
pub const HANDLER_MODULE: &str = "canvas_page.tools";

/// The canvas app's pack, embedded at build time.
pub static CANVAS_ZIP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/canvas.zip"));

/// The mount page: the bundle inlines three, xterm and its stylesheet, so one
/// script tag is the whole page. Relative, so it loads under whatever id the
/// agent was given.
pub const INDEX_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Aisixteen Fantastic · canvas</title></head>\n<body><script type=\"module\" src=\"bundle.min.js\"></script></body></html>\n";

/// The Home's mount page: the same bundle, told by `data-page="home"` to boot
/// the Home (`src/tui/web/canvas/src/home.ts`) instead of the canvas.
pub const HOME_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Aisixteen Fantastic · home</title></head>\n<body data-page=\"home\"><script type=\"module\" src=\"bundle.min.js\"></script></body></html>\n";

/// The example client of the `home` protocol (`src/tui/web/home/list.html`),
/// served as `list.html`.
pub const LIST_HTML: &str = include_str!("../../../web/home/list.html");

/// Every served file, inflated once: the zip's entries plus `index.html`,
/// `home.html` and `list.html`.
fn files() -> &'static BTreeMap<String, Vec<u8>> {
    static FILES: OnceLock<BTreeMap<String, Vec<u8>>> = OnceLock::new();
    FILES.get_or_init(|| {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(CANVAS_ZIP)).expect("the embedded canvas.zip is a zip");
        let mut out = BTreeMap::new();
        for i in 0..zip.len() {
            let mut entry = zip.by_index(i).expect("an entry of the embedded zip");
            if entry.is_dir() {
                continue;
            }
            let mut bytes = Vec::with_capacity(entry.size() as usize);
            entry.read_to_end(&mut bytes).expect("inflate an entry of the embedded zip");
            let name = entry.name().expect("an entry name of the embedded zip").to_string();
            out.insert(name, bytes);
        }
        out.insert("index.html".into(), INDEX_HTML.as_bytes().to_vec());
        out.insert("home.html".into(), HOME_HTML.as_bytes().to_vec());
        out.insert("list.html".into(), LIST_HTML.as_bytes().to_vec());
        out
    })
}

/// The bundle. Stateless: the page is in the binary.
#[derive(Default)]
pub struct CanvasPageBundle;

fn read_stream(a: &verbs::ReadStream) -> (Value, Option<Vec<u8>>) {
    let path = a.path.trim_start_matches('/');
    let Some(bytes) = files().get(path) else {
        return (
            json!({
                "error": format!("canvas_page: no file {path:?} — this page serves {:?}", files().keys().collect::<Vec<_>>()),
                "reason": "not_found",
            }),
            None,
        );
    };
    let size = bytes.len() as u64;
    let offset = a.offset.unwrap_or(0).min(size);
    let length = a.length.unwrap_or(size - offset);
    let end = offset.saturating_add(length).min(size);
    let body = bytes[offset as usize..end as usize].to_vec();
    (
        json!({"path": path, "offset": offset, "next_offset": end, "eof": end >= size, "size": size}),
        Some(body),
    )
}

fn reflect(agent_id: &AgentId) -> Value {
    json!({
        "id": agent_id.as_str(),
        "sentence": "The canvas page's files, served read-only (transport only). To build on the canvas, use the page's view: `docs of=<page guid>|view_root`.",
        "files": files().keys().collect::<Vec<_>>(),
    })
}

#[async_trait]
impl Handler for CanvasPageBundle {
    type Call = verbs::Call;

    fn name(&self) -> &str {
        "canvas_page"
    }

    fn man(&self) -> &'static str {
        MAN
    }

    fn verbs(&self) -> &'static [fantastic_kernel::Verb] {
        verbs::VERBS
    }

    fn dynamic_verbs(&self, agent: &fantastic_kernel::Agent) -> Vec<fantastic_kernel::DynVerb> {
        verbs::dynamic_verbs(agent)
    }

    fn record(&self) -> &'static fantastic_kernel::RecordDecl {
        &record::RECORD
    }

    async fn handle(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        _kernel: &Arc<Kernel>,
    ) -> Result<Reply, BundleError> {
        Ok(Some(match call {
            verbs::Call::Reflect(_) => reflect(agent_id),
            verbs::Call::Boot(_) => json!({"booted": true}),
            verbs::Call::List(_) => json!({"files": files().keys().collect::<Vec<_>>()}),
            // Text-only callers get the head; the bytes ride a binary reply.
            verbs::Call::ReadStream(a) => read_stream(&a).0,
        }))
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        call: Self::Call,
        _body: Option<fantastic_kernel::Blob>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<fantastic_kernel::Blob>), BundleError> {
        if let verbs::Call::ReadStream(a) = &call {
            // the chunk rides at the reply's `bytes` key, as the file bridge's does
            let (head, body) = read_stream(a);
            return Ok((Some(head), body.map(|b| fantastic_kernel::Blob::key("bytes", b))));
        }
        Ok((self.handle(agent_id, call, kernel).await?, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rs(path: &str, offset: Option<u64>, length: Option<u64>) -> verbs::ReadStream {
        verbs::ReadStream { path: path.into(), offset, length }
    }

    #[test]
    fn the_embedded_page_is_the_pack_and_its_index() {
        let f = files();
        assert!(f.contains_key("bundle.min.js") && f.contains_key("readme.md") && f.contains_key("index.html"));
        assert!(f["bundle.min.js"].len() > 500_000, "vendors are inlined");
        // the readme stamps the sha256 of the bundle it ships (verify-before-serve)
        let readme = String::from_utf8_lossy(&f["readme.md"]);
        assert!(readme.contains("expected: "), "the pack stamped the readme");
    }

    #[test]
    fn home_html_mounts_the_bundle_as_the_home_and_list_html_is_the_list_page() {
        let f = files();
        let home = String::from_utf8_lossy(&f["home.html"]);
        assert!(home.contains("<body data-page=\"home\">"), "{home}");
        assert!(home.contains("src=\"bundle.min.js\""), "{home}");
        let list = String::from_utf8_lossy(&f["list.html"]);
        assert_eq!(list, LIST_HTML);
        assert!(list.contains("/home/ws"), "the list page watches home");
        let (head, body) = read_stream(&rs("home.html", None, None));
        assert_eq!(head["eof"], json!(true));
        assert_eq!(body.unwrap(), HOME_HTML.as_bytes());
        let (head, body) = read_stream(&rs("list.html", None, None));
        assert_eq!(head["size"], json!(LIST_HTML.len()));
        assert_eq!(body.unwrap(), LIST_HTML.as_bytes());
    }


    /// A kernel holding the agent `id` (its bundle is the one under test).
    fn kernel_with(id: &str) -> std::sync::Arc<Kernel> {
        let kernel = Arc::new(Kernel::new());
        kernel.register(fantastic_kernel::Agent::new(
            AgentId::from(id),
            None,
            None,
            Default::default(),
            std::path::PathBuf::new(),
            false,
        ));
        kernel
    }

    /// One call through the agent's entry: parsed and checked against the declared verbs
    /// first, then handled. The reply and its body.
    async fn ask(
        kernel: &Arc<Kernel>,
        id: &str,
        payload: Value,
    ) -> (Value, Option<fantastic_kernel::Blob>) {
        let (reply, body) =
            fantastic_kernel::Bundle::handle_binary(&CanvasPageBundle, &AgentId::from(id), payload, None, kernel)
                .await
                .expect("handled");
        (reply.unwrap_or(Value::Null), body)
    }

    /// Every declared verb is checked by the agent's own entry: an unknown verb is
    /// refused naming what the agent answers, an unknown argument and a missing
    /// required one by name.
    #[tokio::test]
    async fn the_canvas_page_checks_every_call_against_its_declared_verbs() {
        let kernel = kernel_with("canvas_page");
        let r = ask(&kernel, "canvas_page", json!({"type": "zzz_nope"})).await.0;
        assert_eq!(r["reason"], "unhandled", "{r}");
        for n in ["boot", "list", "read_stream", "reflect"] {
            assert!(r["error"].as_str().unwrap().contains(n), "{n} in {r}");
        }
        for v in verbs::VERBS {
            let r = ask(&kernel, "canvas_page", json!({"type": v.name, "zzz_unknown": 1})).await.0;
            assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("zzz_unknown")), "{}: {r}", v.name);
        }
        let r = ask(&kernel, "canvas_page", json!({"type": "read_stream"})).await.0;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("missing_arg"), Some("path")), "{r}");
        let r = ask(&kernel, "canvas_page", json!({"type": "read_stream", "path": 3})).await.0;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("path")), "{r}");
        let r = ask(&kernel, "canvas_page", json!({"type": "read_stream", "path": "index.html", "offset": "0"})).await.0;
        assert_eq!((r["reason"].as_str(), r["arg"].as_str()), (Some("bad_arg"), Some("offset")), "{r}");
    }

    #[tokio::test]
    async fn boot_list_reflect_and_read_stream_answer() {
        let kernel = kernel_with("canvas_page");
        assert_eq!(ask(&kernel, "canvas_page", json!({"type": "boot"})).await.0, json!({"booted": true}));
        let listed = ask(&kernel, "canvas_page", json!({"type": "list"})).await.0;
        assert!(listed["files"].as_array().unwrap().iter().any(|f| f == "index.html"), "{listed}");
        let r = ask(&kernel, "canvas_page", json!({"type": "reflect"})).await.0;
        assert_eq!(r["id"], "canvas_page", "{r}");
        let (head, body) = ask(&kernel, "canvas_page", json!({"type": "read_stream", "path": "/index.html", "length": 6})).await;
        assert_eq!(head["next_offset"], json!(6), "{head}");
        assert_eq!(body.unwrap().bytes, INDEX_HTML.as_bytes()[..6]);
        // the declaration the web host reads off the record names `read_stream`
        let declared = read_stream_declared();
        assert!(declared["read_stream"]["properties"].get("path").is_some(), "{declared}");
        assert_eq!(declared["read_stream"]["required"], json!(["path"]), "{declared}");
    }

    #[test]
    fn read_stream_pages_through_a_file_and_refuses_an_unknown_one() {
        let (head, body) = read_stream(&rs("index.html", Some(0), Some(10)));
        assert_eq!(head["next_offset"], json!(10));
        assert_eq!(head["eof"], json!(false));
        assert_eq!(body.unwrap(), INDEX_HTML.as_bytes()[..10]);
        let (head, body) = read_stream(&rs("index.html", Some(10), None));
        assert_eq!(head["eof"], json!(true));
        assert_eq!(body.unwrap(), INDEX_HTML.as_bytes()[10..]);
        let (head, body) = read_stream(&rs("../secret", None, None));
        assert_eq!(head["reason"], json!("not_found"));
        assert!(body.is_none());
    }
}
