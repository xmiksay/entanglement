//! Tracing subscriber setup for the `skutter` binary.
//!
//! Filter precedence: `RUST_LOG` wins (via `EnvFilter::try_from_default_env`,
//! so per-target directives and `trace` are reachable — e.g.
//! `RUST_LOG=entanglement_core::host=trace`); absent that, `--verbose` selects
//! `debug`, otherwise `warn`. In TUI mode the terminal is in raw mode and owns
//! the screen, so logs go to a file sink instead of stderr — otherwise a
//! mid-session `WARN` would corrupt the display.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

/// The fallback log directive when `RUST_LOG` is unset or unparseable.
fn fallback_directive(verbose: bool) -> &'static str {
    if verbose {
        "debug"
    } else {
        "warn"
    }
}

/// Builds the log filter: `RUST_LOG` if present and valid, else the
/// verbosity-derived default. `try_from_default_env` keeps `trace` and
/// per-target directives reachable, which the plain `--verbose` string never
/// allowed (issue #187).
fn build_filter(verbose: bool) -> EnvFilter {
    EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(fallback_directive(verbose)))
}

/// Initializes the global tracing subscriber.
///
/// `tui` routes logs to a file sink (`log_file_path`) with ANSI disabled, since
/// the TUI holds the terminal in raw mode; every other mode logs to stderr
/// (stdout is reserved for command output — NDJSON frames, inspected prompts).
pub fn init(verbose: bool, tui: bool) -> Result<()> {
    let builder = tracing_subscriber::fmt().with_env_filter(build_filter(verbose));
    if tui {
        let path = log_file_path()?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("opening log file {}", path.display()))?;
        eprintln!("skutter: logging to {}", path.display());
        builder.with_ansi(false).with_writer(Arc::new(file)).init();
    } else {
        builder.with_writer(std::io::stderr).init();
    }
    Ok(())
}

/// The TUI log file: `<data_dir>/entanglement/logs/skutter.log`, creating the
/// directory if it doesn't exist.
fn log_file_path() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Failed to determine data directory")?;
    let dir = data_dir.join("entanglement").join("logs");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating log directory {}", dir.display()))?;
    Ok(dir.join("skutter.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_directive_tracks_verbose() {
        assert_eq!(fallback_directive(false), "warn");
        assert_eq!(fallback_directive(true), "debug");
    }

    #[test]
    fn rust_log_overrides_verbose_and_reaches_trace() {
        // `RUST_LOG` is process-global; keep the whole set/assert/restore in one
        // test so no other test races on it.
        let prev = std::env::var("RUST_LOG").ok();

        std::env::set_var("RUST_LOG", "entanglement_core::host=trace");
        // Even with verbose=false (which would otherwise be `warn`), the env
        // directive wins and a trace target is reachable.
        assert!(build_filter(false).to_string().contains("trace"));

        std::env::remove_var("RUST_LOG");
        assert_eq!(build_filter(false).to_string(), "warn");
        assert_eq!(build_filter(true).to_string(), "debug");

        match prev {
            Some(v) => std::env::set_var("RUST_LOG", v),
            None => std::env::remove_var("RUST_LOG"),
        }
    }
}
