//! The user's real home/config/data directories — the one place library code
//! in this crate resolves them (#773).
//!
//! Under `cfg(test)` every accessor panics: a unit test resolving a real user
//! dir would read the developer's own `config.yml`, agents, skills, `.env` or
//! managed files (making the test's outcome depend on whoever runs it) and
//! could write to them. Each caller has an injectable seam instead — the panic
//! names the call site via `#[track_caller]`, so a new offender is easy to
//! trace. Integration tests compile this crate without `cfg(test)`; they are
//! HOME/XDG-sandboxed by `scripts/test-isolated.sh` instead.

use std::path::PathBuf;

/// `dirs::home_dir()`, guarded under `cfg(test)`.
#[track_caller]
pub(crate) fn home_dir() -> Option<PathBuf> {
    guard("home");
    dirs::home_dir()
}

/// `dirs::config_dir()`, guarded under `cfg(test)`.
#[track_caller]
pub(crate) fn config_dir() -> Option<PathBuf> {
    guard("config");
    dirs::config_dir()
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
    #[should_panic(expected = "resolved the real user config dir")]
    fn config_dir_is_unreachable_under_test() {
        super::config_dir();
    }

    #[test]
    #[should_panic(expected = "resolved the real user data dir")]
    fn data_dir_is_unreachable_under_test() {
        super::data_dir();
    }
}
