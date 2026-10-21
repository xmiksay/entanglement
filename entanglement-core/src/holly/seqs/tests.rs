use super::*;

fn sid(s: &str) -> SessionId {
    SessionId::new(s)
}

fn next(reg: &SeqRegistry, s: &str) -> u64 {
    reg.with_next(&sid(s), |seq| seq)
}

/// Register and retire `id` at once, dropping the task's own handle.
fn retire_fresh(reg: &SeqRegistry, id: &SessionId) {
    drop(reg.register(id, 1));
    reg.retire(id);
}

#[test]
fn a_retired_session_keeps_minting_above_its_last_seq() {
    let reg = SeqRegistry::default();
    let _cell = reg.register(&sid("a"), 0);
    assert_eq!((next(&reg, "a"), next(&reg, "a")), (1, 2));
    reg.retire(&sid("a"));
    assert!(!reg.is_live(&sid("a")));
    assert_eq!((next(&reg, "a"), next(&reg, "a")), (3, 4));
}

#[test]
fn a_resume_gets_the_retired_cell_back_above_both_marks() {
    let reg = SeqRegistry::default();
    let first = reg.register(&sid("a"), 5);
    reg.retire(&sid("a"));
    assert_eq!(next(&reg, "a"), 6);
    // The log only knew seq 5: the seed is lower than the retired cell.
    let resumed = reg.register(&sid("a"), 5);
    assert!(Arc::ptr_eq(&first, &resumed), "one cell per id");
    assert!(reg.is_live(&sid("a")));
    assert!(!reg.lock().retired.contains_key(&sid("a")));
    assert_eq!(next(&reg, "a"), 7);
    // A log ahead of the cell wins too.
    reg.retire(&sid("a"));
    let _ahead = reg.register(&sid("a"), 20);
    assert_eq!(next(&reg, "a"), 21);
}

#[test]
fn an_unknown_id_mints_zero_and_leaves_no_entry() {
    let reg = SeqRegistry::default();
    assert_eq!(next(&reg, "ghost"), 0);
    assert_eq!(next(&reg, "ghost"), 0);
    assert!(reg.lock().retired.is_empty());
    assert!(!reg.is_live(&sid("ghost")));
    // A lease of a never-started id pins nothing and creates nothing.
    assert!(!reg.lease(&sid("ghost")).is_pinning());
    assert_eq!(next(&reg, "ghost"), 0);
}

#[test]
fn the_retired_map_is_bounded_and_evicts_the_least_recently_touched() {
    let reg = SeqRegistry::default();
    retire_fresh(&reg, &sid("s0"));
    retire_fresh(&reg, &sid("s1"));
    // An emit touches s0, so s1 is now the one touched longest ago.
    next(&reg, "s0");
    for i in 2..=RETIRED_CAP {
        retire_fresh(&reg, &sid(&format!("s{i}")));
    }
    let seqs = reg.lock();
    assert_eq!(seqs.retired.len(), RETIRED_CAP);
    assert!(seqs.retired.contains_key(&sid("s0")), "touched, so kept");
    assert!(!seqs.retired.contains_key(&sid("s1")), "oldest, so evicted");
}

/// #891: a leased id outlives any number of other retirements and keeps
/// minting above its last seq.
#[test]
fn a_leased_id_survives_past_the_cap_and_keeps_its_sequence() {
    let reg = SeqRegistry::default();
    let cell = reg.register(&sid("bg"), 0);
    next(&reg, "bg");
    let lease = reg.lease(&sid("bg"));
    reg.retire(&sid("bg"));
    drop(cell);
    for i in 0..=RETIRED_CAP {
        retire_fresh(&reg, &sid(&format!("s{i}")));
    }
    assert_eq!(
        reg.lock().retired.len(),
        RETIRED_CAP,
        "unpinned still bound"
    );
    assert_eq!(next(&reg, "bg"), 2, "leased, so never evicted");
    // Released: the next touch reclaims it like any other entry.
    drop(lease);
    for i in 0..=RETIRED_CAP {
        retire_fresh(&reg, &sid(&format!("t{i}")));
    }
    assert_eq!(next(&reg, "bg"), 0, "evicted once the lease is gone");
}

/// With every retired entry pinned the map grows past the cap rather than
/// evicting a counter still in use, and shrinks back once the pins go.
#[test]
fn all_pinned_grows_past_the_cap_then_shrinks_back() {
    let reg = SeqRegistry::default();
    let mut leases = Vec::new();
    for i in 0..=RETIRED_CAP + 2 {
        let id = sid(&format!("p{i}"));
        drop(reg.register(&id, 1));
        leases.push(reg.lease(&id));
        reg.retire(&id);
    }
    assert_eq!(reg.lock().retired.len(), RETIRED_CAP + 3);
    assert!((0..=RETIRED_CAP + 2).all(|i| next(&reg, &format!("p{i}")) == 2));
    leases.clear();
    retire_fresh(&reg, &sid("last"));
    let seqs = reg.lock();
    assert_eq!(seqs.retired.len(), RETIRED_CAP);
    assert!(seqs.retired.contains_key(&sid("last")), "newest kept");
}

/// A lease taken while live follows the id through retire and resume: the
/// resumed task emits through the very cell the lease pins.
#[test]
fn a_lease_spans_a_resume() {
    let reg = SeqRegistry::default();
    let task_a = reg.register(&sid("s"), 3);
    let lease = reg.lease(&sid("s"));
    reg.retire(&sid("s"));
    drop(task_a);
    let task_b = reg.register(&sid("s"), 1);
    let pinned = lease
        .0
        .as_ref()
        .expect("a started id's lease pins its cell");
    assert!(Arc::ptr_eq(pinned, &task_b));
    assert_eq!(next(&reg, "s"), 4);
}

/// A lease taken after the session already retired (a hibernate landing
/// before the detached work starts) pins the retired cell just the same.
#[test]
fn a_lease_taken_after_retire_pins_the_retired_cell() {
    let reg = SeqRegistry::default();
    drop(reg.register(&sid("bg"), 0));
    next(&reg, "bg");
    reg.retire(&sid("bg"));
    let lease = reg.lease(&sid("bg"));
    assert!(lease.is_pinning());
    for i in 0..=RETIRED_CAP {
        retire_fresh(&reg, &sid(&format!("s{i}")));
    }
    assert_eq!(next(&reg, "bg"), 2, "pinned, so never evicted");
    drop(lease);
}
