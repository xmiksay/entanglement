//! HTML → Markdown for `web_fetch`, hardened against hostile pages.
//!
//! `htmd` walks the DOM recursively with no depth limit: a page of a few
//! hundred KB of nested elements overflows the stack and **aborts the whole
//! process** (TUI, `serve`). Building a deep tree is itself expensive:
//! html5ever's tree builder checks element scope by walking its stack of
//! open elements, so N nested elements cost O(N²) — 110k of them (1.9 MB)
//! is minutes of CPU. A hand-rolled lexical scan can't be trusted to see the
//! nesting (a `>` inside a quoted attribute fools one), so the guard uses
//! html5ever's own spec tokenizer, then the real tree:
//!
//! 1. [`token_scan`] runs the real tokenizer (linear, no tree) and tracks
//!    nesting with an open-element stack, collecting the page's text as it
//!    goes. Above [`MAX_DEPTH`] the page is answered with that text and
//!    never reaches the tree builder.
//! 2. Otherwise html5ever builds the rcdom (`htmd`'s own parser) and
//!    [`max_depth`] measures the **real tree** iteratively — the safety net
//!    for any nesting the token-level stack model misses (implicit elements,
//!    reconstructed formatting). Above the limit, `htmd` never sees it and
//!    [`tree_text`] extracts text iteratively. rcdom's own `Drop` is
//!    iterative, so tearing a deep tree down is safe too.
//! 3. All of that runs on a dedicated thread with a generous stack
//!    ([`CONVERT_STACK`]), never on a tokio worker.
//! 4. At most [`MAX_CONVERSIONS`] such threads exist at once. The permit
//!    moves into the thread and is released only when the thread *finishes*
//!    — so a page that times out (its thread can't be killed) still holds
//!    its slot, and pathological pages can't pile up threads. Waiting for a
//!    slot shares the [`CONVERT_TIMEOUT`] budget; on timeout the result is
//!    the [`token_scan`] text, computed on the caller (linear, no
//!    recursion, no tree).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use htmd::Node;
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer,
};
use markup5ever_rcdom::NodeData;
use tokio::sync::Semaphore;

/// Tree depth above which `htmd` is not attempted. Real pages sit well
/// under 100.
pub(super) const MAX_DEPTH: usize = 256;
/// Covers several times [`MAX_DEPTH`] of `htmd` recursion (a test pins 4×).
const CONVERT_STACK: usize = 64 * 1024 * 1024;
const CONVERT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONVERSIONS: usize = 2;

static CONVERSIONS: LazyLock<Arc<Semaphore>> =
    LazyLock::new(|| Arc::new(Semaphore::new(MAX_CONVERSIONS)));

/// Element bodies that carry no reading value (mirrors `htmd`'s skip list).
const SKIPPED: [&str; 7] = [
    "script", "style", "noscript", "svg", "head", "iframe", "template",
];
/// Elements that never take children.
const VOID: [&str; 15] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "source", "track",
    "wbr", "param", "keygen",
];
/// Elements rendered as a line break in plain text.
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
    let deadline = tokio::time::Instant::now() + CONVERT_TIMEOUT;
    let fallback = match run_converter(html.clone(), deadline).await {
        Ok(converted) => return converted,
        Err(why) => why,
    };
    Converted {
        text: token_scan(&html).text,
        fallback: Some(fallback),
    }
}

async fn run_converter(
    html: String,
    deadline: tokio::time::Instant,
) -> Result<Converted, &'static str> {
    let permit = tokio::time::timeout_at(deadline, CONVERSIONS.clone().acquire_owned())
        .await
        .map_err(|_| "HTML converter busy")?
        .map_err(|_| "HTML converter unavailable")?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("web_fetch-html".into())
        .stack_size(CONVERT_STACK)
        .spawn(move || {
            let _ = tx.send(convert(&html));
            // Released only now: an abandoned (timed-out) conversion keeps
            // its slot until it has really stopped burning CPU.
            drop(permit);
        })
        .map_err(|_| "could not start the HTML converter")?;
    match tokio::time::timeout_at(deadline, rx).await {
        Ok(Ok(converted)) => Ok(converted),
        Ok(Err(_)) => Err("HTML conversion failed"),
        Err(_) => Err("HTML conversion timed out"),
    }
}

/// Parse once, measure the real tree, and only hand a shallow one to
/// `htmd`. Call only on a thread with [`CONVERT_STACK`].
fn convert(html: &str) -> Converted {
    let scan = token_scan(html);
    if scan.depth > MAX_DEPTH {
        return Converted {
            text: scan.text,
            fallback: Some("HTML nested too deeply to convert"),
        };
    }
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(SKIPPED.to_vec())
        .build();
    let Ok(tree) = converter.html_to_tree(html) else {
        return Converted {
            text: scan.text,
            fallback: Some("HTML conversion failed"),
        };
    };
    if max_depth(&tree) > MAX_DEPTH {
        return Converted {
            text: tree_text(&tree),
            fallback: Some("HTML nested too deeply to convert"),
        };
    }
    Converted {
        text: converter.tree_to_markdown(&tree),
        fallback: None,
    }
}

fn element_name(node: &Node) -> Option<&str> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.as_ref()),
        _ => None,
    }
}

/// Deepest element nesting in the tree, walked with an explicit stack.
pub(super) fn max_depth(root: &Rc<Node>) -> usize {
    let mut stack = vec![(root.clone(), 0usize)];
    let mut max = 0;
    while let Some((node, depth)) = stack.pop() {
        max = max.max(depth);
        let next = depth + usize::from(element_name(&node).is_some());
        for child in node.children.borrow().iter() {
            stack.push((child.clone(), next));
        }
    }
    max
}

/// Plain text of the tree — skipped elements dropped, block elements as
/// line breaks — walked with an explicit stack, so any depth is safe.
pub(super) fn tree_text(root: &Rc<Node>) -> String {
    enum Step {
        Visit(Rc<Node>),
        Break,
    }
    let mut out = String::new();
    let mut stack = vec![Step::Visit(root.clone())];
    while let Some(step) = stack.pop() {
        let node = match step {
            Step::Break => {
                out.push('\n');
                continue;
            }
            Step::Visit(node) => node,
        };
        if let NodeData::Text { contents } = &node.data {
            out.push_str(&contents.borrow());
            continue;
        }
        let name = element_name(&node);
        if name.is_some_and(|n| SKIPPED.contains(&n)) {
            continue;
        }
        let block = name.is_some_and(|n| BLOCK.contains(&n));
        if block {
            out.push('\n');
            stack.push(Step::Break);
        }
        for child in node.children.borrow().iter().rev() {
            stack.push(Step::Visit(child.clone()));
        }
    }
    collapse_whitespace(&out)
}

/// What [`token_scan`] learned: the estimated nesting depth and the page's
/// readable text.
pub(super) struct Scan {
    pub depth: usize,
    pub text: String,
}

/// Token sink: an open-element stack (an end tag pops back to its matching
/// open element and is otherwise ignored, so stray end tags can't hide
/// nesting) plus the text outside skipped elements.
#[derive(Default)]
struct ScanSink {
    stack: RefCell<Vec<String>>,
    depth: Cell<usize>,
    text: RefCell<String>,
}

impl ScanSink {
    fn tag(&self, tag: &Tag) -> TokenSinkResult<()> {
        let name: &str = tag.name.as_ref();
        if BLOCK.contains(&name) {
            self.text.borrow_mut().push('\n');
        } else {
            self.text.borrow_mut().push(' ');
        }
        let mut stack = self.stack.borrow_mut();
        if tag.kind == TagKind::EndTag {
            if let Some(i) = stack.iter().rposition(|n| n == name) {
                stack.truncate(i);
            }
            return TokenSinkResult::Continue;
        }
        if VOID.contains(&name) {
            return TokenSinkResult::Continue;
        }
        stack.push(name.to_string());
        self.depth.set(self.depth.get().max(stack.len()));
        // The tokenizer alone doesn't know which elements hold raw text;
        // the tree builder normally tells it, so the sink must.
        match name {
            "script" => TokenSinkResult::RawData(RawKind::ScriptData),
            "style" | "xmp" | "iframe" | "noembed" | "noframes" | "noscript" => {
                TokenSinkResult::RawData(RawKind::Rawtext)
            }
            "title" | "textarea" => TokenSinkResult::RawData(RawKind::Rcdata),
            "plaintext" => TokenSinkResult::Plaintext,
            _ => TokenSinkResult::Continue,
        }
    }
}

impl TokenSink for ScanSink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(tag) => return self.tag(&tag),
            Token::CharacterTokens(chars) => {
                let skipped = self
                    .stack
                    .borrow()
                    .last()
                    .is_some_and(|top| SKIPPED.contains(&top.as_str()) || top == "title");
                if !skipped {
                    self.text.borrow_mut().push_str(&chars);
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

/// Run html5ever's spec tokenizer over `html` — linear, no tree — for the
/// nesting estimate and the plain text. Safe on any input.
pub(super) fn token_scan(html: &str) -> Scan {
    let input = BufferQueue::default();
    input.push_back(StrTendril::from_slice(html));
    let tokenizer = Tokenizer::new(ScanSink::default(), Default::default());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let sink = tokenizer.sink;
    Scan {
        depth: sink.depth.get(),
        text: collapse_whitespace(&sink.text.into_inner()),
    }
}

fn collapse_whitespace(s: &str) -> String {
    let mut text = String::with_capacity(s.len());
    for line in s.lines() {
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
