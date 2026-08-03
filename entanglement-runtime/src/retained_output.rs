//! [`RetainedOutputRegistry`] — where a completed operation's output that
//! overflowed its tail/byte cap survives so `poll` can page the remainder
//! (#608, ADR-0161 §7). Replaces `call`'s scratch-artifact dance: retention
//! moves from a file on disk to a size-capped, evicted in-memory entry keyed
//! by a fresh `o-` handle (ADR-0164), minted only when a result actually
//! overflowed — a small result never gets an entry.
//!
//! Generalizes the handle rule this ADR states: not just backgrounded work,
//! but "work you might still have questions about" gets a handle. A blocking
//! `call` that overflows keeps returning its tail inline, plus this handle for
//! the rest.
//!
//! **Explicit `output_file` is untouched** — that is a real file the model
//! asked for, not a truncation workaround. An entry for such a call carries
//! `output_file` instead of a text copy, so `poll` just names the path rather
//! than duplicating its content in memory.
//!
//! Retention is capped per-operation ([`MAX_RETAINED_BYTES`]) and the whole
//! registry is evicted on the same TTL/count shape
//! [`crate::host::jobs::JobRegistry`] uses (#621) — output living in memory
//! rather than on disk is exactly the cost ADR-0161 §7 accepts, and past the
//! cap the excess is reported dropped, never silently lost.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use entanglement_core::{DefaultIdGen, IdGen, IdKind, SessionId};

/// Per-operation retention cap. Past this, the *tail* of the text survives —
/// the same keep-the-end policy as every other bound in this codebase — and
/// the dropped byte count is reported so an operation's very own retention
/// entry can overflow without ever doing so silently.
pub const MAX_RETAINED_BYTES: usize = 256 * 1024;

/// How long a retained entry survives before eviction.
const RETAINED_TTL: Duration = Duration::from_secs(15 * 60);
/// Hard cap on retained entries, independent of the TTL — bounds a burst of
/// many truncated calls spawned faster than the TTL clears them.
const MAX_RETAINED_ENTRIES: usize = 200;

struct Entry {
    owner: Option<SessionId>,
    /// `None` when this entry only points at a real `output_file` — the file
    /// already holds the full text, so it isn't duplicated here.
    text: Option<String>,
    dropped_bytes: usize,
    output_file: Option<String>,
    created: Instant,
}

/// One `poll` page over a retained entry: the requested line slice, whether
/// more remains, and the bookkeeping `poll` needs to render a header.
pub struct Page {
    pub text: String,
    pub total_lines: usize,
    /// 0-based index of the first line included in `text` (== the clamped
    /// `offset` the caller asked for).
    pub start_line: usize,
    pub has_more: bool,
    pub output_file: Option<String>,
    pub dropped_bytes: usize,
}

/// Shared, cheaply-cloned registry of retained operation output. One instance
/// is built at startup and handed to both `call` (the writer) and `poll` (the
/// reader).
#[derive(Clone)]
pub struct RetainedOutputRegistry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    entries: HashMap<String, Entry>,
    id_gen: Arc<dyn IdGen>,
}

impl Default for RetainedOutputRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                entries: HashMap::new(),
                id_gen: Arc::new(DefaultIdGen::new()),
            })),
        }
    }
}

impl RetainedOutputRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a real `output_file` an operation wrote (explicit case,
    /// #608): no text is duplicated in memory, `poll` just names the path.
    pub fn register_file(&self, owner: Option<SessionId>, output_file: String) -> String {
        self.insert(owner, None, 0, Some(output_file))
    }

    /// Register `text` (already known to have overflowed the caller's
    /// tail/byte cap) for `owner`, capping it to [`MAX_RETAINED_BYTES`] and
    /// reporting how much of *that* was dropped. Returns the fresh handle
    /// `poll` dispatches on.
    pub fn register_text(&self, owner: Option<SessionId>, text: String) -> String {
        let (kept, dropped) = cap_retained(text);
        self.insert(owner, Some(kept), dropped, None)
    }

    fn insert(
        &self,
        owner: Option<SessionId>,
        text: Option<String>,
        dropped_bytes: usize,
        output_file: Option<String>,
    ) -> String {
        let mut inner = self.lock();
        evict_expired(&mut inner.entries, Instant::now());
        let id = inner.id_gen.next(IdKind::Output);
        inner.entries.insert(
            id.clone(),
            Entry {
                owner,
                text,
                dropped_bytes,
                output_file,
                created: Instant::now(),
            },
        );
        id
    }

    /// Page `id`'s retained text for `caller`, starting at 0-based line
    /// `offset` and returning up to `tail` lines (`0` ⇒ the rest, still
    /// byte-capped by the caller downstream). `None` when the handle is
    /// unknown *or* isn't `caller`'s own — the same ownership scoping
    /// `JobRegistry`/`AgentRegistry` enforce, so a poll never confirms another
    /// session's handle exists.
    pub fn page(&self, id: &str, caller: &SessionId, offset: u32, tail: u32) -> Option<Page> {
        let mut inner = self.lock();
        evict_expired(&mut inner.entries, Instant::now());
        let entry = inner.entries.get(id)?;
        if !owner_allows(&entry.owner, caller) {
            return None;
        }
        let text = entry.text.as_deref().unwrap_or("");
        let lines: Vec<&str> = text.lines().collect();
        let start = (offset as usize).min(lines.len());
        let end = if tail == 0 {
            lines.len()
        } else {
            (start + tail as usize).min(lines.len())
        };
        Some(Page {
            text: lines[start..end].join("\n"),
            total_lines: lines.len(),
            start_line: start,
            has_more: end < lines.len(),
            output_file: entry.output_file.clone(),
            dropped_bytes: entry.dropped_bytes,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .expect("retained-output registry poisoned")
    }
}

/// Whether `caller` may read an entry owned by `owner` — an ownerless entry
/// (the session-less [`crate::tools::Tool::run`] path) is visible to anyone;
/// an owned entry only to its owner. Mirrors `host::jobs::owner_allows`.
fn owner_allows(owner: &Option<SessionId>, caller: &SessionId) -> bool {
    owner.as_ref().is_none_or(|o| o == caller)
}

/// Truncate `text` to [`MAX_RETAINED_BYTES`], keeping the tail, and report how
/// many bytes were dropped off the front.
fn cap_retained(text: String) -> (String, usize) {
    if text.len() <= MAX_RETAINED_BYTES {
        return (text, 0);
    }
    let mut start = text.len() - MAX_RETAINED_BYTES;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    (text[start..].to_string(), start)
}

/// Remove entries past [`RETAINED_TTL`], then trim to [`MAX_RETAINED_ENTRIES`]
/// (oldest-created first).
fn evict_expired(entries: &mut HashMap<String, Entry>, now: Instant) {
    entries.retain(|_, entry| now.saturating_duration_since(entry.created) < RETAINED_TTL);
    if entries.len() > MAX_RETAINED_ENTRIES {
        let mut by_age: Vec<(String, Instant)> = entries
            .iter()
            .map(|(id, entry)| (id.clone(), entry.created))
            .collect();
        by_age.sort_by_key(|(_, created)| *created);
        for (id, _) in by_age
            .into_iter()
            .take(entries.len() - MAX_RETAINED_ENTRIES)
        {
            entries.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_entry_pages_from_the_requested_offset() {
        let reg = RetainedOutputRegistry::new();
        let owner = SessionId::new("s1");
        let text: String = (1..=100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let id = reg.register_text(Some(owner.clone()), text);
        assert!(id.starts_with("o-"), "{id}");

        let page = reg.page(&id, &owner, 0, 30).unwrap();
        assert_eq!(page.total_lines, 100);
        assert_eq!(page.start_line, 0);
        assert!(page.has_more);
        assert!(page.text.starts_with("line1\n") || page.text.starts_with("line1"));
        assert!(page.text.contains("line30"));
        assert!(!page.text.contains("line31"));

        let page2 = reg.page(&id, &owner, 30, 30).unwrap();
        assert_eq!(page2.start_line, 30);
        assert!(page2.text.contains("line31") && page2.text.contains("line60"));
    }

    #[test]
    fn tail_zero_returns_the_rest_from_offset() {
        let reg = RetainedOutputRegistry::new();
        let owner = SessionId::new("s1");
        let text: String = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let id = reg.register_text(Some(owner.clone()), text);
        let page = reg.page(&id, &owner, 5, 0).unwrap();
        assert!(!page.has_more);
        assert!(page.text.contains("line6") && page.text.contains("line10"));
        assert!(!page.text.contains("line5\n") && !page.text.ends_with("line5"));
    }

    #[test]
    fn unknown_or_foreign_handle_is_none() {
        let reg = RetainedOutputRegistry::new();
        let owner = SessionId::new("owner");
        let stranger = SessionId::new("stranger");
        let id = reg.register_text(Some(owner.clone()), "hi".to_string());
        assert!(reg.page("o-nonexistent", &owner, 0, 30).is_none());
        assert!(reg.page(&id, &stranger, 0, 30).is_none());
        assert!(reg.page(&id, &owner, 0, 30).is_some());
    }

    #[test]
    fn overflow_past_the_retention_cap_is_reported_not_silent() {
        let reg = RetainedOutputRegistry::new();
        let owner = SessionId::new("s1");
        let big = "x".repeat(MAX_RETAINED_BYTES * 2);
        let id = reg.register_text(Some(owner.clone()), big);
        let page = reg.page(&id, &owner, 0, 0).unwrap();
        assert!(page.dropped_bytes > 0, "overflow must be reported");
    }

    #[test]
    fn file_entry_names_the_path_with_no_text_duplication() {
        let reg = RetainedOutputRegistry::new();
        let owner = SessionId::new("s1");
        let id = reg.register_file(Some(owner.clone()), "out/result.txt".to_string());
        let page = reg.page(&id, &owner, 0, 30).unwrap();
        assert_eq!(page.output_file.as_deref(), Some("out/result.txt"));
        assert_eq!(page.total_lines, 0);
    }

    #[test]
    fn ownerless_entry_is_visible_to_any_caller() {
        let reg = RetainedOutputRegistry::new();
        let id = reg.register_text(None, "hi".to_string());
        assert!(reg.page(&id, &SessionId::new("anyone"), 0, 30).is_some());
    }
}
