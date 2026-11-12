//! Small filesystem helpers shared by the store and the Claude Code config writer.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::{Context, Result};

/// Replace `path` with `data` in one step: readers see the old file or the new one,
/// never half of either. The file is private to the user (0600).
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    // A symlinked file (dotfiles repo) must stay a symlink, so write through it.
    let target = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .with_context(|| format!("{} has no file name", target.display()))?;
    let tmp = target.with_file_name(format!(".{name}.bytt-{}", std::process::id()));

    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, &target)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("writing {}", path.display()))
}
