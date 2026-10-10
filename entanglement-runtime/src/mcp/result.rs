//! Mapping an MCP `tools/call` result onto a host tool result (#727).
//!
//! Text blocks become text; an embedded `resource`'s text and a
//! `resource_link` become descriptive lines; `structuredContent` is rendered
//! as JSON only when no text block carried the answer (the spec asks servers
//! to mirror it in text, so printing both would just duplicate it). `isError`
//! surfaces as the structured `is_error` side channel (ADR-0176) on
//! [`McpResult`].
//!
//! **Binary is never inlined.** `image`, `audio` and a `resource` `blob`
//! become a `[binary omitted: …]` placeholder naming mime, size and uri. WHY:
//! binary through MCP bloats the context, and an image part the provider
//! rejects is replayed on every later request (and on resume), poisoning the
//! session. An agent that needs the bytes fetches them through an endpoint/API
//! tool, not through an MCP result.

use base64::Engine as _;
use entanglement_core::ContentPart;
use serde_json::Value;

use crate::host::truncate_output;

/// Prefix on an `isError` result's text. Kept alongside the structured flag:
/// `is_error` reaches heads, hooks and the transcript, but no provider wire
/// carries it to the model, so the model still needs to read the failure.
const ERROR_PREFIX: &str = "MCP tool reported an error: ";

/// A rendered `tools/call` result: the content parts the model reads plus
/// whether the server flagged it `isError`.
#[derive(Debug, PartialEq)]
pub(crate) struct McpResult {
    pub content: Vec<ContentPart>,
    pub is_error: bool,
}

/// Accumulates the rendered lines into one text part.
#[derive(Default)]
struct Parts {
    parts: Vec<ContentPart>,
    text: String,
}

impl Parts {
    fn line(&mut self, s: &str) {
        self.text.push_str(s);
        self.text.push('\n');
    }

    fn flush(&mut self) {
        let text = self.text.trim_end();
        if !text.is_empty() {
            // Same 32 KiB bound every host tool honors: a chatty server can't
            // flood the context in a single call.
            self.parts
                .push(ContentPart::text(truncate_output(text.to_string())));
        }
        self.text.clear();
    }
}

pub(crate) fn render_result(result: &Value) -> McpResult {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut out = Parts::default();
    if is_error {
        out.text.push_str(ERROR_PREFIX);
    }
    let blocks = result.get("content").and_then(Value::as_array);
    let mut saw_text = false;
    for block in blocks.into_iter().flatten() {
        saw_text |= render_block(block, &mut out);
    }
    if !saw_text {
        if let Some(structured) = result.get("structuredContent").filter(|v| !v.is_null()) {
            out.line(&structured.to_string());
        }
    }
    out.flush();
    if out.parts.is_empty() {
        out.parts.push(ContentPart::text("(no content)"));
    }
    McpResult {
        content: out.parts,
        is_error,
    }
}

/// Render one content block into `out`; `true` when it was a text block.
fn render_block(block: &Value, out: &mut Parts) -> bool {
    let str_of = |v: &Value, key: &str| v.get(key).and_then(Value::as_str).map(str::to_string);
    let mime = str_of(block, "mimeType").unwrap_or_else(|| "unknown type".to_string());
    match block.get("type").and_then(Value::as_str) {
        Some("text") => {
            if let Some(t) = str_of(block, "text") {
                out.line(&t);
            }
            return true;
        }
        Some("image") | Some("audio") => {
            let data = str_of(block, "data");
            out.line(&binary_placeholder(&mime, data.as_deref(), None));
        }
        Some("resource") => {
            let res = block.get("resource").unwrap_or(&Value::Null);
            let uri = str_of(res, "uri").unwrap_or_default();
            let mime = str_of(res, "mimeType").unwrap_or_else(|| "unknown type".to_string());
            match (str_of(res, "text"), str_of(res, "blob")) {
                (Some(text), _) => out.line(&format!("[resource {uri} ({mime})]\n{text}")),
                (None, Some(blob)) => out.line(&binary_placeholder(&mime, Some(&blob), Some(&uri))),
                (None, None) => out.line(&format!("[resource {uri} ({mime})]")),
            }
        }
        Some("resource_link") => {
            let uri = str_of(block, "uri").unwrap_or_default();
            let name = str_of(block, "name").unwrap_or_else(|| uri.clone());
            let mut line = format!("[resource link: {name} <{uri}> ({mime})");
            if let Some(desc) = str_of(block, "description") {
                line.push_str(&format!(" — {desc}"));
            }
            line.push(']');
            out.line(&line);
        }
        Some(other) => out.line(&format!("[{other} content omitted]")),
        None => {}
    }
    false
}

/// `[binary omitted: <mime>, <size>, uri=<uri>]` — the only form binary
/// content takes (see the module docs for why). The size is measured by
/// decoding; malformed or missing base64 reads `size unknown`, never a panic.
fn binary_placeholder(mime: &str, b64: Option<&str>, uri: Option<&str>) -> String {
    let size = b64
        .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok())
        .map(|bytes| human_size(bytes.len()))
        .unwrap_or_else(|| "size unknown".to_string());
    match uri.filter(|u| !u.is_empty()) {
        Some(uri) => format!("[binary omitted: {mime}, {size}, uri={uri}]"),
        None => format!("[binary omitted: {mime}, {size}]"),
    }
}

fn human_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        format!("{bytes} B")
    } else if b < KIB * KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{:.1} MiB", b / (KIB * KIB))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn texts(r: &McpResult) -> Vec<&str> {
        r.content.iter().filter_map(ContentPart::as_text).collect()
    }

    #[test]
    fn renders_text_blocks_as_one_part() {
        let r = render_result(&json!({ "content": [
            { "type": "text", "text": "hello" }, { "type": "text", "text": "world" }
        ] }));
        assert_eq!(texts(&r), ["hello\nworld"]);
        assert!(!r.is_error);
    }

    #[test]
    fn is_error_sets_the_structured_flag_and_keeps_the_model_facing_prefix() {
        let r = render_result(
            &json!({ "isError": true, "content": [ { "type": "text", "text": "boom" } ] }),
        );
        assert!(r.is_error);
        assert_eq!(texts(&r), ["MCP tool reported an error: boom"]);
    }

    #[test]
    fn image_and_audio_become_placeholders_without_a_uri() {
        let r = render_result(&json!({ "content": [
            { "type": "text", "text": "before" },
            { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
            { "type": "audio", "data": "A".repeat(2048), "mimeType": "audio/wav" },
        ] }));
        assert_eq!(
            texts(&r),
            ["before\n[binary omitted: image/png, 8 B]\n[binary omitted: audio/wav, 1.5 KiB]"]
        );
        // Text only — no image part ever reaches the context.
        assert!(r.content.iter().all(|p| p.as_text().is_some()));
    }

    #[test]
    fn malformed_or_missing_base64_reads_size_unknown() {
        let r = render_result(&json!({ "content": [
            { "type": "image", "data": "%%% not base64", "mimeType": "image/png" },
            { "type": "audio" },
        ] }));
        assert_eq!(
            texts(&r),
            ["[binary omitted: image/png, size unknown]\n[binary omitted: unknown type, size unknown]"]
        );
    }

    #[test]
    fn human_sizes() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1023), "1023 B");
        assert_eq!(human_size(1536), "1.5 KiB");
        assert_eq!(human_size(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn resources_and_links_render_uri_and_payload_summary() {
        let r = render_result(&json!({ "content": [
            { "type": "resource", "resource": { "uri": "file:///a.txt", "mimeType": "text/plain", "text": "body" } },
            { "type": "resource", "resource": { "uri": "file:///b.bin", "blob": "AAAAAA==" } },
            { "type": "resource_link", "uri": "https://x/y", "name": "y", "mimeType": "text/html", "description": "a page" },
        ] }));
        assert_eq!(
            texts(&r),
            ["[resource file:///a.txt (text/plain)]\nbody\n\
              [binary omitted: unknown type, 4 B, uri=file:///b.bin]\n\
              [resource link: y <https://x/y> (text/html) — a page]"]
        );
    }

    #[test]
    fn structured_content_is_json_only_without_text() {
        let structured = json!({ "temp": 21 });
        let r = render_result(&json!({ "content": [], "structuredContent": structured }));
        assert_eq!(texts(&r), [r#"{"temp":21}"#]);
        let r = render_result(&json!({
            "content": [ { "type": "text", "text": "21 degrees" } ],
            "structuredContent": structured,
        }));
        assert_eq!(texts(&r), ["21 degrees"]);
    }

    #[test]
    fn unknown_blocks_and_empty_results() {
        let r = render_result(&json!({ "content": [ { "type": "hologram" } ] }));
        assert_eq!(texts(&r), ["[hologram content omitted]"]);
        assert_eq!(
            texts(&render_result(&json!({ "content": [] }))),
            ["(no content)"]
        );
    }

    #[test]
    fn oversized_text_is_byte_capped() {
        let big = "x".repeat(crate::host::MAX_OUTPUT_BYTES + 1024);
        let r = render_result(&json!({ "content": [ { "type": "text", "text": big } ] }));
        let out = texts(&r)[0];
        assert!(
            out.contains("[truncated:"),
            "tail: …{}",
            &out[out.len() - 60..]
        );
        assert!(
            out.len() < crate::host::MAX_OUTPUT_BYTES + 128,
            "len={}",
            out.len()
        );
    }
}
