# 0015. Rich-text pipeline: pulldown-cmark → ratatui Text, syntect for code blocks

- Status: Accepted
- Date: 2026-07-05

## Context

The TUI must render markdown content from the model (`TextDelta`, `Plan`). A naive approach would show raw markdown, but opencode renders formatted text, bold, code blocks with syntax highlighting, and diffs for `edit` outputs. We need a pipeline that parses markdown and converts it to ratatui's `Text` widget.

Components needed:

- **Markdown parser:** Convert CommonMark to an AST.
- **Text renderer:** Walk the AST and build `ratatui::text::Text` with appropriate styles (bold, italic, underline, etc.).
- **Syntax highlighter:** For fenced code blocks, tokenize by language and assign colors.
- **Diff renderer:** For `edit` tool outputs, show a unified or stacked diff with `+`/`-` coloring.

Ecosystem options:

- **pulldown-cmark:** A fast, CommonMark-compliant parser. Widely used.
- **syntect:** A syntax highlighter with Sublime Text Textmate grammar support. Already in deps with `default-fancy`.
- **ratatui:** The UI framework; `Text` is the widget for styled text.

Alternatives considered for highlighting:

- **syntect:** Chosen. It's pure Rust and supports many languages out of the box.
- **tree-sitter:** Heavier, requires language-specific parsers. Overkill for a TUI.
- **bat's highlighting (bat::PrettyPrinter):** Good but integrates less cleanly with ratatui's streaming render.

## Decision

Pipeline:

1. **Parse markdown:** `pulldown_cmark::Parser::new(input).collect::<Vec<Event>>()`
2. **Iterate events:** Map markdown events to `ratatui::text::Span`/`Line`/`Text`. Styles:
   - `Emphasis` → italic
   - `Strong` → bold
   - `Code(inline)` → a light background style
   - `Fenced(code)` → delegate to `syntect`
3. **Code highlighting:** For a fenced block, tokenize with `syntect::easy::HighlightLines`, then map scopes to colors. Use a small set of themes (e.g., `base16-ocean.dark`) to keep binary size bounded.
4. **Diff rendering:** For `edit` outputs, parse the `ToolOutput` as a diff (unified format) and render `+` lines in green, `-` lines in red.

Adapt to terminal width (`diff_style: auto` from opencode). If the terminal is narrow, use a stacked diff (before/after columns). Otherwise, unified diff.

## Consequences

- **(+)** Rich rendering matches user expectations (markdown, code, diffs).
- **(+)** All crates already in deps (no new workspace deps for this head).
- **(+)** Pure Rust, no C deps.
- **(−)** `syntect` adds binary size (grammar packs). Mitigation: limit included languages or lazy-load (future).
- **(−)** Markdown rendering is CPU-intensive. Mitigation: cache rendered `Text` for events that don't change (`Plan` is a snapshot; render once and reuse until it updates).

## Alternatives considered

- **tree-sitter:** Rejected. It's powerful but overkill and requires language-specific parsers, increasing binary size.
- **bat's highlighting:** Rejected. It's designed for printing to stdout, not building `ratatui::Text`. Integration would be messy.
- **No highlighting, just show plain code blocks:** Rejected because code readability drops sharply. Users expect highlighting.
- **No diff rendering:** Rejected. Diff context is crucial for understanding `edit` operations.