//! Short, sortable, kind-tagged ids (ADR-0164) — the engine's one built-in
//! generator; ids are not pluggable (#928).
//!
//! Format: `<kind>-<epoch-seconds hex><2-hex process salt><3-hex counter>` —
//! 15 characters, e.g. `s-6a708af0a1002`. Seconds (not milliseconds) keep the
//! timestamp a fixed 8 hex chars without wrapping until 2106, so ids sort
//! lexicographically by creation time; the trailing counter restores
//! sub-second ordering and — because it is monotonic, not random — guarantees
//! a process never mints the same id twice.
//!
//! A mint may name a lineage — the parent a sub-agent is spawned under, or the
//! session a successor/fork continues (ADR-0211) — whose tenant prefix the new
//! id keeps: a child or compaction successor of `t1:s-…` is minted `t1:s-…`.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    /// tail/byte cap (#608, ADR-0161 §7) — a sub-agent answer that
    /// truncated, paged the rest of the way via `poll`.
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

/// Mint a fresh id of `kind`. `lineage` is the session the new one belongs
/// to — a sub-agent's parent, or the session a compaction successor or fork
/// continues — and `None` for a fresh root, a job or a request id. The new id
/// keeps the lineage's tenant prefix (everything up to and including its
/// first `:`, the `{tenant}:{id}` convention of
/// docs/embedding/tenants-and-heads.md §1); an empty tenant still counts, so
/// a lineage `:abc` yields the prefix `:`.
pub fn mint_id(kind: IdKind, lineage: Option<&SessionId>) -> String {
    let (second, counter) = next_tick();
    format!(
        "{}{}-{:08x}{:02x}{:03x}",
        lineage.and_then(tenant_prefix).unwrap_or(""),
        kind.prefix(),
        second,
        process_salt(),
        counter
    )
}

fn tenant_prefix(id: &SessionId) -> Option<&str> {
    id.0.find(':').map(|i| &id.0[..=i])
}

// Process-global (module statics, not per-caller state), so "never twice"
// holds for every mint in the process. The salt is drawn once, lazily.
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
/// rather than reusing a counter value (the one place minting can block a hot
/// path; unreachable outside a synthetic stress test).
fn next_tick() -> (u64, u16) {
    loop {
        let now = epoch_seconds();
        let mut state = COUNTER.lock().expect("id counter lock poisoned");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_15_chars_kind_prefixed_and_hex_tailed() {
        for (kind, prefix) in [
            (IdKind::Session, 's'),
            (IdKind::Job, 'j'),
            (IdKind::Request, 'r'),
            (IdKind::Output, 'o'),
            (IdKind::Script, 'x'),
        ] {
            let id = mint_id(kind, None);
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
        let mut seen = HashSet::new();
        for _ in 0..2000 {
            assert!(
                seen.insert(mint_id(IdKind::Job, None)),
                "minted a duplicate id"
            );
        }
    }

    #[test]
    fn counter_exhaustion_waits_for_the_next_second_instead_of_repeating() {
        // Drives past the 4096/s budget at least once — the one branch of
        // `next_tick` that sleeps — and asserts it still never repeats.
        let mut seen = HashSet::new();
        for _ in 0..(COUNTER_MAX as u32 + 10) {
            assert!(seen.insert(mint_id(IdKind::Request, None)));
        }
    }

    #[test]
    fn a_lineage_passes_on_its_tenant_prefix() {
        let root = SessionId::new("t1:abc");
        let id = mint_id(IdKind::Session, Some(&root));
        assert!(id.starts_with("t1:s-"), "{id}");
        assert_eq!(id.len(), "t1:".len() + 15);
    }

    #[test]
    fn prefix_is_up_to_the_first_colon_and_an_empty_tenant_counts() {
        let nested = SessionId::new("t1:t2:abc");
        assert_eq!(tenant_prefix(&nested), Some("t1:"));
        assert_eq!(tenant_prefix(&SessionId::new(":abc")), Some(":"));
    }

    #[test]
    fn unprefixed_or_absent_lineage_keeps_the_plain_format() {
        let plain = SessionId::new("s-6a708af0a1002");
        for lineage in [None, Some(&plain)] {
            let id = mint_id(IdKind::Session, lineage);
            assert_eq!(id.len(), 15, "{id}");
            assert!(id.starts_with("s-"));
        }
    }
}
