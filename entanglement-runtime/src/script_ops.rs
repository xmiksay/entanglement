//! Background-script registry shared by `rhai` (the launcher) and `poll` (the
//! joiner, #637, ADR-0185). A `rhai` call with `background: true` registers
//! here owned by the launching session and returns immediately with an `x-`
//! handle (ADR-0164); the detached script's `print` output streams into the
//! entry and `poll` drains it incrementally — the same destructive-delta
//! contract as a `j-` job handle.
//!
//! Deliberately its own registry rather than a task-backed
//! [`crate::host::jobs::JobRegistry`] variant: a script is an in-process
//! `spawn_blocking` task, not an OS process — there is no process group to
//! SIGKILL, no exit code, and no stdout/stderr pipes to drain. "Kill" here is
//! the **cooperative** stop flag the engine's progress callback polls (#167):
//! a `poll` with `kill: true` trips it and the script terminates at its next
//! engine operation — which means an in-flight `exec`/`bash` binding call
//! runs to its own budget-clamped timeout first (the documented ADR-0161 §5
//! limit this design accepts).
//!
//! Buffering, waiting, ownership, and eviction all mirror `JobRegistry`
//! deliberately (#605/#621): a capped buffer dropping the oldest bytes, a
//! [`Notify`] woken on new output and on finish so a poll can wait instead of
//! busy-draining, owner-scoped visibility where a wrong owner is
//! indistinguishable from an unknown handle, and a lazy TTL/count sweep of
//! finished entries from `register`/`poll`. Running scripts are never evicted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;

use entanglement_core::{DefaultIdGen, IdGen, IdKind, SessionId};

/// Retention cap for a not-yet-polled script's output — same bound and same
/// keep-the-tail policy as a job's per-stream buffer.
const MAX_SCRIPT_BUFFER: usize = 256 * 1024;
/// How long a finished entry survives before eviction.
const SCRIPT_TTL: Duration = Duration::from_secs(15 * 60);
/// Hard cap on finished entries, independent of the TTL.
const MAX_FINISHED_SCRIPTS: usize = 200;

#[derive(Default)]
struct ScriptState {
    output: Vec<u8>,
    /// Bytes dropped from the front of the buffer since the last poll because
    /// the cap was hit — surfaced so a poll never silently loses output.
    dropped: u64,
    finished: bool,
    /// The script ended in an error (an uncaught exception, the deadline, or
    /// the stop flag) — mirrors the blocking path's `is_error` (ADR-0176).
    is_error: bool,
    /// Terminated by the cooperative stop flag (a `poll` `kill: true`), not by
    /// its own completion or deadline.
    stopped: bool,
    /// Terminated by the engine-enforced wall-clock deadline.
    timed_out: bool,
    /// Set alongside `finished` — the eviction clock starts here.
    finished_at: Option<Instant>,
}

/// One registered background script. The launcher holds an `Arc` to stream
/// output and record the terminal state; the registry holds another for
/// `poll`/listing.
pub struct ScriptOp {
    label: String,
    /// The session that launched this script — `None` leaves it visible to any
    /// poller (mirrors an ownerless job).
    owner: Option<SessionId>,
    /// The same cooperative stop flag the engine's progress callback polls —
    /// tripping it is the only kill primitive an in-process task has.
    stop: Arc<AtomicBool>,
    timeout: Duration,
    started: Instant,
    state: Mutex<ScriptState>,
    /// Woken on every appended chunk and on finish, so a poll can wait.
    notify: Notify,
}

impl ScriptOp {
    /// Stream a chunk of script output (a `print` line, or the final result
    /// line) into the buffer, waking any parked poll.
    pub fn append_output(&self, text: &str) {
        {
            let mut st = self.state.lock().expect("script state poisoned");
            st.output.extend_from_slice(text.as_bytes());
            if st.output.len() > MAX_SCRIPT_BUFFER {
                let overflow = st.output.len() - MAX_SCRIPT_BUFFER;
                st.output.drain(0..overflow);
                st.dropped += overflow as u64;
            }
        }
        self.notify.notify_waiters();
    }

    /// Record the engine-enforced deadline firing — called from the progress
    /// callback so the terminal state can distinguish a timeout from an
    /// ordinary script error.
    pub fn mark_timed_out(&self) {
        self.state.lock().expect("script state poisoned").timed_out = true;
    }

    /// Record the terminal state: append the final result/error line, flip
    /// `finished`, and wake any parked poll. `stopped` is derived here — an
    /// errored script whose stop flag is set (and whose deadline never fired)
    /// was killed cooperatively.
    pub fn finish(&self, final_text: &str, is_error: bool) {
        {
            let mut st = self.state.lock().expect("script state poisoned");
            st.output.extend_from_slice(final_text.as_bytes());
            if st.output.len() > MAX_SCRIPT_BUFFER {
                let overflow = st.output.len() - MAX_SCRIPT_BUFFER;
                st.output.drain(0..overflow);
                st.dropped += overflow as u64;
            }
            st.finished = true;
            st.is_error = is_error;
            st.stopped = is_error && !st.timed_out && self.stop.load(Ordering::SeqCst);
            st.finished_at = Some(Instant::now());
        }
        self.notify.notify_waiters();
    }
}

/// A single `poll` read: the output accumulated since the previous poll plus
/// the current status.
pub struct ScriptPoll {
    pub label: String,
    pub running: bool,
    pub is_error: bool,
    pub stopped: bool,
    pub timed_out: bool,
    pub output: Vec<u8>,
    pub dropped: u64,
    pub timeout_secs: u64,
}

/// One entry in a pending-operations listing (#607, ADR-0161 §6) — ownerless
/// entries are never listed, mirroring [`crate::host::jobs::JobOpInfo`].
pub struct ScriptOpInfo {
    pub session: SessionId,
    pub handle: String,
    pub label: String,
    pub running: bool,
    pub elapsed: Duration,
}

/// Shared, cheaply-cloned registry of background scripts. One instance is
/// built at startup and handed to both the `rhai` launcher and the `poll`
/// joiner.
#[derive(Clone)]
pub struct ScriptRegistry {
    inner: Arc<Inner>,
}

struct Inner {
    scripts: Mutex<HashMap<String, Arc<ScriptOp>>>,
    /// Mints `x-` script handles (ADR-0164).
    id_gen: Arc<dyn IdGen>,
}

impl Default for ScriptRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Inner {
                scripts: Mutex::new(HashMap::new()),
                id_gen: Arc::new(DefaultIdGen::new()),
            }),
        }
    }
}

impl ScriptRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a background script owned by `owner`, returning its handle and
    /// the entry the launcher streams output into. `stop` is the cooperative
    /// kill flag shared with the engine's progress callback; `timeout` is the
    /// engine-enforced deadline, kept here only for the poll header.
    pub fn register(
        &self,
        label: String,
        owner: Option<SessionId>,
        timeout: Duration,
        stop: Arc<AtomicBool>,
    ) -> (String, Arc<ScriptOp>) {
        self.evict_expired();
        let op = Arc::new(ScriptOp {
            label,
            owner,
            stop,
            timeout,
            started: Instant::now(),
            state: Mutex::new(ScriptState::default()),
            notify: Notify::new(),
        });
        let id = self.inner.id_gen.next(IdKind::Script);
        self.inner
            .scripts
            .lock()
            .expect("script registry poisoned")
            .insert(id.clone(), op.clone());
        (id, op)
    }

    /// Poll a script for output since the last poll, waiting up to
    /// `timeout_secs` (`0` = unbounded) for new output or finish. `caller`
    /// must own the script (or it must be ownerless) or this returns `None` —
    /// indistinguishable from an unknown handle. `kill` trips the cooperative
    /// stop flag and returns immediately with whatever is buffered — the
    /// script terminates at its next engine operation, so a later poll (not
    /// this one) reports the killed terminal state.
    pub async fn poll(
        &self,
        id: &str,
        caller: &SessionId,
        kill: bool,
        timeout_secs: u64,
    ) -> Option<ScriptPoll> {
        self.evict_expired();
        let op = self
            .inner
            .scripts
            .lock()
            .expect("script registry poisoned")
            .get(id)
            .cloned()?;
        if !owner_allows(&op.owner, caller) {
            return None;
        }
        if kill {
            op.stop.store(true, Ordering::SeqCst);
            return Some(snapshot(&op));
        }
        let deadline =
            (timeout_secs != 0).then(|| Instant::now() + Duration::from_secs(timeout_secs));
        loop {
            // Registered *before* the ready-check (the standard `Notify`
            // pattern) so a notification fired between the check and the
            // `.await` below is never missed.
            let notified = op.notify.notified();
            if has_new(&op) {
                break;
            }
            match deadline {
                None => notified.await,
                Some(dl) => {
                    let remaining = dl.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        break;
                    }
                    let _ = tokio::time::timeout(remaining, notified).await;
                }
            }
        }
        Some(snapshot(&op))
    }

    /// Snapshot every entry for a pending-operations listing, optionally
    /// scoped to one session. Ownerless entries are never listed. Sorted by
    /// handle for a deterministic reply.
    pub fn snapshot_ops(&self, session: Option<&SessionId>) -> Vec<ScriptOpInfo> {
        let scripts = self.inner.scripts.lock().expect("script registry poisoned");
        let mut list: Vec<ScriptOpInfo> = scripts
            .iter()
            .filter_map(|(handle, op)| {
                let owner = op.owner.as_ref()?;
                if session.is_some_and(|s| s != owner) {
                    return None;
                }
                Some(ScriptOpInfo {
                    session: owner.clone(),
                    handle: handle.clone(),
                    label: op.label.clone(),
                    running: !op.state.lock().expect("script state poisoned").finished,
                    elapsed: op.started.elapsed(),
                })
            })
            .collect();
        list.sort_by(|a, b| a.handle.cmp(&b.handle));
        list
    }

    /// Remove finished entries past [`SCRIPT_TTL`], then trim to
    /// [`MAX_FINISHED_SCRIPTS`] (oldest-finished first). Running scripts are
    /// never touched. Called lazily from `register`/`poll` (#621).
    fn evict_expired(&self) {
        let now = Instant::now();
        let mut scripts = self.inner.scripts.lock().expect("script registry poisoned");
        scripts.retain(|_, op| {
            let st = op.state.lock().expect("script state poisoned");
            match st.finished_at {
                Some(at) => now.duration_since(at) < SCRIPT_TTL,
                None => true,
            }
        });
        let mut finished: Vec<(String, Instant)> = scripts
            .iter()
            .filter_map(|(id, op)| {
                let st = op.state.lock().expect("script state poisoned");
                st.finished_at.map(|at| (id.clone(), at))
            })
            .collect();
        if finished.len() > MAX_FINISHED_SCRIPTS {
            finished.sort_by_key(|(_, at)| *at);
            for (id, _) in &finished[..finished.len() - MAX_FINISHED_SCRIPTS] {
                scripts.remove(id);
            }
        }
    }
}

/// Whether `caller` may read a script owned by `owner` — an ownerless entry is
/// visible to anyone; an owned one only to its owner (mirrors jobs, #605).
fn owner_allows(owner: &Option<SessionId>, caller: &SessionId) -> bool {
    owner.as_ref().is_none_or(|o| o == caller)
}

/// Whether `op` has something a poll hasn't seen yet: unconsumed output or a
/// terminal status. Peeks without draining.
fn has_new(op: &ScriptOp) -> bool {
    let st = op.state.lock().expect("script state poisoned");
    !st.output.is_empty() || st.finished
}

/// Drain `op`'s buffer into a [`ScriptPoll`] snapshot (`mem::take`, so a poll
/// is destructive and incremental).
fn snapshot(op: &ScriptOp) -> ScriptPoll {
    let mut st = op.state.lock().expect("script state poisoned");
    ScriptPoll {
        label: op.label.clone(),
        running: !st.finished,
        is_error: st.is_error,
        stopped: st.stopped,
        timed_out: st.timed_out,
        output: std::mem::take(&mut st.output),
        dropped: std::mem::take(&mut st.dropped),
        timeout_secs: op.timeout.as_secs(),
    }
}

#[cfg(test)]
mod tests;
