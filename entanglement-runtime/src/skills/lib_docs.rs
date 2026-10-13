//! Doc comments of a `lib.rhai`'s functions, read with a small text pass
//! (ADR-0228). Rhai's own doc-comment capture needs its `metadata` feature,
//! which is crate-wide and makes the parser *reject* any `///` or `/** */`
//! not directly followed by a `fn` — breaking ordinary scripts with a header
//! or stray doc-style comment. The listing only needs the comment run above
//! each `fn`, which plain text gives.

/// One function's doc comment: name, parameter count when the signature
/// fits on its line (to tell overloads apart), and the text.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct FnDoc {
    pub name: String,
    pub arity: Option<usize>,
    pub doc: String,
}

/// Every public `fn` in `source` that carries a doc comment — the
/// contiguous `///` lines or the one `/** … */` block directly above it.
pub(super) fn fn_docs(source: &str) -> Vec<FnDoc> {
    let lines: Vec<&str> = source.lines().map(str::trim).collect();
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.strip_prefix("fn ") else {
            continue; // `private fn …` is not listed, other lines are no fn
        };
        let Some((name, params)) = rest.split_once('(') else {
            continue;
        };
        let doc = doc_above(&lines[..i]);
        if doc.is_empty() {
            continue;
        }
        let arity = params
            .split_once(')')
            .map(|(p, _)| p.split(',').filter(|s| !s.trim().is_empty()).count());
        out.push(FnDoc {
            name: name.trim().to_string(),
            arity,
            doc,
        });
    }
    out
}

/// The doc text of the comment run ending right above `above`'s last line.
fn doc_above(above: &[&str]) -> String {
    let mut doc: Vec<String> = Vec::new();
    let mut end = above.len();
    while end > 0 && above[end - 1].starts_with("///") {
        end -= 1;
    }
    if end < above.len() {
        doc.extend(above[end..].iter().map(|l| margin(&l[3..])));
    } else if above.last().is_some_and(|l| l.ends_with("*/")) {
        let Some(start) = above.iter().rposition(|l| l.starts_with("/*")) else {
            return String::new();
        };
        let block = above[start..].join("\n");
        // `/**/` and `/*** … */` are not doc comments (Rust's rule, Rhai's too).
        let Some(body) = block
            .strip_prefix("/**")
            .filter(|b| !b.starts_with('*') && !b.starts_with('/'))
        else {
            return String::new();
        };
        let body = body.strip_suffix("*/").unwrap_or(body);
        doc.extend(body.lines().map(|l| {
            let l = l.trim();
            // A block's `*` margin, never a `*` that is part of the text.
            let l = match l.strip_prefix('*') {
                Some(r) if r.is_empty() || r.starts_with(' ') => r,
                _ => l,
            };
            l.trim().to_string()
        }));
    }
    while doc.first().is_some_and(String::is_empty) {
        doc.remove(0);
    }
    while doc.last().is_some_and(String::is_empty) {
        doc.pop();
    }
    doc.join("\n")
}

/// A `///` line's text without the single conventional space after it.
fn margin(text: &str) -> String {
    text.strip_prefix(' ')
        .unwrap_or(text)
        .trim_end()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(source: &str, name: &str) -> Option<String> {
        fn_docs(source)
            .into_iter()
            .find(|d| d.name == name)
            .map(|d| d.doc)
    }

    #[test]
    fn line_and_block_doc_comments_are_read() {
        let src = "/// Count lines.\n/// Returns an int.\nfn count(text) { 1 }\n\
                   /**\n * Greet someone.\n * Politely.\n */\nfn greet(name) { name }\n\
                   /** One-liner. */\nfn one() { 1 }";
        assert_eq!(doc(src, "count").unwrap(), "Count lines.\nReturns an int.");
        assert_eq!(doc(src, "greet").unwrap(), "Greet someone.\nPolitely.");
        assert_eq!(doc(src, "one").unwrap(), "One-liner.");
    }

    #[test]
    fn plain_comments_gaps_private_fns_and_file_headers_carry_no_doc() {
        let src = "/* file header */\n\n// plain\nfn plain() { 1 }\n\
                   /// detached\n\nfn gap() { 1 }\n\
                   /// hidden\nprivate fn secret() { 1 }\n\
                   /*** banner ***/\nfn banner() { 1 }";
        assert!(fn_docs(src).is_empty(), "{:?}", fn_docs(src));
    }

    /// The old pass stripped any leading `*`: text like `**bold**` or a
    /// `/// *` bullet must survive.
    #[test]
    fn only_the_block_margin_star_is_stripped() {
        let src = "/**\n * **bold** first\n *bare\n */\nfn a() { 1 }\n/// * bullet\nfn b() { 1 }";
        assert_eq!(doc(src, "a").unwrap(), "**bold** first\n*bare");
        assert_eq!(doc(src, "b").unwrap(), "* bullet");
    }

    #[test]
    fn arity_tells_overloads_apart() {
        let src = "/// one\nfn f(a) { a }\n/// two\nfn f(a, b) { a }\n/// zero\nfn g() { 1 }";
        let docs = fn_docs(src);
        assert_eq!(docs[0].arity, Some(1));
        assert_eq!(docs[1].arity, Some(2));
        assert_eq!(docs[2].arity, Some(0));
    }
}
