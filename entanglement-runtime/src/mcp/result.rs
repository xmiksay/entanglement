//! Mapping an MCP `tools/call` result onto a host tool result (#727).
//!
//! Text blocks become text, a provider-renderable `image` block becomes a real
//! image [`ContentPart`] (the same block `read` returns for an image file,
//! #221), and everything else — audio, embedded `resource`s, `resource_link`s,
//! oversize or unsupported images — becomes a descriptive text line instead of
//! vanishing. `structuredContent` is rendered as JSON only when no text block
//! carried the answer (the spec asks servers to mirror it in text, so printing
//! both would just duplicate it). `isError` surfaces as the structured
//! `is_error` side channel (ADR-0176) on [`McpResult`].

use entanglement_core::ContentPart;
use serde_json::Value;

use crate::host::truncate_output;

/// Prefix on an `isError` result's text. Kept alongside the structured flag:
/// `is_error` reaches heads, hooks and the transcript, but no provider wire
/// carries it to the model, so the model still needs to read the failure.
const ERROR_PREFIX: &str = "MCP tool reported an error: ";

/// Media types the providers accept as an inline image block — the same set
/// `read` recognizes. Anything else is described, not inlined.
const INLINE_IMAGE_TYPES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// Largest decoded image inlined (Anthropic's per-image ceiling). A bigger one
/// would fail the whole next LLM request, not just this tool call.
const MAX_INLINE_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// A rendered `tools/call` result: the content parts the model reads plus
/// whether the server flagged it `isError`.
#[derive(Debug, PartialEq)]
pub(crate) struct McpResult {
    pub content: Vec<ContentPart>,
    pub is_error: bool,
}

/// Accumulates text lines between non-text parts so consecutive text blocks
/// fold into one text part, keeping image parts in their original order.
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

    fn image(&mut self, mime: &str, data: &str) {
        self.flush();
        self.parts.push(ContentPart::image(mime, data));
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
        Some("image") => {
            let data = str_of(block, "data").unwrap_or_default();
            let bytes = decoded_len(&data);
            if !INLINE_IMAGE_TYPES.contains(&mime.as_str()) {
                out.line(&format!(
                    "[image omitted: {mime}, {bytes} bytes — not an inlinable type]"
                ));
            } else if bytes > MAX_INLINE_IMAGE_BYTES {
                out.line(&format!(
                    "[image omitted: {mime}, {bytes} bytes — over the {MAX_INLINE_IMAGE_BYTES}-byte inline cap]"
                ));
            } else if bytes == 0 {
                out.line(&format!("[image omitted: {mime}, empty]"));
            } else {
                out.image(&mime, &data);
            }
        }
        Some("audio") => {
            let bytes = decoded_len(&str_of(block, "data").unwrap_or_default());
            out.line(&format!("[audio omitted: {mime}, {bytes} bytes]"));
        }
        Some("resource") => {
            let res = block.get("resource").unwrap_or(&Value::Null);
            let uri = str_of(res, "uri").unwrap_or_default();
            let mime = str_of(res, "mimeType").unwrap_or_else(|| "unknown type".to_string());
            match (str_of(res, "text"), str_of(res, "blob")) {
                (Some(text), _) => out.line(&format!("[resource {uri} ({mime})]\n{text}")),
                (None, Some(blob)) => out.line(&format!(
                    "[resource {uri} ({mime}): binary, {} bytes omitted]",
                    decoded_len(&blob)
                )),
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

/// Decoded size of a base64 payload, without decoding it (padding-aware).
fn decoded_len(b64: &str) -> usize {
    let trimmed = b64.trim_end_matches('=');
    trimmed.len() * 3 / 4
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
    fn inlinable_image_becomes_an_image_part_in_order() {
        let r = render_result(&json!({ "content": [
            { "type": "text", "text": "before" },
            { "type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png" },
            { "type": "text", "text": "after" },
        ] }));
        assert_eq!(
            r.content,
            [
                ContentPart::text("before"),
                ContentPart::image("image/png", "iVBORw0KGgo="),
                ContentPart::text("after"),
            ]
        );
    }

    #[test]
    fn unsupported_or_oversize_images_are_described_not_dropped() {
        let r = render_result(&json!({ "content": [
            { "type": "image", "data": "AAAA", "mimeType": "image/tiff" },
        ] }));
        assert_eq!(
            texts(&r),
            ["[image omitted: image/tiff, 3 bytes — not an inlinable type]"]
        );
        let big = "A".repeat(MAX_INLINE_IMAGE_BYTES / 3 * 4 + 8);
        let r = render_result(&json!({ "content": [
            { "type": "image", "data": big, "mimeType": "image/png" },
        ] }));
        assert!(texts(&r)[0].contains("inline cap"), "{:?}", texts(&r));
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
              [resource file:///b.bin (unknown type): binary, 4 bytes omitted]\n\
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
