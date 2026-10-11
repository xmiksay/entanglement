//! HTML → Markdown for `web_fetch`, hardened against hostile pages.
//!
//! `htmd` walks the DOM recursively with no depth limit, synchronously: a
//! ~100 KB page of `<div>` × 20 000 overflows the stack and **aborts the
//! whole process** (TUI, `serve`), and a few hundred thousand levels burn a
//! minute of CPU. So conversion is guarded three ways:
//!
//! 1. A cheap linear tag scan ([`nesting_depth`]) estimates DOM depth first;
//!    above [`MAX_DEPTH`] the page skips `htmd` entirely and gets
//!    [`plain_text`] extraction instead (a flat loop, no recursion).
//! 2. Conversion runs on its own thread with a generous stack
//!    ([`CONVERT_STACK`]), never on a tokio worker — the scan is an estimate,
//!    so the stack leaves room for the DOM to come out deeper than counted.
//! 3. A [`CONVERT_TIMEOUT`] bounds the wait; on timeout (or any failure) the
//!    result is again [`plain_text`]. The thread can't be killed, but with the
//!    depth gate in front it is bounded work that finishes on its own.

use std::time::Duration;

/// Estimated nesting depth above which `htmd` is not attempted. Real pages
/// sit well under 100.
pub(super) const MAX_DEPTH: usize = 256;
/// Stack for the conversion thread: covers several times [`MAX_DEPTH`] of
/// `htmd` recursion (pinned by a test at 4×), so an underestimate of the
/// DOM's real depth still can't overflow.
const CONVERT_STACK: usize = 64 * 1024 * 1024;
const CONVERT_TIMEOUT: Duration = Duration::from_secs(5);

/// Elements whose content is raw text, never markup.
const RAW_TEXT: [&str; 8] = [
    "script", "style", "textarea", "title", "xmp", "iframe", "noembed", "noframes",
];
/// Elements that never take children.
const VOID: [&str; 15] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr", "param", "keygen",
];
/// Tags rendered as a line break in [`plain_text`].
const BLOCK: [&str; 14] = [
    "br",
    "p",
    "div",
    "li",
    "tr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "section",
    "article",
    "blockquote",
];

/// The converted page, and why it fell back to plain text, if it did.
pub(super) struct Converted {
    pub text: String,
    pub fallback: Option<&'static str>,
}

pub(super) async fn to_markdown(html: String) -> Converted {
    if nesting_depth(&html) > MAX_DEPTH {
        return Converted {
            text: plain_text(&html),
            fallback: Some("HTML nested too deeply to convert"),
        };
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    let input = html.clone();
    let spawned = std::thread::Builder::new()
        .name("web_fetch-html".into())
        .stack_size(CONVERT_STACK)
        .spawn(move || {
            let _ = tx.send(convert(&input));
        });
    let fallback = match spawned {
        Err(_) => "could not start the HTML converter",
        Ok(_) => match tokio::time::timeout(CONVERT_TIMEOUT, rx).await {
            Ok(Ok(Some(markdown))) => {
                return Converted {
                    text: markdown,
                    fallback: None,
                }
            }
            Ok(_) => "HTML conversion failed",
            Err(_) => "HTML conversion timed out",
        },
    };
    Converted {
        text: plain_text(&html),
        fallback: Some(fallback),
    }
}

/// The `htmd` conversion itself — call only on a thread with
/// [`CONVERT_STACK`].
fn convert(html: &str) -> Option<String> {
    htmd::HtmlToMarkdown::builder()
        // Page chrome and code that carries no reading value but would eat
        // the context budget.
        .skip_tags(vec![
            "script", "style", "noscript", "svg", "head", "iframe", "template",
        ])
        .build()
        .convert(html)
        .ok()
}

enum Token<'a> {
    Text(&'a str),
    Open(String),
    Close(String),
}

/// A forgiving, linear HTML tokenizer: comments/doctypes skipped, raw-text
/// element content skipped (an `Open` with no body), a `<` that never
/// closes kept as text. Precision isn't the goal — both consumers only need
/// a safe approximation.
fn tokens(html: &str) -> impl Iterator<Item = Token<'_>> {
    let mut rest = html;
    std::iter::from_fn(move || loop {
        if rest.is_empty() {
            return None;
        }
        let Some(lt) = rest.find('<') else {
            let text = rest;
            rest = "";
            return Some(Token::Text(text));
        };
        if lt > 0 {
            let text = &rest[..lt];
            rest = &rest[lt..];
            return Some(Token::Text(text));
        }
        let after = &rest[1..];
        if let Some(comment) = after.strip_prefix("!--") {
            rest = comment.find("-->").map_or("", |i| &comment[i + 3..]);
            continue;
        }
        let Some(gt) = after.find('>') else {
            // A `<` that never closes is just text (`a < b`).
            let text = rest;
            rest = "";
            return Some(Token::Text(text));
        };
        let tag = &after[..gt];
        rest = &after[gt + 1..];
        let (closing, body) = match tag.strip_prefix('/') {
            Some(b) => (true, b),
            None => (false, tag),
        };
        let name: String = body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect::<String>()
            .to_ascii_lowercase();
        if name.is_empty() || !body.starts_with(|c: char| c.is_ascii_alphabetic()) {
            continue; // `<!doctype>`, `<?xml>`, a stray `<`
        }
        if closing {
            return Some(Token::Close(name));
        }
        if RAW_TEXT.contains(&name.as_str()) {
            let end = format!("</{name}");
            rest = find_ascii_ci(rest, &end).map_or("", |i| &rest[i..]);
        }
        return Some(Token::Open(name));
    })
}

/// Byte index of `needle` (ASCII) in `hay`, case-insensitively.
fn find_ascii_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// Estimated maximum element nesting. An end tag pops back to its matching
/// open element and is otherwise ignored — so `</span>` junk can't hide
/// unclosed `<div>`s — and implicit closes (`<p>`, `<li>`) are not modelled,
/// which only over-counts. Overestimating is the safe direction: it sends a
/// page to plain text, never to a stack overflow.
pub(super) fn nesting_depth(html: &str) -> usize {
    let mut stack: Vec<String> = Vec::new();
    let mut max = 0;
    for token in tokens(html) {
        match token {
            Token::Open(name) if !VOID.contains(&name.as_str()) => {
                stack.push(name);
                max = max.max(stack.len());
            }
            Token::Close(name) => {
                if let Some(i) = stack.iter().rposition(|n| *n == name) {
                    stack.truncate(i);
                }
            }
            _ => {}
        }
    }
    max
}

/// Text content with tags removed, script/style bodies dropped, block tags
/// as line breaks, the common entities decoded, and whitespace collapsed.
/// A flat loop — safe on any input.
pub(super) fn plain_text(html: &str) -> String {
    let mut out = String::new();
    for token in tokens(html) {
        match token {
            Token::Text(t) => out.push_str(t),
            Token::Open(n) | Token::Close(n) if BLOCK.contains(&n.as_str()) => out.push('\n'),
            _ => out.push(' '),
        }
    }
    let decoded = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    let mut text = String::with_capacity(decoded.len());
    for line in decoded.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

#[cfg(test)]
mod tests;
