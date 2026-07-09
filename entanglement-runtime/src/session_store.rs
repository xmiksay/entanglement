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
        // rather than assuming it's record zero.
        let meta = records
            .iter()
            .find_map(|r| match &r.payload {
                LogPayload::Out(OutEvent::SessionStarted {
                    profile,
                    model,
                    root,
                    ts,
                    parent,
                    ..
                }) => Some(SessionMeta {
                    id: session_id.clone(),
                    agent: profile.clone(),
                    model: model.clone(),
                    created: *ts,
                    last_active,
                    parent: parent.clone(),
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
    fn pair_records_associates_each_prompt_with_following_events() {
        let sid = SessionId::new("s");
        let prompt = |t: &str| {
            LogRecord::new(
                sid.clone(),
                LogPayload::In(InMsg::Prompt {
                    session: sid.clone(),
                    text: t.to_string(),
                }),
            )
        };
        let text = |seq: u64, t: &str| {
            LogRecord::new(
                sid.clone(),
                LogPayload::Out(OutEvent::TextDelta {
                    session: sid.clone(),
                    seq,
                    text: t.to_string(),
                }),
            )
        };
        let done = |seq: u64| {
            LogRecord::new(
                sid.clone(),
                LogPayload::Out(OutEvent::Done {
                    session: sid.clone(),
                    seq,
                }),
            )
        };

        let records = vec![
            prompt("hi"),
            text(1, "hello"),
            done(2),
            prompt("again"),
            text(3, "yo"),
            done(4),
        ];

        let paired = pair_records(&records);
        assert_eq!(paired.len(), 4);

        // First prompt pairs with the first out event; it's consumed so the
        // trailing events of that turn pair with `None`.
        match &paired[0] {
            (Some(InMsg::Prompt { text, .. }), OutEvent::TextDelta { .. }) => {
                assert_eq!(text, "hi")
            }
            other => panic!("unexpected pairing: {other:?}"),
        }
        assert!(matches!(paired[1], (None, OutEvent::Done { .. })));
        match &paired[2] {
            (Some(InMsg::Prompt { text, .. }), OutEvent::TextDelta { .. }) => {
                assert_eq!(text, "again")
            }
            other => panic!("unexpected pairing: {other:?}"),
        }
        assert!(matches!(paired[3], (None, OutEvent::Done { .. })));
    }

    #[test]
    fn pair_records_drops_trailing_inbound_without_output() {
        let sid = SessionId::new("s");
        let records = vec![LogRecord::new(
            sid.clone(),
            LogPayload::In(InMsg::Prompt {
                session: sid.clone(),
                text: "no reply yet".to_string(),
            }),
        )];
        assert!(pair_records(&records).is_empty());
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
    fn read_tolerates_truncated_tail_line() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let cwd = temp_dir.path();
        let session_id = SessionId::new("test-tail");

        append(
            cwd,
            &session_id,
            &LogRecord::new(
                session_id.clone(),
                LogPayload::In(InMsg::Prompt {
                    session: session_id.clone(),
                    text: "kept".to_string(),
                }),
            ),
        )
        .expect("append should succeed");

        // Simulate a crash mid-append: a partial final line, no trailing newline.
        let path = session_path(cwd, &session_id).expect("session_path should succeed");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("Failed to open file");
        write!(file, "{{\"ts\":123,\"sess").expect("Failed to write truncated line");
        drop(file);

        let records = read(cwd, &session_id).expect("truncated tail is tolerated");
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn read_rejects_interior_corruption() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let cwd = temp_dir.path();
        let session_id = SessionId::new("test-interior");

        append(
            cwd,
            &session_id,
            &LogRecord::new(
                session_id.clone(),
                LogPayload::In(InMsg::Prompt {
                    session: session_id.clone(),
                    text: "one".to_string(),
                }),
            ),
        )
        .expect("append should succeed");

        // Garbage in the middle...
        let path = session_path(cwd, &session_id).expect("session_path should succeed");
        let mut file = OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("Failed to open file");
        writeln!(file, "{{ not json").expect("Failed to write corrupt line");
        drop(file);

        // ...followed by a valid record → a hole, not a truncated tail.
        append(
            cwd,
            &session_id,
            &LogRecord::new(
                session_id.clone(),
                LogPayload::Out(OutEvent::Done {
                    session: session_id.clone(),
                    seq: 1,
                }),
            ),
        )
        .expect("append should succeed");

        let err = read(cwd, &session_id).expect_err("interior corruption must error");
        assert!(
            err.to_string().contains("Interior corruption"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn integrity_gap_detects_and_sums_tombstones() {
        let sid = SessionId::new("s");
        let records = vec![
            LogRecord::new(sid.clone(), LogPayload::Gap { dropped: 4 }),
            LogRecord::new(
                sid.clone(),
                LogPayload::Out(OutEvent::Done {
                    session: sid.clone(),
                    seq: 1,
                }),
            ),
            LogRecord::new(sid.clone(), LogPayload::Gap { dropped: 6 }),
        ];
        assert_eq!(integrity_gap(&records), Some(10));
    }

    #[test]
    fn integrity_gap_none_for_clean_log() {
        let sid = SessionId::new("s");
        let records = vec![LogRecord::new(
            sid.clone(),
            LogPayload::Out(OutEvent::Done {
                session: sid.clone(),
                seq: 1,
            }),
        )];
        assert_eq!(integrity_gap(&records), None);
    }

    #[test]
    fn list_sessions_skips_one_bad_file() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let cwd = temp_dir.path();

        let started = |id: &SessionId, ts: u64| {
            LogRecord::new(
                id.clone(),
                LogPayload::Out(OutEvent::SessionStarted {
                    session: id.clone(),
                    parent: None,
                    profile: "build".to_string(),
                    model: None,
                    root: true,
                    ts,
                }),
            )
        };

        let good = SessionId::new("good");
        append(cwd, &good, &started(&good, 1000)).expect("append should succeed");

        // A file with interior corruption: read() errors, so listing must skip it
        // rather than abort the whole enumeration.
        let bad = SessionId::new("bad");
        append(cwd, &bad, &started(&bad, 2000)).expect("append should succeed");
        let bad_path = session_path(cwd, &bad).expect("session_path should succeed");
        let mut f = OpenOptions::new()
            .append(true)
            .open(&bad_path)
            .expect("Failed to open file");
        writeln!(f, "GARBAGE mid-file").expect("Failed to write corrupt line");
        drop(f);
        append(
            cwd,
            &bad,
            &LogRecord::new(
                bad.clone(),
                LogPayload::Out(OutEvent::Done {
                    session: bad.clone(),
                    seq: 1,
                }),
            ),
        )
        .expect("append should succeed");

        let sessions = list_sessions(cwd).expect("list_sessions should skip the bad file");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, good);
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

    #[test]
    fn children_of_finds_direct_children() {
        let root_id = SessionId::new("root");
        let child1_id = SessionId::new("child1");
        let child2_id = SessionId::new("child2");
        let grandchild_id = SessionId::new("grandchild");

        let sessions = vec![
            SessionMeta {
                id: root_id.clone(),
                agent: "build".to_string(),
                model: None,
                created: 0,
                last_active: 0,
                parent: None,
                root: true,
            },
            SessionMeta {
                id: child1_id.clone(),
                agent: "build".to_string(),
                model: None,
                created: 0,
                last_active: 0,
                parent: Some(root_id.clone()),
                root: false,
            },
            SessionMeta {
                id: child2_id.clone(),
                agent: "build".to_string(),
                model: None,
                created: 0,
                last_active: 0,
                parent: Some(root_id.clone()),
                root: false,
            },
            SessionMeta {
                id: grandchild_id.clone(),
                agent: "build".to_string(),
                model: None,
                created: 0,
                last_active: 0,
                parent: Some(child1_id.clone()),
                root: false,
            },
        ];

        let children = children_of(&sessions, &root_id);
        assert_eq!(children.len(), 2);
        assert!(children.iter().any(|s| s.id == child1_id));
        assert!(children.iter().any(|s| s.id == child2_id));

        let grandchildren = children_of(&sessions, &child1_id);
        assert_eq!(grandchildren.len(), 1);
        assert_eq!(grandchildren[0].id, grandchild_id);
    }

    #[test]
    fn root_of_walks_up_parent_chain() {
        let root_id = SessionId::new("root");
        let child1_id = SessionId::new("child1");
        let child2_id = SessionId::new("child2");

        let sessions = vec![
            SessionMeta {
                id: root_id.clone(),
                agent: "build".to_string(),
                model: None,
                created: 0,
                last_active: 0,
                parent: None,
                root: true,
            },
            SessionMeta {
                id: child1_id.clone(),
                agent: "build".to_string(),
                model: None,
                created: 0,
                last_active: 0,
                parent: Some(root_id.clone()),
                root: false,
            },
            SessionMeta {
                id: child2_id.clone(),
                agent: "build".to_string(),
                model: None,
                created: 0,
                last_active: 0,
                parent: Some(child1_id.clone()),
                root: false,
            },
        ];

        assert_eq!(root_of(&sessions, &root_id), root_id);
        assert_eq!(root_of(&sessions, &child1_id), root_id);
        assert_eq!(root_of(&sessions, &child2_id), root_id);
    }

    #[test]
    fn root_of_returns_self_for_orphan_session() {
        let orphan_id = SessionId::new("orphan");

        let sessions = vec![SessionMeta {
            id: orphan_id.clone(),
            agent: "build".to_string(),
            model: None,
            created: 0,
            last_active: 0,
            parent: None,
            root: true,
        }];

        assert_eq!(root_of(&sessions, &orphan_id), orphan_id);
    }

    #[test]
    fn forward_compatible_multi_session_log_rebuilds_tree() {
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let cwd = temp_dir.path();
        let root_id = SessionId::new("root");
        let child1_id = SessionId::new("child1");
        let child2_id = SessionId::new("child2");

        let root_started = LogRecord::new(
            root_id.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: root_id.clone(),
                parent: None,
                profile: "build".to_string(),
                model: None,
                root: true,
                ts: 1000,
            }),
        );

        let child1_started = LogRecord::new(
            child1_id.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: child1_id.clone(),
                parent: Some(root_id.clone()),
                profile: "build".to_string(),
                model: None,
                root: false,
                ts: 2000,
            }),
        );

        let child2_started = LogRecord::new(
            child2_id.clone(),
            LogPayload::Out(OutEvent::SessionStarted {
                session: child2_id.clone(),
                parent: Some(root_id.clone()),
                profile: "build".to_string(),
                model: None,
                root: false,
                ts: 3000,
            }),
        );

        append(cwd, &root_id, &root_started).expect("append should succeed");
        append(cwd, &root_id, &child1_started).expect("append should succeed");
        append(cwd, &root_id, &child2_started).expect("append should succeed");

        let records = read(cwd, &root_id).expect("read should succeed");
        assert_eq!(records.len(), 3);

        let sessions = list_sessions(cwd).expect("list_sessions should succeed");
        assert_eq!(sessions.len(), 1, "Only root session file exists");

        let root_meta = sessions
            .iter()
            .find(|s| s.id == root_id)
            .expect("root should exist");
        assert_eq!(root_meta.parent, None);
        assert!(root_meta.root);
    }
}
