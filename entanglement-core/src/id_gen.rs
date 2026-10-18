//! Short, sortable, kind-tagged ids behind a pluggable generator (ADR-0164).
//!
//! Format: `<kind>-<epoch-seconds hex><2-hex process salt><3-hex counter>` —
//! 15 characters, e.g. `s-6a708af0a1002`. Seconds (not milliseconds) keep the
//! timestamp a fixed 8 hex chars without wrapping until 2106, so ids sort
//! lexicographically by creation time; the trailing counter restores
//! sub-second ordering and — because it is monotonic, not random — guarantees
//! a process never mints the same id twice.
//!
//! Every mint carries an [`IdContext`] — the lineage the new id belongs to
//! (ADR-0211). [`DefaultIdGen`] uses it to keep a tenant prefix: a child or
//! compaction successor of `t1:s-…` is minted `t1:s-…` too.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use entanglement_provider::UserId;

use crate::protocol::SessionId;

/// The namespace an id belongs to — its one-character prefix. Scope is
/// sessions/agent handles, background jobs, and runtime-minted request ids
/// (correlation ids a head mints itself); a `ToolCall.id` is provider-supplied
/// on the wire and is never run through this (reformatting it broke Gemini,
/// #444).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    /// Session / agent handle — an agent handle IS a session id.
    Session,
    /// Background job (replaces the old `bg-N` counter).
    Job,
    /// Runtime-minted request/correlation id.
    Request,
    /// Retained output from a completed operation that overflowed its
    /// tail/byte cap (#608, ADR-0161 §7) — a blocking `call` that truncated,
    /// paged the rest of the way via `poll`.
    Output,
    /// Background `rhai` script (#637, ADR-0185) — an in-process
    /// `spawn_blocking` task, not an OS process, so it gets its own kind
    /// rather than riding `Job`. `x` (script e*x*ecution): `s`/`r` are taken.
    Script,
}

impl IdKind {
    fn prefix(self) -> char {
        match self {
            IdKind::Session => 's',
            IdKind::Job => 'j',
            IdKind::Request => 'r',
            IdKind::Output => 'o',
            IdKind::Script => 'x',
        }
    }
}

/// What an id is minted *for* (ADR-0211): the session it is spawned under,
/// the session it succeeds (a compaction fork, ADR-0205), and the owning user
/// inherited along that lineage. All `None` for a fresh root, a job, or a
/// request id. `#[non_exhaustive]` so the context can grow without breaking an
/// embedder's [`IdGen`]; build one with [`IdContext::default`],
/// [`child_of`][Self::child_of] or [`successor_of`][Self::successor_of].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct IdContext<'a> {
    pub parent: Option<&'a SessionId>,
    pub predecessor: Option<&'a SessionId>,
    pub user: Option<&'a UserId>,
}

impl<'a> IdContext<'a> {
    /// A sub-agent spawned under `parent`.
    pub fn child_of(parent: &'a SessionId) -> Self {
        Self {
            parent: Some(parent),
            ..Self::default()
        }
    }

    /// A compaction successor continuing `predecessor` (ADR-0205).
    pub fn successor_of(predecessor: &'a SessionId) -> Self {
        Self {
            predecessor: Some(predecessor),
            ..Self::default()
        }
    }

    /// The same context with the lineage's owning user attached.
    pub fn with_user(self, user: Option<&'a UserId>) -> Self {
        Self { user, ..self }
    }

    /// The tenant prefix a new id inherits: everything up to and including
    /// the first `:` of the parent's id, else the predecessor's (the
    /// `{tenant}:{id}` convention, docs/embedding/tenants-and-heads.md §1). `None` when there is
    /// no lineage or its id carries no `:`. An empty tenant still counts: a
    /// parent `:abc` yields the prefix `:`.
    pub fn inherited_prefix(&self) -> Option<&'a str> {
        let id = &self.parent.or(self.predecessor)?.0;
        id.find(':').map(|i| &id[..=i])
    }
}

/// Mints ids for a given [`IdKind`] within an [`IdContext`]. The default
/// ([`DefaultIdGen`]) follows ADR-0164 plus ADR-0211's prefix inheritance; an
/// embedder needing fleet-coordinated ids (a database sequence, a
/// Snowflake-style generator, or plain UUIDs) or its own tenant encoding
/// installs its own via [`EngineConfig::id_gen`][crate::EngineConfig::id_gen] —
/// `SessionId` staying an opaque `String` newtype is what makes any generator's
/// output valid. An override keeping the `{tenant}:` convention can reuse
/// [`IdContext::inherited_prefix`].
pub trait IdGen: Send + Sync {
    fn next(&self, kind: IdKind, ctx: &IdContext<'_>) -> String;
}

/// The ADR-0164 scheme. Its `(last_second, counter)` state is process-global
/// (not per-instance, via module statics) so "never twice" holds regardless of
/// how many `DefaultIdGen` values exist — every instance mints from the same
/// counter. The per-process salt is drawn once, lazily, on first use.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultIdGen;

impl DefaultIdGen {
    pub fn new() -> Self {
        Self
    }
}

static SALT: OnceLock<u8> = OnceLock::new();
static COUNTER: Mutex<(u64, u16)> = Mutex::new((0, 0));

/// Counter width: 3 hex chars, i.e. values `0..=0xfff` (4096 ids/s).
const COUNTER_MAX: u16 = 0xfff;

fn process_salt() -> u8 {
    *SALT.get_or_init(|| {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        (pid ^ nanos) as u8
    })
}

fn epoch_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_secs()
}

/// Advance the process-global counter and return `(second, counter)` for the
/// caller's id. Blocks — sleeping in short increments — only when a single
/// second's 4096-id budget is exhausted, so a caller waits for the next second
/// rather than reusing a counter value (the one place this generator can
/// block a hot path; unreachable outside a synthetic stress test).
fn next_tick() -> (u64, u16) {
    loop {
        let now = epoch_seconds();
        let mut state = COUNTER.lock().expect("id-gen counter lock poisoned");
        if state.0 != now {
            *state = (now, 0);
            return (now, 0);
        }
        if state.1 < COUNTER_MAX {
            state.1 += 1;
            return (now, state.1);
        }
        drop(state);
        std::thread::sleep(Duration::from_millis(5));
    }
}

impl IdGen for DefaultIdGen {
    fn next(&self, kind: IdKind, ctx: &IdContext<'_>) -> String {
        let (second, counter) = next_tick();
        format!(
            "{}{}-{:08x}{:02x}{:03x}",
            ctx.inherited_prefix().unwrap_or(""),
            kind.prefix(),
            second,
            process_salt(),
            counter
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_15_chars_kind_prefixed_and_hex_tailed() {
        let gen = DefaultIdGen::new();
        for (kind, prefix) in [
            (IdKind::Session, 's'),
            (IdKind::Job, 'j'),
            (IdKind::Request, 'r'),
            (IdKind::Output, 'o'),
            (IdKind::Script, 'x'),
        ] {
            let id = gen.next(kind, &IdContext::default());
            assert_eq!(id.len(), 15, "unexpected length for {id}");
            assert_eq!(id.chars().next(), Some(prefix));
            assert_eq!(id.chars().nth(1), Some('-'));
            assert!(
                id[2..].chars().all(|c| c.is_ascii_hexdigit()),
                "tail should be all hex: {id}"
            );
        }
    }

    #[test]
    fn ids_never_repeat_within_a_process() {
        let gen = DefaultIdGen::new();
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            assert!(
                seen.insert(gen.next(IdKind::Job, &IdContext::default())),
                "minted a duplicate id"
            );
        }
    }

    #[test]
    fn multiple_generator_instances_share_the_process_global_counter() {
        // Two independent `DefaultIdGen`s must never collide — the counter they
        // draw from is a module static, not instance state.
        let a = DefaultIdGen::new();
        let b = DefaultIdGen::new();
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(a.next(IdKind::Session, &IdContext::default())));
            assert!(seen.insert(b.next(IdKind::Session, &IdContext::default())));
        }
    }

    #[test]
    fn counter_exhaustion_waits_for_the_next_second_instead_of_repeating() {
        // Drives past the 4096/s budget at least once — the one branch of
        // `next_tick` that sleeps — and asserts it still never repeats.
        let gen = DefaultIdGen::new();
        let mut seen = HashSet::new();
        for _ in 0..(COUNTER_MAX as u32 + 10) {
            assert!(seen.insert(gen.next(IdKind::Request, &IdContext::default())));
        }
    }

    #[test]
    fn children_and_successors_inherit_the_tenant_prefix() {
        let gen = DefaultIdGen::new();
        let root = SessionId::new("t1:abc");
        let child = gen.next(IdKind::Session, &IdContext::child_of(&root));
        assert!(child.starts_with("t1:s-"), "{child}");
        assert_eq!(child.len(), "t1:".len() + 15);
        let successor = gen.next(IdKind::Session, &IdContext::successor_of(&root));
        assert!(successor.starts_with("t1:s-"), "{successor}");
    }

    #[test]
    fn prefix_is_up_to_the_first_colon_and_parent_wins() {
        let nested = SessionId::new("t1:t2:abc");
        let other = SessionId::new("t9:xyz");
        let ctx = IdContext {
            parent: Some(&nested),
            predecessor: Some(&other),
            user: None,
        };
        assert_eq!(ctx.inherited_prefix(), Some("t1:"));
        assert_eq!(
            IdContext::successor_of(&other).inherited_prefix(),
            Some("t9:")
        );
    }

    #[test]
    fn unprefixed_or_absent_lineage_keeps_the_plain_format() {
        let gen = DefaultIdGen::new();
        let plain = SessionId::new("s-6a708af0a1002");
        for ctx in [IdContext::default(), IdContext::child_of(&plain)] {
            let id = gen.next(IdKind::Session, &ctx);
            assert_eq!(id.len(), 15, "{id}");
            assert!(id.starts_with("s-"));
        }
    }
}
