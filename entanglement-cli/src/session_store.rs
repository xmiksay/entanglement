use anyhow::{Context, Result};
use entanglement_core::{InMsg, OutEvent, SessionId};
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
/// use entanglement_cli::session_store::safe_cwd_name;
/// assert_eq!(safe_cwd_name("/mnt/nvme/agent"), "mnt-nvme-agent");
/// assert_eq!(safe_cwd_name("/a-b"), "a-b");
/// assert_eq!(safe_cwd_name("C:\\Users\\test"), "C:-Users-test");
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

/// Payload of a log record: either an inbound message or outbound event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "direction", rename_all = "lowercase")]
pub enum LogPayload {
    In(InMsg),
    Out(OutEvent),
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
/// Tolerant of corrupt lines — skips lines that fail to parse.
#[allow(dead_code)]
pub fn read(cwd: &Path, root_session_id: &SessionId) -> Result<Vec<LogRecord>> {
    let path = session_path(cwd, root_session_id)?;
    let file = File::open(&path)
        .with_context(|| format!("Failed to open session file: {}", path.display()))?;

    let reader = BufReader::new(file);
    let mut records = Vec::new();

    for line in reader.lines() {
        let line = line.with_context(|| {
            format!("Failed to read line from session file: {}", path.display())
        })?;
        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<LogRecord>(&line) {
            Ok(record) => records.push(record),
            Err(e) => {
                tracing::warn!(
                    "Skipping corrupt line in {}: {} (line: {})",
                    path.display(),
                    e,
                    line
                );
            }
        }
    }

    Ok(records)
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
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        if !path.is_file() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }

        let file_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("Invalid session file name: {:?}", path))?;

        let session_id = SessionId::new(file_name);

        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("Failed to read metadata for: {}", path.display()))?;
        let last_active = metadata
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        let records = read(cwd, &session_id)?;
        let meta = records
            .first()
            .and_then(|r| match &r.payload {
                LogPayload::Out(OutEvent::SessionStarted {
                    profile,
                    model,
                    root,
                    ts,
                    ..
                }) => Some(SessionMeta {
                    id: session_id.clone(),
                    agent: profile.clone(),
                    model: model.clone(),
                    created: *ts,
                    last_active,
                    parent: None,
                    root: *root,
                }),
                _ => None,
            })
            .unwrap_or_else(|| SessionMeta {
                id: session_id.clone(),
                agent: "unknown".to_string(),
                model: None,
                created: last_active,
                last_active,
                parent: None,
                root: true,
            });

        sessions.push(meta);
    }

    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_dir_returns_data_dir_entanglement_sessions() {
        let dir = base_dir().expect("base_dir should succeed");
        assert!(dir.ends_with("entanglement/sessions"));
    }

    #[test]
    fn base_dir_creates_directory_if_missing() {
        let dir = base_dir().expect("base_dir should succeed");
        assert!(dir.exists(), "Base directory should exist");
        assert!(dir.is_dir(), "Base should be a directory");
    }

    #[test]
    fn safe_cwd_name_replaces_slashes() {
        assert_eq!(
            safe_cwd_name(Path::new("/mnt/nvme/agent")),
            "mnt-nvme-agent"
        );
        assert_eq!(safe_cwd_name(Path::new("/a/b/c")), "a-b-c");
    }

    #[test]
    fn safe_cwd_name_trims_leading_dash() {
        assert_eq!(safe_cwd_name(Path::new("/a-b")), "a-b");
        assert_eq!(safe_cwd_name(Path::new("///a")), "a");
    }

    #[test]
    fn safe_cwd_name_handles_windows_paths() {
        assert_eq!(safe_cwd_name(Path::new("C:\\Users\\test")), "C:-Users-test");
    }

    #[test]
    fn safe_cwd_name_preserves_spaces_and_unicode() {
        assert_eq!(safe_cwd_name(Path::new("/my path")), "my path");
        assert_eq!(safe_cwd_name(Path::new("/héllo/wørld")), "héllo-wørld");
    }

    #[test]
    fn append_and_read_roundtrip() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let cwd = temp_dir.path();
        let session_id = SessionId::new("test-session");

        let record1 = LogRecord::new(
            session_id.clone(),
            LogPayload::In(InMsg::Prompt {
                session: session_id.clone(),
                text: "hello".to_string(),
            }),
        );

        let record2 = LogRecord::new(
            session_id.clone(),
            LogPayload::Out(OutEvent::Done {
                session: session_id.clone(),
                seq: 1,
            }),
        );

        append(cwd, &session_id, &record1).expect("append should succeed");
        append(cwd, &session_id, &record2).expect("append should succeed");

        let records = read(cwd, &session_id).expect("read should succeed");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].session, session_id);
        assert_eq!(records[1].session, session_id);

        match &records[0].payload {
            LogPayload::In(InMsg::Prompt { text, .. }) => assert_eq!(text, "hello"),
            _ => panic!("Expected Prompt"),
        }

        match &records[1].payload {
            LogPayload::Out(OutEvent::Done { .. }) => {}
            _ => panic!("Expected Done"),
        }
    }

    #[test]
    fn read_skips_corrupt_lines() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let cwd = temp_dir.path();
        let session_id = SessionId::new("test-corrupt");

        let valid_record = LogRecord::new(
            session_id.clone(),
            LogPayload::In(InMsg::Prompt {
                session: session_id.clone(),
                text: "valid".to_string(),
            }),
        );

        append(cwd, &session_id, &valid_record).expect("append should succeed");

        let path = session_path(cwd, &session_id).expect("session_path should succeed");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("Failed to open file");
        writeln!(file, "{{invalid json}}").expect("Failed to write corrupt line");
        writeln!(file).expect("Failed to write empty line");

        let records = read(cwd, &session_id).expect("read should succeed");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn multi_session_interleaving() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let cwd = temp_dir.path();
        let root_id = SessionId::new("root-session");
        let sub_id = SessionId::new("sub-session");

        let root_record = LogRecord::new(
            root_id.clone(),
            LogPayload::In(InMsg::Prompt {
                session: root_id.clone(),
                text: "root".to_string(),
            }),
        );

        let sub_record = LogRecord::new(
            sub_id.clone(),
            LogPayload::In(InMsg::Prompt {
                session: sub_id.clone(),
                text: "sub".to_string(),
            }),
        );

        append(cwd, &root_id, &root_record).expect("append should succeed");
        append(cwd, &root_id, &sub_record).expect("append should succeed");

        let records = read(cwd, &root_id).expect("read should succeed");
        assert_eq!(records.len(), 2);

        let root_records: Vec<_> = records.iter().filter(|r| r.session == root_id).collect();
        let sub_records: Vec<_> = records.iter().filter(|r| r.session == sub_id).collect();

        assert_eq!(root_records.len(), 1);
        assert_eq!(sub_records.len(), 1);
    }
}
