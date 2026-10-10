//! File-revert outcome types (#731, ADR-0212): what the runtime's
//! `EngineConfig::file_reverter` seam returns for an
//! [`InMsg::RevertFiles`][super::InMsg::RevertFiles], and what
//! [`OutEvent::FilesReverted`][super::OutEvent::FilesReverted] carries.

use serde::{Deserialize, Serialize};

/// One file a revert left alone, and why — e.g. the user edited it since the
/// agent did (restoring would clobber their work), its backup was skipped for
/// size, or the backup blob is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevertSkip {
    pub path: String,
    pub reason: String,
}

/// The outcome of one revert. `to_turn` is the turn the session's files were
/// reverted to — the request's own value, or the resolved one for an "undo"
/// (`to_turn: None`). `restored` lists the paths put back (rewritten to their
/// earlier content, or deleted when the agent had created them); `skipped`
/// the paths that could not be, each with its reason.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRevertReport {
    pub to_turn: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restored: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<RevertSkip>,
}
