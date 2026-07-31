//! File timestamp utilities for the file-touch gate (ADR-0142).

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

/// Get a file's modification timestamp in milliseconds since Unix epoch.
/// Returns `Ok(None)` if the file doesn't exist; `Err` for other errors.
pub fn get_file_mtime(path: &Path) -> Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }

    let metadata = fs::metadata(path)
        .with_context(|| format!("failed to read metadata for {}", path.display()))?;

    let modified = metadata
        .modified()
        .with_context(|| format!("failed to get modification time for {}", path.display()))?;

    // Convert SystemTime to milliseconds since Unix epoch
    let duration = modified
        .duration_since(std::time::UNIX_EPOCH)
        .with_context(|| format!("modification time before Unix epoch for {}", path.display()))?;

    Ok(Some(duration.as_millis() as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_get_file_mtime_existing() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "test content").unwrap();
        file.flush().unwrap();

        let mtime = get_file_mtime(file.path()).unwrap();
        assert!(mtime.is_some());
        assert!(mtime.unwrap() > 0);
    }

    #[test]
    fn test_get_file_mtime_nonexistent() {
        let mtime = get_file_mtime(Path::new("/nonexistent/file/path")).unwrap();
        assert!(mtime.is_none());
    }

    #[test]
    fn test_get_file_mtime_updates() {
        let mut file = NamedTempFile::new().unwrap();
        writeln!(file, "initial content").unwrap();
        file.flush().unwrap();

        let mtime1 = get_file_mtime(file.path()).unwrap().unwrap();

        // Wait a bit and modify
        std::thread::sleep(std::time::Duration::from_millis(10));
        writeln!(file, "more content").unwrap();
        file.flush().unwrap();

        let mtime2 = get_file_mtime(file.path()).unwrap().unwrap();

        // mtime should have increased
        assert!(mtime2 > mtime1);
    }
}
