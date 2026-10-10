//! Session listing: per-file [`SessionMeta`] derived from the log, the
//! first-prompt snippet, and parent/child navigation over the listing.

use std::path::Path;
use std::time::SystemTime;

use anyhow::{Context, Result};
use entanglement_core::{content_text, InMsg, OutEvent, SessionId};

use super::{read, session_dir, LogPayload};

/// Session metadata derived from the log file.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SessionMeta {
    /// Session ID.
    pub id: SessionId,
    /// Agent profile name.
    pub agent: String,
    /// Model name (if specified).
    pub model: Option<String>,
    /// Creation timestamp (from SessionStarted event).
    pub created: u64,
    /// Last active timestamp (from file mtime).
    pub last_active: u64,
    /// Parent session ID (None for root sessions).
    pub parent: Option<SessionId>,
    /// Whether this is a root session.
    pub root: bool,
    /// Truncated snippet of the first user prompt, for human-readable listings
    /// (#327). `None` when the log carries no `Prompt` (e.g. a session that only
    /// ever received tool results). See [`first_prompt_snippet`].
    pub first_prompt: Option<String>,
    /// Display name set via `SetSessionMeta`, from the log's last
    /// `SessionMetaChanged` record (last write wins — unlike `first_prompt`'s
    /// first-write scan). Preferred over `first_prompt` in listings when set.
    pub name: Option<String>,
}

/// Maximum length (in chars) of a [`SessionMeta::first_prompt`] snippet before
/// the trailing `…` ellipsis (#327).
pub(super) const FIRST_PROMPT_MAX: usize = 60;

/// Renders a first-prompt snippet for session listings (#327): the leading run
/// of `text`, cut at the first newline, then truncated to ~[`FIRST_PROMPT_MAX`]
/// chars on a word boundary, with a trailing `…` when anything was dropped.
pub fn first_prompt_snippet(text: &str) -> String {
    // A prompt often opens with a whole paragraph; the first raw line is the
    // label. Keep the untrimmed line to measure how much followed it.
    let raw_first_line = text.lines().next().unwrap_or("");
    let first_line = raw_first_line.trim();
    // Anything with content after the first line counts as dropped → ellipsis.
    let has_more = !text[raw_first_line.len()..].trim().is_empty();

    if first_line.chars().count() <= FIRST_PROMPT_MAX {
        return if has_more {
            format!("{first_line}…")
        } else {
            first_line.to_string()
        };
    }

    // Over the budget: cut at the last word boundary within the window so we
    // don't slice a word in half, falling back to a hard char cut if the first
    // word alone already overflows.
    let window: String = first_line.chars().take(FIRST_PROMPT_MAX).collect();
    let cut = window
        .rfind(char::is_whitespace)
        .map(|i| window[..i].trim_end().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or(window);
    format!("{cut}…")
}

/// Lists all sessions in the current working directory's session folder.
///
/// Reads the first line of each `.jsonl` file to extract metadata.
#[allow(dead_code)]
pub fn list_sessions(cwd: &Path) -> Result<Vec<SessionMeta>> {
    let dir = session_dir(cwd)?;
    let mut sessions = Vec::new();

    let entries = std::fs::read_dir(&dir)
        .with_context(|| format!("Failed to read session directory: {}", dir.display()))?;

    for entry in entries {
        // One unreadable/corrupt file must not abort listing of every other
        // session — skip it with a warning and carry on (#104).
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("Skipping unreadable session directory entry: {}", e);
                continue;
            }
        };
        let path = entry.path();

        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }

        let Some(file_name) = path.file_stem().and_then(|s| s.to_str()) else {
            tracing::warn!("Skipping session file with invalid name: {:?}", path);
            continue;
        };

        let session_id = SessionId::new(file_name);

        let last_active = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let records = match read(cwd, &session_id) {
            Ok(records) => records,
            Err(e) => {
                tracing::warn!("Skipping corrupt session file {}: {}", path.display(), e);
                continue;
            }
        };
        // The first record is now the opening `Prompt` (inbound logging landed
        // ahead of `SessionStarted`), so scan for the `SessionStarted` event
        // rather than assuming it's record zero. Capture the first user `Prompt`
        // snippet in the same pass — no extra I/O (#327).
        let mut started: Option<SessionMeta> = None;
        let mut first_prompt: Option<String> = None;
        let mut name: Option<String> = None;
        for r in &records {
            match &r.payload {
                LogPayload::Out(OutEvent::SessionStarted {
                    agent,
                    model,
                    root,
                    ts,
                    parent,
                    ..
                }) if started.is_none() => {
                    started = Some(SessionMeta {
                        id: session_id.clone(),
                        agent: agent.clone(),
                        model: model.clone(),
                        created: *ts,
                        last_active,
                        parent: parent.clone(),
                        root: *root,
                        first_prompt: None,
                        name: None,
                    });
                }
                LogPayload::In(InMsg::Prompt { content, .. }) if first_prompt.is_none() => {
                    let text = content_text(content);
                    if !text.trim().is_empty() {
                        first_prompt = Some(first_prompt_snippet(&text));
                    }
                }
                // Last write wins (do NOT copy first_prompt's first-write
                // guard): the record carries full merged state. A root log
                // interleaves spawned children's records, so only the root's
                // own metadata names this file's row.
                LogPayload::Out(OutEvent::SessionMetaChanged {
                    session,
                    name: new_name,
                    ..
                }) if *session == session_id => {
                    name = new_name.clone();
                }
                _ => {}
            }
        }

        let mut meta = started.unwrap_or_else(|| SessionMeta {
            id: session_id.clone(),
            agent: "unknown".to_string(),
            model: None,
            created: last_active,
            last_active,
            parent: None,
            root: true,
            first_prompt: None,
            name: None,
        });
        meta.first_prompt = first_prompt;
        meta.name = name;

        sessions.push(meta);
    }

    Ok(sessions)
}

/// Returns all child sessions of the given parent session ID.
#[allow(dead_code)]
pub fn children_of<'a>(sessions: &'a [SessionMeta], parent_id: &SessionId) -> Vec<&'a SessionMeta> {
    sessions
        .iter()
        .filter(|s| s.parent.as_ref() == Some(parent_id))
        .collect()
}

/// Returns the root session ID for the given session ID by walking up the parent chain.
/// Returns the session ID itself if it has no parent (is already a root).
#[allow(dead_code)]
pub fn root_of(sessions: &[SessionMeta], session_id: &SessionId) -> SessionId {
    let mut current_id = session_id.clone();
    let mut visited = std::collections::HashSet::new();

    loop {
        if !visited.insert(current_id.clone()) {
            tracing::warn!("Cycle detected in session parent chain for {}", current_id);
            return session_id.clone();
        }

        let session = sessions.iter().find(|s| s.id == current_id);
        match session.and_then(|s| s.parent.as_ref()) {
            Some(parent_id) => current_id = parent_id.clone(),
            None => return current_id,
        }
    }
}
