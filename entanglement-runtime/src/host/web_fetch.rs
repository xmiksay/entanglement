//! `web_fetch` (#736, ADR-0220): fetch one http(s) URL and hand the model
//! readable text. The network side — SSRF guard, pinned DNS, re-validated
//! redirects, byte cap, binary detection — is
//! [`entanglement_core::web_fetch::WebFetcher`] (provider-owned, ADR-0053);
//! this module only parses arguments, converts HTML, and pages the result.
//!
//! Capability is [`Capability::Exec`], not `Read`: the vocabulary's `Read` is
//! "reads the host", which `research`/`plan`/`auto` allow unprompted, while
//! an outbound request to a model-chosen URL is an outward effect (a query
//! string can carry data out). `Exec` is what every other outbound-network
//! tool already declares (`endpoint__*`, `call`), so the built-in modes grade
//! `web_fetch` exactly like `curl` with no new rule entries: `build` allows
//! it, every other mode prompts. A user who wants unprompted docs lookups
//! adds an argument-scoped rule such as `web_fetch(https://docs.rs/*)`.

use std::borrow::Cow;
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;

use entanglement_core::web_fetch::{FetchBody, FetchOutcome, WebFetcher, DEFAULT_MAX_BYTES};

use crate::capability::Capability;
use crate::host::MAX_OUTPUT_BYTES;
use crate::tools::Tool;

pub const WEB_FETCH_TOOL: &str = "web_fetch";

pub struct WebFetchTool {
    fetcher: WebFetcher,
}

impl WebFetchTool {
    pub fn new() -> Result<Self> {
        Ok(Self {
            fetcher: WebFetcher::new()?,
        })
    }
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
enum Format {
    #[default]
    Markdown,
    Text,
    Raw,
}

#[derive(Deserialize)]
struct Input {
    url: String,
    #[serde(default)]
    format: Format,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(WEB_FETCH_TOOL)
    }

    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::Exec]
    }

    fn description(&self) -> &str {
        "Fetch a public http(s) URL and return its content as readable text. \
         HTML is converted to Markdown by default; binary content (images, PDFs, \
         archives) is reported, never inlined. Private, loopback and link-local \
         addresses are refused. Long pages come back in pages of 32 KiB — call \
         again with the `offset` the result names to read on."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {
                    "type": "string",
                    "description": "The http:// or https:// URL to fetch."
                },
                "format": {
                    "type": "string",
                    "enum": ["markdown", "text", "raw"],
                    "description": "markdown (default): HTML converted to Markdown; text: the same without link/image URLs; raw: the body exactly as served."
                },
                "max_bytes": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Download cap in bytes (default 2 MiB, at most 10 MiB)."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 0,
                    "description": "Byte offset into the converted content to start the returned page at (default 0)."
                }
            },
            "required": ["url"]
        })
    }

    async fn run(&self, input: &str) -> Result<String> {
        let input: Input = serde_json::from_str(input).context("invalid web_fetch arguments")?;
        let outcome = self
            .fetcher
            .fetch(&input.url, input.max_bytes.unwrap_or(DEFAULT_MAX_BYTES))
            .await?;
        render(outcome, input.format, input.offset.unwrap_or(0))
    }
}

/// Render a fetch outcome as the tool result: a short header, then one page
/// of the converted content.
fn render(outcome: FetchOutcome, format: Format, offset: usize) -> Result<String> {
    let mime = outcome.mime.as_deref().unwrap_or("unknown");
    let (text, truncated) = match outcome.body {
        FetchBody::Binary { size } => {
            let size = size.map_or_else(|| "unknown size".to_string(), |n| format!("{n} bytes"));
            return Ok(format!(
                "[binary omitted: {mime}, {size}, {}] (HTTP {})",
                outcome.final_url, outcome.status
            ));
        }
        FetchBody::Text { text, truncated } => (text, truncated),
    };
    let is_html = matches!(mime, "text/html" | "application/xhtml+xml");
    let content = match format {
        Format::Raw => text,
        Format::Markdown if is_html => html_to_markdown(&text)?,
        Format::Text if is_html => strip_link_targets(&html_to_markdown(&text)?),
        Format::Markdown | Format::Text => text,
    };
    let mut header = format!(
        "URL: {}\nStatus: {}\nContent-Type: {mime}\n",
        outcome.final_url, outcome.status
    );
    if truncated {
        header.push_str("[download stopped at the byte cap — raise max_bytes for more]\n");
    }
    header.push('\n');
    header.push_str(&page(&content, offset)?);
    Ok(header)
}

/// One [`MAX_OUTPUT_BYTES`] page of `content` starting at byte `offset`
/// (snapped to a char boundary), with a pointer to the next page — the same
/// context budget every other host tool's output gets (ADR-0008).
fn page(content: &str, offset: usize) -> Result<String> {
    if offset > content.len() {
        bail!(
            "offset {offset} is past the end of the content ({} bytes)",
            content.len()
        );
    }
    let start = floor_boundary(content, offset);
    let end = floor_boundary(content, start.saturating_add(MAX_OUTPUT_BYTES));
    let mut out = content[start..end].to_string();
    if end < content.len() {
        out.push_str(&format!(
            "\n\n[page {start}..{end} of {} bytes — call again with offset={end} for more]",
            content.len()
        ));
    }
    Ok(out)
}

fn floor_boundary(s: &str, idx: usize) -> usize {
    let mut idx = idx.min(s.len());
    while !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn html_to_markdown(html: &str) -> Result<String> {
    htmd::HtmlToMarkdown::builder()
        // Page chrome and code that carries no reading value but would eat
        // the context budget.
        .skip_tags(vec![
            "script", "style", "noscript", "svg", "head", "iframe", "template",
        ])
        .build()
        .convert(html)
        .context("converting HTML to Markdown")
}

/// Drop link and image targets from Markdown: `[text](url)` → `text`,
/// `![alt](url)` → `alt`. `text` format's whole point — prose without the
/// URL noise that can double a page's token count.
fn strip_link_targets(markdown: &str) -> String {
    static LINK: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"!?\[([^\]]*)\]\([^)]*\)").expect("invariant: static regex compiles")
    });
    LINK.replace_all(markdown, "$1").into_owned()
}

#[cfg(test)]
mod tests;
