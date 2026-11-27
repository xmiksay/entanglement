//! Shared `ModeTable` test fixture (ADR-0207 stage 4) — see `main.rs`'s
//! `mod mode_support` doc comment for when to reach for this vs. a real
//! built-in mode.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use entanglement_core::{Permission, SessionId};
use entanglement_runtime::mode::{Limits, Mode, ModeTable, Rules};
use entanglement_runtime::tools::{ArgKind, PermissionScope};

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

/// An empty permission-mode map — `ModeResolver` folds `OutEvent::ModeChanged`
/// into this live, so an empty map here is fine: every session starts in
/// `DEFAULT_MODE` ("build") and core emits `ModeChanged` before the first
/// `ToolExec`, folding it in before any call is ever graded.
pub fn perm_modes() -> Arc<Mutex<HashMap<SessionId, String>>> {
    Arc::new(Mutex::new(HashMap::new()))
}

/// The scope a stub standing in for `bash` declares, so scoped `bash(...)`
/// and `bash{...}` rules grade it like the real tool: the registered tool
/// answers, never its name (#962).
pub fn command_scope() -> PermissionScope {
    PermissionScope {
        arg: ArgKind::Command,
        workdir: true,
    }
}
