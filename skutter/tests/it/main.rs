//! `skutter`'s integration harness (one binary, ADR-0180): suites that spawn
//! the `skutter` binary, and the `serve` head driven over a real socket. Run
//! one module via `cargo test -p skutter --test it <mod>`.

// The engine fixture the `serve` suites share.
mod support;

// A `skutter` child isolated from the developer's real dirs (#747).
mod skutter_cmd;

mod attended_serve;
mod attended_wake;
mod attended_wake_queue;
mod attended_wake_tree;
// `skutter`'s lifecycle hooks (#199/#734) over the real engine (#964).
mod hook_events;
mod hooks;
mod launch_mode_typo;
mod provider_selection;
mod serve;
mod slash_commands;
mod startup_warnings;
mod user_config;
