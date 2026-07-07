# 0011. TUI head: ratatui + crossterm in `entanglement-runtime`

- Status: Accepted
- Date: 2026-07-05

## Context

The TUI head (`skutter tui`) needs a Rust terminal UI framework. Options:

- **ratatui + crossterm**: The ecosystem's de facto standard. `ratatui` builds widgets and layouts over a backend; `crossterm` provides cross-platform terminal control. Both are pure-Rust and widely used.
- **termion**: A lower-level alternative to crossterm. Less actively maintained; crossterm has broader terminal compatibility.
- **ncurses-rs**: Bindings to the C ncurses library. Better cross-platform historical support but adds a C dependency and doesn't integrate with Rust's async model as cleanly.
- **tui-rs**: The predecessor to ratatui (now archived). Ratatui is the maintained fork.

**Hygiene constraint (ADR-0006):** `entanglement-core` must stay free of UI/transport deps. The TUI deps must live in a head crate.

## Decision

Use **ratatui 0.29 + crossterm 0.28**, both already declared in `Cargo.toml` workspace dependencies. The TUI lives entirely in `entanglement-runtime`, the single head crate (ADR-0010). `entanglement-core` imports none of these crates; `make tree` enforces this.

Event loop pattern: spawn a tokio task that polls crossterm key events and forwards them over an `mpsc<Event>`; the main loop `select!`s over that channel and `holly.subscribe()`. Mutate an `App` state struct and draw when dirty via `ratatui::Terminal::draw()`.

## Consequences

- **(+)** Well-tested, active ecosystem. `tui-textarea` (for multiline input) is built on this stack.
- **(+)** Pure Rust, no C deps. Works on Windows/Linux/macOS.
- **(+)** Already pinned in workspace deps (lines 34-39). No new dependency for the TUI head.
- **(+)** Hygiene gate passes: none of `ratatui|crossterm|tui-textarea|pulldown-cmark|syntect` appear in `entanglement-core`'s dependency tree (`make tree`).
- **(−)** Learning curve for ratatui's widget/layout model, but the patterns are stable.
- **(−)** crossterm requires raw mode and alternate screen management; we must restore terminal state on panic/exit.

## Alternatives considered

- **termion**: Rejected due to lower activity and narrower terminal support.
- **ncurses-rs**: Rejected because adding a C dependency complicates builds and the async integration is clunkier.
- **Put ratatui in entanglement-core**: Rejected by ADR-0006. The head crate is the right place (ADR-0010).