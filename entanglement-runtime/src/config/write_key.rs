//! Comment-preserving writer for one top-level key of the user's `config.yml`
//! (#560; the managed-file discipline of [ADR-0084], the "materialize into the
//! file a user already hand-edits" principle of [ADR-0083]).
//!
//! [`mcp_persist`][super::mcp_persist] rewrites the `mcp:` section through a
//! `serde_yaml::Value` round-trip, which drops **every comment in the file**.
//! That is tolerable for a section only `/mcp add|remove` ever authors; it is
//! not tolerable for the keys the first-run scaffold (#219) documents inline —
//! persisting one setting from a dialog must not silently delete the ~190 lines
//! of explanation that make the file discoverable. `serde_yaml` cannot
//! round-trip comments, so this module does a **targeted line-range edit**
//! instead of a parse-and-serialize: only the written key's own lines change,
//! and every other line, comment, blank line and line terminator survives byte
//! for byte.
//!
//! [`upsert_block`] resolves three cases, first match wins:
//!
//! 1. a **live** `key:` block — the key line plus its indented continuation —
//!    is replaced in place, keeping the key's position and the comments above
//!    it;
//! 2. else a **commented scaffold** line (`#key:` plus any commented children)
//!    becomes the real key, again in place, so an untouched template keeps its
//!    ordering and its explanations;
//! 3. else the block is appended with a one-line comment.
//!
//! [`save_key`] wraps it with the I/O every managed write in this module
//! shares: the advisory lock (#329) around read-modify-write, so a second
//! `skutter` can't clobber the update, and [`atomic_write`] for the write
//! itself. The writer re-parses its own output before committing — a config
//! file this process corrupted would be a loud error on the *next* startup,
//! far from the cause.
//!
//! The user file is watched (`watch.rs` watches `${config_dir}/entanglement/`
//! and the parent of any `ENTANGLEMENT_*_FILE` override), so a long-running
//! head picks the change up within one debounce window — for the surfaces that
//! re-read it, per ADR-0084's documented limit that a running session keeps
//! what it resolved at start. Which is exactly the contract these two settings
//! want: persisting sets the default for the **next** session, while the live
//! re-pin changes the running one.
//!
//! [ADR-0084]: ../../../docs/adr/0084-runtime-live-reload-and-managed-file-locking.md
//! [ADR-0083]: ../../../docs/adr/0083-in-app-tool-allowlist-editing-as-user-layer-materialization.md

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use entanglement_core::{Discovery, ToolAdvertising};
use serde_yaml::Value;

use super::{atomic::atomic_write, lock, user_config_path, CONFIG_FILE_ENV, TEMPLATE_YML};

const ADVERTISING_COMMENT: &str =
    "Tool advertising (ADR-0196): full | tool_search. Saved from the /set dialog.";
const DISCOVERY_COMMENT: &str =
    "Client-side discovery strategy per provider (ADR-0204). Saved from the /set dialog.";

/// Persist the session's advertising mode and/or its provider's discovery
/// strategy as the install-wide defaults for **new** sessions. Each key is its
/// own locked read-modify-write; `None` skips that key entirely, so the dialog
/// never rewrites a setting the user didn't touch.
pub fn save_advertising_defaults(
    mode: Option<ToolAdvertising>,
    provider: &str,
    discovery: Option<Discovery>,
) -> Result<()> {
    if let Some(mode) = mode {
        save_tool_advertising(mode).context("saving tool_advertising")?;
    }
    if let Some(discovery) = discovery {
        save_discovery(provider, discovery)
            .with_context(|| format!("saving discovery for provider '{provider}'"))?;
    }
    Ok(())
}

/// Write `tool_advertising: <mode>`.
pub fn save_tool_advertising(mode: ToolAdvertising) -> Result<PathBuf> {
    save_key("tool_advertising", ADVERTISING_COMMENT, |_| {
        Ok(Value::String(mode.label().to_string()))
    })
}

/// Write `discovery: {<provider>: <strategy>}`, **merging** into whatever the
/// file already holds: the map is per provider and a head only ever knows the
/// one it is talking to, so replacing the whole map would silently drop a
/// user's entries for their other providers.
pub fn save_discovery(provider: &str, strategy: Discovery) -> Result<PathBuf> {
    save_key("discovery", DISCOVERY_COMMENT, |current| {
        let mut map = match current {
            Some(Value::Mapping(m)) => m.clone(),
            _ => serde_yaml::Mapping::new(),
        };
        map.insert(
            Value::String(provider.to_string()),
            Value::String(strategy.label().to_string()),
        );
        Ok(Value::Mapping(map))
    })
}

/// Set one top-level `key` of the user config to whatever `value` returns for
/// the key's current value (`None` when the file doesn't set it), preserving
/// every other byte of the file. `comment` is the one-line note written above
/// the key when it has to be appended. Returns the written path.
///
/// Read-modify-write under the shared advisory lock: `value` sees the file's
/// *current* on-disk state, re-read inside the lock, per
/// [`lock::with_locked_file`]'s contract. An absent file is created from the
/// commented scaffold (like [`super::env_key::set_key`] does for the managed
/// `.env`), so the very first persist still yields a documented file.
pub fn save_key(
    key: &str,
    comment: &str,
    value: impl FnOnce(Option<&Value>) -> Result<Value>,
) -> Result<PathBuf> {
    let path = user_config_path().ok_or_else(|| {
        anyhow!("no config directory available; set {CONFIG_FILE_ENV} to a path first")
    })?;
    lock::with_locked_file(&path, || {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating config dir {}", parent.display()))?;
                }
                TEMPLATE_YML.to_string()
            }
            Err(e) => {
                return Err(e).with_context(|| format!("reading user config {}", path.display()))
            }
        };
        let doc: Value = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing user config {}", path.display()))?;
        let new_value = value(doc.get(key))?;
        let block = render_block(key, &new_value)?;
        let updated = upsert_block(&text, key, &block, comment);
        verify(&updated, key, &new_value)
            .with_context(|| format!("refusing to write a broken {}", path.display()))?;
        atomic_write(&path, &updated)?;
        Ok(path.clone())
    })
}

/// `key` and its value as YAML lines — `tool_advertising: full` or a
/// `discovery:` header with one indented line per provider.
fn render_block(key: &str, value: &Value) -> Result<String> {
    let mut map = serde_yaml::Mapping::new();
    map.insert(Value::String(key.to_string()), value.clone());
    serde_yaml::to_string(&Value::Mapping(map)).context("serializing the config key")
}

/// Re-parse the rewritten file and confirm `key` now reads back as `value`.
/// Cheap insurance that a line-range edit never commits something the loader
/// would reject (or, worse, read as a different setting).
fn verify(text: &str, key: &str, value: &Value) -> Result<()> {
    let doc: Value = serde_yaml::from_str(text).context("re-parsing the rewritten config")?;
    match doc.get(key) {
        Some(got) if got == value => Ok(()),
        Some(got) => Err(anyhow!("`{key}` read back as {got:?}, expected {value:?}")),
        None => Err(anyhow!("`{key}` is missing from the rewritten config")),
    }
}

/// Splice `block` into `text` as the value of top-level `key`. See the module
/// doc for the three cases; every untouched line keeps its exact bytes.
pub fn upsert_block(text: &str, key: &str, block: &str, comment: &str) -> String {
    let segments: Vec<&str> = text.split_inclusive('\n').collect();

    if let Some(start) = segments.iter().position(|s| live_key(s, key)) {
        return splice(&segments, start, live_block_end(&segments, start), block);
    }
    if let Some(start) = segments.iter().position(|s| commented_key(s, key)) {
        return splice(
            &segments,
            start,
            commented_block_end(&segments, start),
            block,
        );
    }
    append(text, block, comment)
}

/// Replace `segments[start..end]` with `block`'s lines, reusing the replaced
/// key line's own terminator so a CRLF file stays CRLF and a file whose last
/// line has no newline doesn't grow one. `pub(super)`: [`super::migrate_permissions`]
/// reuses this pure line-range splice with its own, more conservative block-end
/// rule ([`live_block_end`]'s column-0-comment-terminates-the-block heuristic,
/// built for the tightly controlled first-run scaffold, mis-detects a
/// hand-edited file that leaves a stray commented example between a live key
/// and its real indented content — found corrupting a real `config.yml` via
/// the integration suite).
pub(super) fn splice(segments: &[&str], start: usize, end: usize, block: &str) -> String {
    let term = if segments[start].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let trailing = if segments[start].ends_with('\n') {
        term
    } else {
        ""
    };

    let mut out =
        String::with_capacity(segments.iter().map(|s| s.len()).sum::<usize>() + block.len());
    out.extend(segments[..start].iter().copied());
    let lines: Vec<&str> = block.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        out.push_str(line);
        out.push_str(if i + 1 < lines.len() { term } else { trailing });
    }
    out.extend(segments[end..].iter().copied());
    out
}

/// Append `block` under a one-line `comment`, separated by a blank line.
fn append(text: &str, block: &str, comment: &str) -> String {
    // A file that already uses CRLF keeps using it; a fresh/LF file gets LF.
    let term = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(term);
    }
    if !out.is_empty() {
        out.push_str(term);
    }
    out.push_str(&format!("# {comment}{term}"));
    for line in block.lines() {
        out.push_str(line);
        out.push_str(term);
    }
    out
}

/// A segment's content without its line terminator.
fn content(segment: &str) -> &str {
    segment.trim_end_matches('\n').trim_end_matches('\r')
}

/// Whether `segment` is a live top-level `key:` line — column 0, uncommented,
/// and the text before the first `:` is exactly `key`.
fn live_key(segment: &str, key: &str) -> bool {
    let c = content(segment);
    if c.starts_with(char::is_whitespace) || c.starts_with('#') {
        return false;
    }
    c.split_once(':').is_some_and(|(k, _)| k.trim() == key)
}

/// Where a live block ends (exclusive): the key line plus every indented
/// continuation line. Trailing blank lines are left out — they read as the
/// separator before the *next* setting's comment, not as part of this value.
/// A column-0 comment ends the block for the same reason.
fn live_block_end(segments: &[&str], start: usize) -> usize {
    let mut end = start + 1;
    let mut i = start + 1;
    while i < segments.len() {
        let c = content(segments[i]);
        if c.trim().is_empty() {
            i += 1;
        } else if c.starts_with(char::is_whitespace) {
            i += 1;
            end = i;
        } else {
            break;
        }
    }
    end
}

/// Whether `segment` is the scaffold's commented placeholder for `key`
/// (`#key:` or `# key:`) — at most one space after the `#`, which is what
/// separates a commented *key* from a commented *child* (below).
fn commented_key(segment: &str, key: &str) -> bool {
    let c = content(segment);
    let Some(rest) = c.strip_prefix('#') else {
        return false;
    };
    let rest = rest.strip_prefix(' ').unwrap_or(rest);
    if rest.starts_with(char::is_whitespace) {
        return false;
    }
    rest.split_once(':').is_some_and(|(k, _)| k.trim() == key)
}

/// Where a commented placeholder's block ends (exclusive): the `#key:` line
/// plus the commented example children directly under it.
fn commented_block_end(segments: &[&str], start: usize) -> usize {
    let mut end = start + 1;
    while end < segments.len() && commented_child(segments[end]) {
        end += 1;
    }
    end
}

/// Whether `segment` is a commented *child* of the line above — `#` followed
/// by at least two spaces of YAML indent (`#  zai: native_first`). WHY the
/// indent test: the scaffold writes prose with exactly one space (`# Tool
/// advertising …`), so the indent is what tells an example value apart from
/// the next setting's explanation, which must never be eaten.
fn commented_child(segment: &str) -> bool {
    let c = content(segment);
    let Some(rest) = c.strip_prefix('#') else {
        return false;
    };
    let trimmed = rest.trim_start();
    !trimmed.is_empty() && rest.len() - trimmed.len() >= 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ENV_LOCK};

    fn advertising(mode: &str) -> String {
        format!("tool_advertising: {mode}\n")
    }

    #[test]
    fn writing_into_the_untouched_scaffold_uncomments_the_key_in_place() {
        let out = upsert_block(
            TEMPLATE_YML,
            "tool_advertising",
            &advertising("full"),
            "unused",
        );

        assert!(out.contains("\ntool_advertising: full\n"));
        assert!(
            !out.contains("#tool_advertising: tool_search"),
            "the placeholder became the real key"
        );
        // Every other line of the scaffold survives, comments included.
        for line in TEMPLATE_YML.lines() {
            if line.starts_with("#tool_advertising") {
                continue;
            }
            assert!(out.contains(line), "lost scaffold line {line:?}");
        }
        // And it parses back to exactly the intended value.
        verify(&out, "tool_advertising", &Value::String("full".into())).unwrap();
    }

    #[test]
    fn a_commented_block_takes_its_example_children_with_it() {
        let out = upsert_block(
            TEMPLATE_YML,
            "discovery",
            "discovery:\n  zai: invoke\n",
            "unused",
        );

        assert!(out.contains("\ndiscovery:\n  zai: invoke\n"), "{out}");
        assert!(!out.contains("#  zai: native_first"), "children replaced");
        assert!(!out.contains("#  gemini: invoke"), "children replaced");
        // The prose above the placeholder is a one-space comment and stays.
        assert!(out.contains("# Client-side discovery strategy, per provider"));
        let mut want = serde_yaml::Mapping::new();
        want.insert(Value::String("zai".into()), Value::String("invoke".into()));
        verify(&out, "discovery", &Value::Mapping(want)).unwrap();
    }

    #[test]
    fn replacing_an_existing_value_touches_only_that_line() {
        let text = "# lead-in\nprovider: zai\n\n# mode\ntool_advertising: tool_search\n\n# tail\nverbose: true\n";
        let out = upsert_block(text, "tool_advertising", &advertising("full"), "unused");
        assert_eq!(
            out,
            "# lead-in\nprovider: zai\n\n# mode\ntool_advertising: full\n\n# tail\nverbose: true\n"
        );
    }

    #[test]
    fn replacing_a_multi_line_block_drops_its_old_children() {
        let text = "discovery:\n  zai: append\n  gemini: invoke\n\n# next\nverbose: true\n";
        let out = upsert_block(text, "discovery", "discovery:\n  zai: invoke\n", "unused");
        assert_eq!(
            out, "discovery:\n  zai: invoke\n\n# next\nverbose: true\n",
            "the blank line and the next key's comment are untouched"
        );
    }

    #[test]
    fn a_missing_key_is_appended_with_its_comment() {
        let text = "# lead-in\nprovider: zai\n";
        let out = upsert_block(text, "tool_advertising", &advertising("full"), "why");
        assert_eq!(
            out,
            "# lead-in\nprovider: zai\n\n# why\ntool_advertising: full\n"
        );
    }

    #[test]
    fn crlf_line_endings_survive_both_paths() {
        let text = "# lead-in\r\ntool_advertising: tool_search\r\n# tail\r\nverbose: true\r\n";
        let out = upsert_block(text, "tool_advertising", &advertising("full"), "why");
        assert_eq!(
            out,
            "# lead-in\r\ntool_advertising: full\r\n# tail\r\nverbose: true\r\n"
        );
        assert!(!out.contains("full\n# tail"), "no bare LF introduced");

        // The append path keeps the file's terminator too, including for the
        // multi-line block.
        let out = upsert_block(text, "discovery", "discovery:\n  zai: invoke\n", "why");
        assert!(
            out.ends_with("\r\n# why\r\ndiscovery:\r\n  zai: invoke\r\n"),
            "{out:?}"
        );
        assert!(!out.contains("discovery:\n"), "no bare LF introduced");
    }

    #[test]
    fn a_file_without_a_trailing_newline_keeps_not_having_one() {
        // Replacing the very last line: no newline is invented.
        let text = "provider: zai\ntool_advertising: tool_search";
        let out = upsert_block(text, "tool_advertising", &advertising("full"), "why");
        assert_eq!(out, "provider: zai\ntool_advertising: full");

        // Appending after it completes the last line first, so the file stays
        // parseable rather than gluing two keys onto one line.
        let out = upsert_block(text, "discovery", "discovery:\n  zai: invoke\n", "why");
        assert_eq!(
            out,
            "provider: zai\ntool_advertising: tool_search\n\n# why\ndiscovery:\n  zai: invoke\n"
        );
    }

    #[test]
    fn a_commented_child_is_never_mistaken_for_a_key() {
        // `#  zai: …` is an example under `#discovery:`, not a `zai:` key.
        assert!(!commented_key("#  zai: native_first\n", "zai"));
        assert!(commented_key("#discovery:\n", "discovery"));
        assert!(commented_key("# discovery:\n", "discovery"));
        assert!(commented_child("#  zai: native_first\n"));
        assert!(!commented_child("# Client-side discovery strategy\n"));
        // An indented `tool_advertising:` inside some other block is not the
        // top-level key.
        assert!(!live_key("  tool_advertising: full\n", "tool_advertising"));
        assert!(live_key("tool_advertising: full", "tool_advertising"));
    }

    #[test]
    fn two_keys_written_in_sequence_both_land_and_reload() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yml");
        std::env::set_var(CONFIG_FILE_ENV, &path);

        // No file yet: the writer scaffolds from the template, then edits it.
        save_advertising_defaults(Some(ToolAdvertising::Full), "zai", Some(Discovery::Invoke))
            .unwrap();
        // A second provider merges in rather than replacing the map, and the
        // mode is rewritten in place.
        save_advertising_defaults(
            Some(ToolAdvertising::ToolSearch),
            "gemini",
            Some(Discovery::NativeFirst),
        )
        .unwrap();

        let resolved = Config::load(dir.path()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::env::remove_var(CONFIG_FILE_ENV);

        assert_eq!(resolved.tool_advertising, Some(ToolAdvertising::ToolSearch));
        assert_eq!(resolved.discovery.get("zai"), Some(&Discovery::Invoke));
        assert_eq!(
            resolved.discovery.get("gemini"),
            Some(&Discovery::NativeFirst)
        );
        // Sibling settings and the scaffold's documentation are still there.
        assert!(text.contains("# entanglement — user configuration."));
        assert!(
            text.contains("#agent: general"),
            "untouched keys stay commented"
        );
        assert_eq!(
            resolved.agent.as_deref(),
            Some("general"),
            "from the defaults"
        );
    }

    #[test]
    fn no_config_path_is_a_loud_error() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("config.yml");
        std::env::set_var(CONFIG_FILE_ENV, &path);
        // A missing parent dir is created, not an error — the first persist on
        // a machine with no config dir yet must still work.
        let written = save_tool_advertising(ToolAdvertising::Full).unwrap();
        std::env::remove_var(CONFIG_FILE_ENV);
        assert_eq!(written, path);
        assert!(path.exists());
    }
}
