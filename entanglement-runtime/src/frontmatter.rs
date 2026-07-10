//! Shared `---`-delimited YAML frontmatter splitter.
//!
//! Both file-based agent definitions (#112) and skill definitions (#114) are a
//! markdown file that opens with a `---` line, carries a YAML frontmatter block,
//! a closing `---`, then a markdown body. This is the one place that split
//! lives.

use anyhow::{bail, Result};

/// Split a `---`-delimited YAML frontmatter block from the body below it.
/// Requires the file to open with a `---` line and to carry a closing `---`.
/// Returns `(frontmatter, body)` — the body keeps no trailing/leading blank
/// normalization beyond a plain line join (callers trim as needed).
pub fn split(content: &str) -> Result<(String, String)> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        bail!("missing YAML frontmatter: the file must start with a `---` line");
    }
    let mut frontmatter = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            closed = true;
            break;
        }
        frontmatter.push_str(line);
        frontmatter.push('\n');
    }
    if !closed {
        bail!("unterminated frontmatter: missing the closing `---` line");
    }
    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((frontmatter, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frontmatter_from_body() {
        let (fm, body) = split("---\nname: x\n---\nhello\nworld").unwrap();
        assert_eq!(fm, "name: x\n");
        assert_eq!(body, "hello\nworld");
    }

    #[test]
    fn missing_opening_fence_is_an_error() {
        let err = split("no frontmatter here").unwrap_err();
        assert!(err.to_string().contains("frontmatter"), "got: {err}");
    }

    #[test]
    fn unterminated_frontmatter_is_an_error() {
        let err = split("---\nname: x\n").unwrap_err();
        assert!(err.to_string().contains("unterminated"), "got: {err}");
    }
}
