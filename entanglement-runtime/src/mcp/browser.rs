//! Opening the OAuth authorization URL in the user's browser (ADR-0153).
//!
//! Runtime-side on purpose: spawning a process is policy, and
//! `entanglement-provider` stays free of it (it only hands back the URL).
//!
//! Launching is **not** gated behind an opt-in env var. The user typed
//! `/mcp connect <server>`; opening their own browser to finish the flow they
//! just asked for is the obvious intent, and it stays inside the local trust
//! model (ADR-0047/ADR-0048) — this is a local, single-user tool driving a
//! local browser with a URL the runtime itself constructed.
//!
//! The URL is **always** emitted to the head as well, whether or not the launch
//! succeeds, so an SSH/headless session can copy it by hand. A failure here is
//! therefore never fatal: it degrades to "copy this link".

use std::process::Stdio;

/// Try to open `url` in the platform's default browser. Returns whether a
/// launcher was successfully spawned — the caller reports the URL either way.
///
/// The child is fully detached from our stdio: `xdg-open` on some desktops
/// forwards the browser's chatter to stderr, which would corrupt the TUI's
/// terminal.
pub fn open(url: &str) -> bool {
    for (program, args) in launchers() {
        let spawned = std::process::Command::new(program)
            .args(args)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        match spawned {
            Ok(_child) => {
                // Deliberately not awaited: `xdg-open` exits immediately on some
                // systems and lingers for the browser's lifetime on others, so
                // its status says nothing useful. Not reaping it leaves a
                // short-lived zombie at most — the process outlives us anyway.
                tracing::debug!("opened the authorization URL with `{program}`");
                return true;
            }
            // Try the next candidate: a missing launcher is the common case on
            // a minimal or headless system.
            Err(e) => tracing::debug!("could not launch `{program}`: {e}"),
        }
    }
    tracing::info!("no browser launcher available; the authorization URL was printed instead");
    false
}

/// Platform launchers, in preference order.
fn launchers() -> Vec<(&'static str, &'static [&'static str])> {
    if cfg!(target_os = "macos") {
        vec![("open", &[][..])]
    } else if cfg!(target_os = "windows") {
        // `start` is a shell builtin, so it needs `cmd /C`; the empty string is
        // the window title `start` would otherwise consume from the URL.
        vec![("cmd", &["/C", "start", ""][..])]
    } else {
        // Linux/BSD: the XDG launcher, then common desktop fallbacks.
        vec![
            ("xdg-open", &[][..]),
            ("gio", &["open"][..]),
            ("wslview", &[][..]),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launcher_list_is_non_empty_and_platform_shaped() {
        let l = launchers();
        assert!(!l.is_empty());
        if cfg!(target_os = "linux") {
            assert_eq!(l[0].0, "xdg-open");
            assert!(l[0].1.is_empty());
        }
        if cfg!(target_os = "windows") {
            assert_eq!(l[0].0, "cmd");
            assert_eq!(l[0].1, &["/C", "start", ""]);
        }
    }

    #[test]
    fn open_reports_failure_when_no_launcher_exists() {
        // Not a launch test (CI has no browser) — this pins the contract that a
        // missing launcher returns `false` rather than panicking, which is what
        // makes the "print the URL anyway" fallback sound.
        //
        // Scrub PATH so even a present `xdg-open` can't be found.
        let saved = std::env::var_os("PATH");
        std::env::set_var("PATH", "");
        let opened = open("https://example.invalid/authorize");
        match saved {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
        assert!(!opened);
    }
}
