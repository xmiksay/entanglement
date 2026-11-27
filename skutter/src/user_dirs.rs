//! The user's real home/data directories — the one place skutter code
//! resolves them (#773, the runtime's `user_dirs` for this crate).
//!
//! Under `cfg(test)` every accessor panics, so a unit test cannot read or
//! write the developer's own files; it injects the path instead. The panic
//! names the call site via `#[track_caller]`. Integration tests compile
//! without `cfg(test)`; `scripts/test-isolated.sh` sandboxes them instead.
//!
//! Both the library and the binary declare this module: each crate root has
//! its own `cfg(test)`, so a module shared through the library would not
//! guard the binary's unit tests.

// Each crate root uses a different subset of the accessors.
#![allow(dead_code)]

use std::path::PathBuf;

/// `dirs::home_dir()`, guarded under `cfg(test)`.
#[track_caller]
pub(crate) fn home_dir() -> Option<PathBuf> {
    guard("home");
    dirs::home_dir()
}

/// `dirs::data_dir()`, guarded under `cfg(test)`.
#[track_caller]
pub(crate) fn data_dir() -> Option<PathBuf> {
    guard("data");
    dirs::data_dir()
}

#[track_caller]
fn guard(which: &str) {
    if cfg!(test) {
        panic!(
            "unit test resolved the real user {which} dir at {}; inject the path \
             through the component's seam instead (#773)",
            std::panic::Location::caller()
        );
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic(expected = "resolved the real user home dir")]
    fn home_dir_is_unreachable_under_test() {
        super::home_dir();
    }

    #[test]
    #[should_panic(expected = "resolved the real user data dir")]
    fn data_dir_is_unreachable_under_test() {
        super::data_dir();
    }
}
