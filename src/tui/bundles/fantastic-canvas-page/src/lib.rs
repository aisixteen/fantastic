//! THE CANVAS PAGE, as an ordinary bundle built outside the kernel tree.
//!
//! A `canvas_page` agent is a child of a `web` host.
//! It serves the canvas app from the binary itself: `canvas.zip` (packed by
//! `build.rs`, embedded byte-for-byte) plus a one-tag `index.html` that loads the
//! zip's `bundle.min.js`. The host's generic `GET /<id>/file/<path>` route pipes
//! it, because this agent answers the SOURCE verb `read_stream` and declares it
//! on its record (`verbs`). Read-only: there is nothing to write.
//!
//! Its verbs: `reflect`, `boot` (no-op), `read_stream {path, offset?, length?}`
//! (a BINARY reply: the bytes as the body, `{path, offset, next_offset, eof,
//! size}` as the head), `list` (the served paths).

use std::collections::BTreeMap;
use std::io::Read;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use fantastic_kernel::bundle::{Bundle, BundleError, Reply};
use fantastic_kernel::{AgentId, Kernel};
use serde_json::{json, Value};

/// The `handler_module` this bundle registers under.
pub const HANDLER_MODULE: &str = "canvas_page.tools";

/// The canvas app's pack, embedded at build time.
pub static CANVAS_ZIP: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/canvas.zip"));

/// The mount page: the bundle inlines three, xterm and its stylesheet, so one
/// script tag is the whole page. Relative, so it loads under whatever id the
/// agent was given.
pub const INDEX_HTML: &str = "<!doctype html><html><head><meta charset=\"utf-8\"><title>Aisixteen Fantastic · canvas</title></head>\n<body><script type=\"module\" src=\"bundle.min.js\"></script></body></html>\n";

/// What `read_stream` declares on the record, so the host serves it.
pub const READ_STREAM_DOC: &str = "args: path:str (req), offset:int?, length:int?. A file of the embedded canvas page as a BINARY reply — the bytes as the body, {path, offset, next_offset, eof, size} as the head.";

/// Every served file, inflated once: the zip's entries plus `index.html`.
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
            out.insert(entry.name().to_string(), bytes);
        }
        out.insert("index.html".into(), INDEX_HTML.as_bytes().to_vec());
        out
    })
}

/// The bundle. Stateless: the page is in the binary.
#[derive(Default)]
pub struct CanvasPageBundle;

fn read_stream(payload: &Value) -> (Value, Option<Vec<u8>>) {
    let path = payload.get("path").and_then(Value::as_str).unwrap_or("").trim_start_matches('/');
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
    let offset = payload.get("offset").and_then(Value::as_u64).unwrap_or(0).min(size);
    let length = payload.get("length").and_then(Value::as_u64).unwrap_or(size - offset);
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
        "sentence": "The canvas page, embedded in this binary: served read-only through the parent web host's /file/ route.",
        "files": files().keys().collect::<Vec<_>>(),
        "verbs": {
            "reflect": "No args.",
            "boot": "No-op; the parent `web` host serves the route.",
            "read_stream": READ_STREAM_DOC,
            "list": "No args. The served paths.",
        },
    })
}

#[async_trait]
impl Bundle for CanvasPageBundle {
    fn name(&self) -> &str {
        "canvas_page"
    }

    async fn handle(&self, agent_id: &AgentId, payload: &Value, _kernel: &Arc<Kernel>) -> Result<Reply, BundleError> {
        let verb = payload.get("type").and_then(Value::as_str).unwrap_or("");
        Ok(Some(match verb {
            "reflect" => reflect(agent_id),
            "boot" => json!({"booted": true}),
            "list" => json!({"files": files().keys().collect::<Vec<_>>()}),
            // Text-only callers get the head; the bytes ride a binary reply.
            "read_stream" => read_stream(payload).0,
            other => json!({"error": format!("canvas_page: unknown type {other:?}"), "reason": "unhandled"}),
        }))
    }

    async fn handle_binary(
        &self,
        agent_id: &AgentId,
        header: Value,
        _blob: Vec<u8>,
        kernel: &Arc<Kernel>,
    ) -> Result<(Reply, Option<Vec<u8>>), BundleError> {
        if header.get("type").and_then(Value::as_str) == Some("read_stream") {
            let (head, body) = read_stream(&header);
            return Ok((Some(head), body));
        }
        Ok((self.handle(agent_id, &header, kernel).await?, None))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn read_stream_pages_through_a_file_and_refuses_an_unknown_one() {
        let (head, body) = read_stream(&json!({"path": "index.html", "offset": 0, "length": 10}));
        assert_eq!(head["next_offset"], json!(10));
        assert_eq!(head["eof"], json!(false));
        assert_eq!(body.unwrap(), INDEX_HTML.as_bytes()[..10]);
        let (head, body) = read_stream(&json!({"path": "index.html", "offset": 10}));
        assert_eq!(head["eof"], json!(true));
        assert_eq!(body.unwrap(), INDEX_HTML.as_bytes()[10..]);
        let (head, body) = read_stream(&json!({"path": "../secret"}));
        assert_eq!(head["reason"], json!("not_found"));
        assert!(body.is_none());
    }
}
