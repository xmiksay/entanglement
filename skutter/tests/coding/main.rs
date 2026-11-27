//! The coding layer moved out of the library (#932): the on-disk session
//! store and its resume checks, file-based agent and skill definitions, the
//! per-write backups and the aux-model tasks, over the real engine. One
//! binary (ADR-0180); run one module via `cargo test -p skutter --test coding
//! <mod>`.

mod narrate;
mod session_title;
