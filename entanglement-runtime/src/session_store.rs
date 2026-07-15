use anyhow::{Context, Result};
use entanglement_core::{content_text, InMsg, OutEvent, SessionId};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Returns the base data directory for entanglement session storage.
///
/// This is `<data_dir>/entanglement/sessions`, creating it if it doesn't exist.
///
/// # Errors
///
/// Returns an error if:
/// - The data directory cannot be determined
/// - The directory cannot be created
pub fn base_dir() -> Result<PathBuf> {
    let data_dir = dirs::data_dir().context("Failed to determine data directory")?;

    let base = data_dir.join("entanglement/sessions");

    if !base.exists() {
        std::fs::create_dir_all(&base)
            .with_context(|| format!("Failed to create sessions directory: {}", base.display()))?;
    }

    Ok(base)
}

/// Sanitizes a path for safe filesystem use.
///
/// Replaces `/` and `\` with `-`, trims leading `-`.
/// Leaves all other bytes as-is (including spaces and Unicode).
///
/// # Known limitation
///
/// This is not collision-proof. Two distinct paths can map to the same folder
/// (e.g., `/a-b` and `/a/b`). A future hash-suffix disambiguator can be added
/// without breaking reads.
///
/// # Examples
///
/// ```
/// use std::path::Path;
/// use entanglement_runtime::session_store::safe_cwd_name;
/// assert_eq!(safe_cwd_name(Path::new("/mnt/nvme/agent")), "mnt-nvme-agent");
/// assert_eq!(safe_cwd_name(Path::new("/a-b")), "a-b");
/// assert_eq!(safe_cwd_name(Path::new("C:\\Users\\test")), "C:-Users-test");
/// ```
pub fn safe_cwd_name(cwd: &Path) -> String {
    let path_str = cwd.to_string_lossy();
    let mut result = path_str.replace(['/', '\\'], "-");
    result = result.trim_start_matches('-').to_string();
    result
}

/// Returns the session directory for a given current working directory.
pub fn session_dir(cwd: &Path) -> Result<PathBuf> {
    let base = base_dir()?;
    let safe_name = safe_cwd_name(cwd);
    let dir = base.join(&safe_name);

    if !dir.exists() {
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("Failed to create session directory: {}", dir.display()))?;
    }

    Ok(dir)
}

/// Returns the path to a session's JSONL file.
pub fn session_path(cwd: &Path, root_session_id: &SessionId) -> Result<PathBuf> {
    let dir = session_dir(cwd)?;
    Ok(dir.join(format!("{}.jsonl", root_session_id.0)))
}

/// Payload of a log record: either an inbound message, an outbound event, or a
/// gap tombstone.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "lowercase")]
pub enum LogPayload {
    In(InMsg),
    Out(OutEvent),
    /// Tombstone marking that the persistence recorder lagged and dropped a
    /// contiguous run of `dropped` broadcast records before this point (#104).
    /// It is not a real message — a poison marker. The file stays well-formed,
    /// so without it `Session::replay` would silently fold an incomplete
    /// history (e.g. a `ToolCall` missing its `ToolOutput`) into a wrong
    /// `Context`. [`integrity_gap`] detects it so resume refuses.
    Gap {
        dropped: u64,
    },
}

/// A single record in the session log.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogRecord {
    /// Unix timestamp in milliseconds.
    pub ts: u64,
    /// The session this record belongs to.
    pub session: SessionId,
    /// The actual message/event payload.
    pub payload: LogPayload,
}

impl LogRecord {
    /// Creates a new log record with the current timestamp.
    pub fn new(session: SessionId, payload: LogPayload) -> Self {
        let ts = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        Self {
            ts,
            session,
            payload,
        }
    }
}

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
}

/// Maximum length (in chars) of a [`SessionMeta::first_prompt`] snippet before
/// the trailing `…` ellipsis (#327).
const FIRST_PROMPT_MAX: usize = 60;

/// Renders a first-prompt snippet for session listings (#327): the leading run
/// of `text`, cut at the first newline, then truncated to ~[`FIRST_PROMPT_MAX`]
/// chars on a word boundary, with a trailing `…` when anything was dropped.
fn first_prompt_snippet(text: &str) -> String {
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

/// Appends a log record to a session file.
///
/// Creates the file if it doesn't exist.
pub fn append(cwd: &Path, root_session_id: &SessionId, record: &LogRecord) -> Result<()> {
    let path = session_path(cwd, root_session_id)?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;

    let line = serde_json::to_string(record).context("Failed to serialize log record")?;
    writeln!(file, "{}", line)
        .with_context(|| format!("Failed to write to session file: {}", path.display()))?;

    Ok(())
}

/// Reads all records from a session file.
///
/// Distinguishes a crash-truncated *tail* (tolerated) from *interior*
/// corruption (surfaced as an error) — see #104. A partial final line is the
/// expected result of a crash mid-`append`, so it is skipped with a warning.
/// An unparseable line with any parseable line after it is a hole (partial
/// write, bit flip) that replay would silently fold over into a wrong
/// `Context`, so it aborts the read instead. Blank lines are noise and are
/// ignored without affecting tail/interior detection.
///
/// Gap tombstones ([`LogPayload::Gap`]) parse cleanly and are returned like any
/// other record; [`integrity_gap`] inspects them so resume can refuse.
#[allow(dead_code)]
pub fn read(cwd: &Path, root_session_id: &SessionId) -> Result<Vec<LogRecord>> {
    let path = session_path(cwd, root_session_id)?;
    let file = File::open(&path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;

    let reader = BufReader::new(file);
    let mut records = Vec::new();
    // An unparseable line is only tolerable as the file's tail. Hold it here
    // until we know whether another (non-blank) line follows: if one does, the
    // held line was interior corruption, not a truncated tail.
    let mut pending_corrupt: Option<String> = None;

    for line in reader.lines() {
        let line = line.with_context(|| {
            format!("Failed to read line from session file: {}", path.display())
        })?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(bad) = pending_corrupt.take() {
            return Err(anyhow::anyhow!(
                "Interior corruption in session file {}: unparseable line followed by more \
                 records — the log has a hole and cannot be safely replayed (bad line: {})",
                path.display(),
                bad
            ));
        }

        match serde_json::from_str::<LogRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) => pending_corrupt = Some(line),
        }
    }

    if let Some(bad) = pending_corrupt {
        tracing::warn!(
            "Tolerating truncated final line in {} (likely a crash mid-append): {}",
            path.display(),
            bad
        );
    }

    Ok(records)
}

/// Total records the recorder dropped, if the log carries any [`LogPayload::Gap`]
/// tombstone — `None` for an intact log. Callers about to resume must refuse a
/// `Some(_)`: a gap means a contiguous run of events is missing, so replay would
/// silently reconstruct a wrong `Context` (#104).
#[allow(dead_code)]
pub fn integrity_gap(records: &[LogRecord]) -> Option<u64> {
    let mut dropped = 0u64;
    let mut any = false;
    for record in records {
        if let LogPayload::Gap { dropped: n } = &record.payload {
            any = true;
            dropped = dropped.saturating_add(*n);
        }
    }
    any.then_some(dropped)
}

/// Pairs a log's records into the `(Option<InMsg>, OutEvent)` tuples that
/// [`entanglement_core::Holly::resume`] / `Session::replay` expect.
///
/// Each `Out` record is paired with the most recent preceding `In` record (the
/// message that produced it); the `In` is then consumed so it pairs with exactly
/// one `Out`. `In` records with no following `Out` are dropped — replay folds
/// state from events, so an unanswered inbound message carries nothing to restore.
pub fn pair_records(records: &[LogRecord]) -> Vec<(Option<InMsg>, OutEvent)> {
    let mut paired: Vec<(Option<InMsg>, OutEvent)> = Vec::new();
    let mut last_in: Option<InMsg> = None;

    for record in records {
        match &record.payload {
            LogPayload::In(in_msg) => {
                last_in = Some(in_msg.clone());
            }
            LogPayload::Out(out_event) => {
                paired.push((last_in.take(), out_event.clone()));
            }
            // A gap tombstone carries no state to restore. Resume paths call
            // `integrity_gap` and refuse before reaching here; this arm only
            // keeps pairing total-ordered if a caller pairs a gapped log anyway.
            LogPayload::Gap { .. } => {}
        }
    }

    paired
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
        for r in &records {
            match &r.payload {
                LogPayload::Out(OutEvent::SessionStarted {
                    profile,
                    model,
                    root,
                    ts,
                    parent,
                    ..
                }) if started.is_none() => {
                    started = Some(SessionMeta {
                        id: session_id.clone(),
                        agent: profile.clone(),
                        model: model.clone(),
                        created: *ts,
                        last_active,
                        parent: parent.clone(),
                        root: *root,
                        first_prompt: None,
                    });
                }
                LogPayload::In(InMsg::Prompt { content, .. }) if first_prompt.is_none() => {
                    let text = content_text(content);
                    if !text.trim().is_empty() {
                        first_prompt = Some(first_prompt_snippet(&text));
                    }
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
        });
        meta.first_prompt = first_prompt;

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

#[cfg(test)]
mod tests_log;
#[cfg(test)]
mod tests_sessions;
