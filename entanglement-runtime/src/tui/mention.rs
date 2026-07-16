//! `@file` reference completion (issue #15, ADR-0030). A TUI-side fuzzy file
//! finder over the working directory: typing `@` opens a popup that filters the
//! indexed relative paths as you type; Tab/Enter inserts the pick as `@path`.
//!
//! The index is a flat snapshot built once at TUI startup from the same
//! `glob`-based enumeration the `glob` host tool uses, so completion stays
//! head-side and never touches the engine.

use ratatui::widgets::ListState;
use std::ops::Range;
use std::path::Path;

use crate::host::list_files;

/// Directory names never worth indexing for `@file` completion. `.git` and other
/// dot-dirs are already skipped by the glob walk (a leading `*` won't match a
/// leading `.`); these are the noisy non-hidden build/vendor trees.
const IGNORED_DIRS: [&str; 6] = [".git", "target", "node_modules", ".venv", "dist", "build"];

/// Cap on rows shown in the popup — the index itself is bounded by the glob
/// walk's own file cap.
const MAX_MATCHES: usize = 50;

/// A flat, relative-path index of the working directory's files, built once at
/// TUI startup. Paths use `/` separators regardless of platform.
#[derive(Debug, Default, Clone)]
pub struct FileIndex {
    files: Vec<String>,
}

impl FileIndex {
    /// Enumerate files under `root` (bounded by the glob walk's cap), dropping
    /// entries inside [`IGNORED_DIRS`]. A read error yields an empty index
    /// rather than failing the TUI.
    pub fn build(root: &Path) -> Self {
        let mut files: Vec<String> = match list_files(root, "**/*", &[]) {
            Ok(list) => list
                .files
                .iter()
                .filter_map(|p| p.strip_prefix(root).ok())
                .map(|rel| rel.to_string_lossy().replace('\\', "/"))
                .filter(|rel| !is_ignored(rel))
                .collect(),
            Err(_) => Vec::new(),
        };
        files.sort();
        files.dedup();
        Self { files }
    }

    pub fn files(&self) -> &[String] {
        &self.files
    }

    #[cfg(test)]
    pub fn from_paths(mut files: Vec<String>) -> Self {
        files.sort();
        files.dedup();
        Self { files }
    }
}

fn is_ignored(rel: &str) -> bool {
    rel.split('/').any(|seg| IGNORED_DIRS.contains(&seg))
}

/// Fuzzy subsequence score of `query` against `candidate`. Higher is better;
/// `None` if `query` isn't a subsequence of `candidate`. Matches in the basename
/// and consecutive runs score higher, so `foo` ranks `src/foo.rs` above a
/// scattered `f…o…o` match, and shorter paths break ties.
fn fuzzy_score(candidate: &str, query: &str) -> Option<i64> {
    if query.is_empty() {
        return Some(0);
    }
    let cand: Vec<char> = candidate.chars().collect();
    let q: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    let basename_start = cand.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);

    let mut score: i64 = 0;
    let mut qi = 0;
    let mut prev_match: Option<usize> = None;
    for (ci, &ch) in cand.iter().enumerate() {
        if qi >= q.len() {
            break;
        }
        let cl = ch.to_lowercase().next().unwrap_or(ch);
        if cl == q[qi] {
            score += 1;
            if ci >= basename_start {
                score += 2;
            }
            if prev_match == Some(ci.wrapping_sub(1)) {
                score += 3;
            }
            prev_match = Some(ci);
            qi += 1;
        }
    }
    (qi == q.len()).then(|| score * 100 - cand.len() as i64)
}

/// If the text immediately before the cursor ends in an `@`-prefixed token,
/// return the query (chars after `@`). The `@` must sit at the start of the line
/// or follow whitespace so `user@host` doesn't trigger it, and the token must
/// not contain whitespace (a completed word ends the mention).
pub fn active_mention_query(line_before_cursor: &str) -> Option<&str> {
    let at = line_before_cursor.rfind('@')?;
    if at > 0 {
        let prev = line_before_cursor[..at].chars().next_back()?;
        if !prev.is_whitespace() {
            return None;
        }
    }
    let query = &line_before_cursor[at + 1..];
    if query.chars().any(char::is_whitespace) {
        return None;
    }
    Some(query)
}

/// Byte range `[@ … cursor)` of the active mention token on the line, for
/// replacement. `None` when there is no active mention.
pub fn active_mention_range(line_before_cursor: &str) -> Option<Range<usize>> {
    active_mention_query(line_before_cursor)?;
    let at = line_before_cursor.rfind('@')?;
    Some(at..line_before_cursor.len())
}

/// Popup state for `@file` completion — persistent across frames (so
/// navigation survives redraws), with `query`/`matches` recomputed whenever the
/// input line changes via [`MentionPopup::update`].
pub struct MentionPopup {
    index: FileIndex,
    visible: bool,
    query: String,
    matches: Vec<String>,
    state: ListState,
}

impl MentionPopup {
    pub fn new(index: FileIndex) -> Self {
        Self {
            index,
            visible: false,
            query: String::new(),
            matches: Vec::new(),
            state: ListState::default(),
        }
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    pub fn matches(&self) -> &[String] {
        &self.matches
    }

    #[cfg(test)]
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn state(&mut self) -> &mut ListState {
        &mut self.state
    }

    pub fn selected(&self) -> Option<&String> {
        self.state.selected().and_then(|i| self.matches.get(i))
    }

    pub fn hide(&mut self) {
        self.visible = false;
        self.matches.clear();
        self.state.select(None);
    }

    /// Recompute from the input line up to the cursor. Shows the popup iff an
    /// active `@token` is present and matched at least one file.
    pub fn update(&mut self, line_before_cursor: &str) {
        match active_mention_query(line_before_cursor) {
            Some(query) => {
                self.query = query.to_string();
                self.recompute();
                self.visible = !self.matches.is_empty();
            }
            None => self.hide(),
        }
    }

    fn recompute(&mut self) {
        let mut scored: Vec<(i64, &String)> = self
            .index
            .files()
            .iter()
            .filter_map(|f| fuzzy_score(f, &self.query).map(|s| (s, f)))
            .collect();
        // Best score first; stable path order breaks ties for a deterministic list.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        self.matches = scored
            .into_iter()
            .take(MAX_MATCHES)
            .map(|(_, f)| f.clone())
            .collect();
        self.state.select((!self.matches.is_empty()).then_some(0));
    }

    pub fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0);
        self.state.select(Some((cur + 1) % self.matches.len()));
    }

    pub fn select_prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let cur = self.state.selected().unwrap_or(0);
        let prev = if cur == 0 {
            self.matches.len() - 1
        } else {
            cur - 1
        };
        self.state.select(Some(prev));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignored_dirs_filtered() {
        assert!(is_ignored("target/debug/foo"));
        assert!(is_ignored("node_modules/x/y.js"));
        assert!(!is_ignored("src/main.rs"));
    }

    #[test]
    fn from_paths_sorts_and_dedups() {
        let idx = FileIndex::from_paths(vec!["b".into(), "a".into(), "a".into()]);
        assert_eq!(idx.files(), &["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn fuzzy_prefers_basename_and_consecutive() {
        // Consecutive basename match should outrank a scattered one.
        let good = fuzzy_score("src/foo.rs", "foo").unwrap();
        let scattered = fuzzy_score("f/o/o.rs", "foo").unwrap();
        assert!(good > scattered, "good={good} scattered={scattered}");
        assert!(fuzzy_score("src/main.rs", "xyz").is_none());
    }

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_score("anything", ""), Some(0));
    }

    #[test]
    fn active_query_requires_word_boundary() {
        assert_eq!(active_mention_query("@sr"), Some("sr"));
        assert_eq!(active_mention_query("look at @src/ma"), Some("src/ma"));
        assert_eq!(active_mention_query("@"), Some(""));
        // Not a mention: `@` mid-word (email-like) or token already ended.
        assert_eq!(active_mention_query("user@host"), None);
        assert_eq!(active_mention_query("@src done "), None);
    }

    #[test]
    fn active_range_spans_at_to_cursor() {
        assert_eq!(active_mention_range("hi @src"), Some(3..7));
        assert_eq!(active_mention_range("no mention"), None);
    }

    #[test]
    fn popup_shows_and_selects_on_match() {
        let idx = FileIndex::from_paths(vec![
            "src/main.rs".into(),
            "src/tui/app.rs".into(),
            "README.md".into(),
        ]);
        let mut popup = MentionPopup::new(idx);

        popup.update("@app");
        assert!(popup.visible());
        assert_eq!(popup.query(), "app");
        assert_eq!(popup.selected(), Some(&"src/tui/app.rs".to_string()));

        // A query that matches nothing hides the popup.
        popup.update("@zzzzz");
        assert!(!popup.visible());

        // Losing the token hides it too.
        popup.update("plain text");
        assert!(!popup.visible());
        assert!(popup.selected().is_none());
    }

    #[test]
    fn popup_navigation_wraps() {
        let idx = FileIndex::from_paths(vec!["a.rs".into(), "ab.rs".into()]);
        let mut popup = MentionPopup::new(idx);
        popup.update("@a");
        let first = popup.selected().cloned();
        popup.select_next();
        assert_ne!(popup.selected().cloned(), first);
        popup.select_next();
        assert_eq!(popup.selected().cloned(), first);
    }
}
