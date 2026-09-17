//! Shared `ModeTable` test fixture (ADR-0207 stage 4) — see `main.rs`'s
//! `mod mode_support` doc comment for when to reach for this vs. a real
//! built-in mode.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::{Permission, SessionId};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};

/// A single mode named `"build"` (matching `entanglement_core::DEFAULT_MODE`,
/// so a session that never sends `SetMode` resolves against it) with
/// `default: Permission::Allow` and no rules — every call, of every tool,
/// runs unprompted. The direct analog of the pre-ADR-0207 `build` agent's
/// `default: allow` profile these test helpers were built around.
pub fn allow_all_table() -> Arc<ModeTable> {
    let mode = Mode {
        name: "build".to_string(),
        default: Permission::Allow,
        rules: Rules::default(),
        limits: Limits::default(),
        sandbox: None,
        sandbox_network: false,
    };
    Arc::new(ModeTable::new(vec![mode]).expect("single-mode table is valid"))
}

/// An empty permission-mode map — `ProfileResolver` folds `OutEvent::ModeChanged`
/// into this live, so an empty map here is fine: every session starts in
/// `DEFAULT_MODE` ("build") and core emits `ModeChanged` before the first
/// `ToolExec`, folding it in before any call is ever graded.
pub fn perm_modes() -> Arc<Mutex<HashMap<SessionId, String>>> {
    Arc::new(Mutex::new(HashMap::new()))
}
