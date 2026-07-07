use anyhow::{Context, Result};
use std::path::PathBuf;

/// Returns the base data directory for entanglement session storage.
///
/// This is `~/.entanglement` (user home directory), creating it if it doesn't exist.
///
/// # Errors
///
/// Returns an error if:
/// - The home directory cannot be determined
/// - The directory cannot be created
#[allow(dead_code)]
pub fn base_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Failed to determine home directory")?;

    let base = home.join(".entanglement");

    if !base.exists() {
        std::fs::create_dir_all(&base)
            .with_context(|| format!("Failed to create base directory: {}", base.display()))?;
    }

    Ok(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_dir_returns_home_dot_entanglement() {
        let dir = base_dir().expect("base_dir should succeed");
        assert!(dir.ends_with(".entanglement"));
    }

    #[test]
    fn base_dir_creates_directory_if_missing() {
        let dir = base_dir().expect("base_dir should succeed");
        assert!(dir.exists(), "Base directory should exist");
        assert!(dir.is_dir(), "Base should be a directory");
    }
}
