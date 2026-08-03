//! Short, sortable, kind-tagged ids behind a pluggable generator (ADR-0164).
//!
//! Format: `<kind>-<epoch-seconds hex><2-hex process salt><3-hex counter>` —
//! 15 characters, e.g. `s-6a708af0a1002`. Seconds (not milliseconds) keep the
//! timestamp a fixed 8 hex chars without wrapping until 2106, so ids sort
//! lexicographically by creation time; the trailing counter restores
//! sub-second ordering and — because it is monotonic, not random — guarantees
//! a process never mints the same id twice.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
}

impl IdKind {
    fn prefix(self) -> char {
        match self {
            IdKind::Session => 's',
            IdKind::Job => 'j',
            IdKind::Request => 'r',
        }
    }
}

/// Mints ids for a given [`IdKind`]. The default ([`DefaultIdGen`]) follows
/// ADR-0164; an embedder needing fleet-coordinated ids (a database sequence, a
/// Snowflake-style generator, or plain UUIDs) installs its own via
/// [`EngineConfig::id_gen`][crate::EngineConfig::id_gen] — `SessionId` staying
/// an opaque `String` newtype is what makes any generator's output valid.
pub trait IdGen: Send + Sync {
    fn next(&self, kind: IdKind) -> String;
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
    fn next(&self, kind: IdKind) -> String {
        let (second, counter) = next_tick();
        format!(
            "{}-{:08x}{:02x}{:03x}",
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
        ] {
            let id = gen.next(kind);
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
            assert!(seen.insert(gen.next(IdKind::Job)), "minted a duplicate id");
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
            assert!(seen.insert(a.next(IdKind::Session)));
            assert!(seen.insert(b.next(IdKind::Session)));
        }
    }

    #[test]
    fn counter_exhaustion_waits_for_the_next_second_instead_of_repeating() {
        // Drives past the 4096/s budget at least once — the one branch of
        // `next_tick` that sleeps — and asserts it still never repeats.
        let gen = DefaultIdGen::new();
        let mut seen = HashSet::new();
        for _ in 0..(COUNTER_MAX as u32 + 10) {
            assert!(seen.insert(gen.next(IdKind::Request)));
        }
    }
}
