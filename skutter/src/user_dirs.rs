//! The user's real home/config/data directories — the one place skutter
//! resolves them (#773; the library resolves none, #967) — and what skutter
//! derives from them: each definition kind's user layer and the per-project
//! session store.
//!
//! Under `cfg(test)` every accessor panics, so a unit test cannot read or
//! write the developer's own files; it injects the path instead. The panic
//! names the call site via `#[track_caller]`. Integration tests compile
//! without `cfg(test)`; `scripts/test-isolated.sh` sandboxes them instead.
//!
//! Both the library and the binary declare this module: each crate root has
//! its own `cfg(test)`, so a module shared through the library would not
//! guard the binary's unit tests. The converse gap: library code a bin
//! test calls (e.g. `skutter::logging`) is compiled without `cfg(test)`, so
//! only `scripts/test-isolated.sh` covers it there.

// Each crate root uses a different subset of the accessors.
#![allow(dead_code)]

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use entanglement_runtime::layers::UserLayer;
use entanglement_runtime::session_store::SessionStore;

/// Replaces the whole user agents layer (tests + non-XDG setups; also the
/// cross-vendor opt-out).
pub const AGENTS_DIR_ENV: &str = "ENTANGLEMENT_AGENTS_DIR";
/// Replaces the whole user skills layer.
pub const SKILLS_DIR_ENV: &str = "ENTANGLEMENT_SKILLS_DIR";
/// Replaces the whole user slash-commands layer.
pub const COMMANDS_DIR_ENV: &str = "ENTANGLEMENT_COMMANDS_DIR";

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

/// The user layer for the definition kind `env` overrides
/// ([`AGENTS_DIR_ENV`]/[`SKILLS_DIR_ENV`]/[`COMMANDS_DIR_ENV`]): exactly
/// that dir when set, else the cross-vendor + native dirs under the real
/// home and config dirs.
#[track_caller]
pub(crate) fn user_layer(env: &str) -> UserLayer {
    user_layer_from(
        env,
        |name| std::env::var_os(name),
        || (home_dir(), config_dir()),
    )
}

/// [`user_layer`] over an injected env lookup and `(home, config)` dirs —
/// the latter consulted only when the override is unset. The definitions
/// watcher maps the same way, so it watches exactly the dirs loaded.
pub(crate) fn user_layer_from(
    env: &str,
    lookup: impl Fn(&str) -> Option<OsString>,
    dirs: impl FnOnce() -> (Option<PathBuf>, Option<PathBuf>),
) -> UserLayer {
    if let Some(over) = lookup(env) {
        return UserLayer::Dir(over.into());
    }
    let (home, config) = dirs();
    UserLayer::Home { home, config }
}

/// `cwd`'s session store under `<data_dir>/entanglement/sessions`; one that
/// keeps nothing when the platform has no data dir.
#[track_caller]
pub(crate) fn session_store(cwd: &Path) -> SessionStore {
    match data_dir() {
        Some(data) => SessionStore::for_cwd(&data.join("entanglement/sessions"), cwd),
        None => SessionStore::unavailable(),
    }
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
    use super::*;

    #[test]
    #[should_panic(expected = "resolved the real user home dir")]
    fn home_dir_is_unreachable_under_test() {
        home_dir();
    }

    #[test]
    #[should_panic(expected = "resolved the real user config dir")]
    fn config_dir_is_unreachable_under_test() {
        config_dir();
    }

    #[test]
    #[should_panic(expected = "resolved the real user data dir")]
    fn data_dir_is_unreachable_under_test() {
        data_dir();
    }

    #[test]
    #[should_panic(expected = "resolved the real user data dir")]
    fn session_store_refuses_the_real_data_dir_under_test() {
        // The #747 guard: a unit test can never reach the developer's live logs.
        let _ = session_store(Path::new("/some/project"));
    }

    /// A set override is the whole user layer, even when missing (the
    /// library warns), and the home/config dirs are never consulted.
    #[test]
    fn an_override_replaces_the_whole_user_layer() {
        let layer = user_layer_from(
            SKILLS_DIR_ENV,
            |n| (n == SKILLS_DIR_ENV).then(|| "/no-such-dir".into()),
            || panic!("home/config must not be consulted under an override"),
        );
        assert!(matches!(layer, UserLayer::Dir(d) if d == Path::new("/no-such-dir")));
    }

    #[test]
    fn an_unset_override_reads_the_home_and_config_dirs() {
        let layer = user_layer_from(
            SKILLS_DIR_ENV,
            |_| None,
            || (Some("/home/u".into()), Some("/home/u/.config".into())),
        );
        let UserLayer::Home { home, config } = layer else {
            panic!("expected the home/config layer, got {layer:?}");
        };
        assert_eq!(home.as_deref(), Some(Path::new("/home/u")));
        assert_eq!(config.as_deref(), Some(Path::new("/home/u/.config")));
    }
}
