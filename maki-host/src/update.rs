use std::path::PathBuf;

use maki_storage::{StateDir, StorageError};

const BACKUP_FILENAME: &str = "maki_backup";

/// Where `maki update` keeps the binary it replaced, for `maki rollback`.
pub fn backup_path() -> Result<PathBuf, StorageError> {
    Ok(StateDir::resolve()?.path().join(BACKUP_FILENAME))
}
