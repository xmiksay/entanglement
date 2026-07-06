# Header 1

## Header 2

### Sub-Header 3

This is **bold text**. This is *emphasized text*. And `inline code`. We can combine them like this: ***all at once***.

---

Here's a list of features from the rich-text pipeline (ADR 0015):

- **Bold emphasis** for important items
- *Italic* highlighting  
- `` inline code examples`` with light background styling
- `Fenced code blocks` that delegate to syntect highlighting
- Diff rendering: + green lines, - red lines
- Terminal width adapts via diff_style: auto

---

```rust {theme=base16-ocean.dark}
// Example Rust code block
// Tokenized with syntect::easy::HighlightLines
fn main() -> Result<()> {
    println!("Hello!");
}
```

Note the **syntax highlighting** and terminal adaptation to width.

> This is a [blockquote](https://example.com) — you can also include links like [`pulldown-cmark`](https://crates.io/crates/pulldown-cmark).

---

## Consequences (from ADR 0015):

- **(+)** Rich rendering matches user expectations
- **(−)** `syntect` adds binary size  
- **Pure Rust**, no C deps!

---

*This document demonstrates the markdown capabilities of our TUI.*
