//! Locks: bytt's own (an flock) and Claude Code's (lock directories).
//!
//! Claude Code guards its token refresh and its `~/.claude.json` writes with the npm
//! `proper-lockfile` package: the lock is a directory, `mkdir` is the mutex, and a
//! holder that stops touching the directory's mtime is considered dead. Taking the
//! same locks means a swap can never land in the middle of Claude Code's own refresh.

use std::fs::{self, File};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

/// How long to wait for a lock Claude Code holds. It holds the credential lock for
/// one token request and the config lock for one file write.
const WAIT: Duration = Duration::from_secs(9);

/// A held `proper-lockfile` style directory lock, released on drop.
///
/// bytt holds these for local I/O only, well inside the staleness window, so it
/// does not need to keep the mtime fresh the way a long-lived holder would.
#[derive(Debug)]
pub struct DirLock {
    path: PathBuf,
}

impl DirLock {
    /// Take the lock at `path`, treating a holder older than `stale` as dead.
    pub fn acquire(path: PathBuf, stale: Duration) -> Result<Self> {
        Self::acquire_within(path, stale, WAIT)
    }

    fn acquire_within(path: PathBuf, stale: Duration, wait: Duration) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let start = Instant::now();
        loop {
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).with_context(|| format!("locking {}", path.display())),
            }
            if is_stale(&path, stale) && fs::remove_dir(&path).is_ok() {
                continue;
            }
            if start.elapsed() >= wait {
                bail!(
                    "{} is held, Claude Code is probably refreshing its login. Try again in a few seconds.",
                    path.display()
                );
            }
            sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for DirLock {
    fn drop(&mut self) {
        let _ = fs::remove_dir(&self.path);
    }
}

fn is_stale(path: &Path, stale: Duration) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > stale)
}

/// bytt's own lock: one process at a time may change accounts, stored logins or state.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_lock_excludes_and_releases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        let held = DirLock::acquire(path.clone(), Duration::from_secs(60)).unwrap();
        let second = DirLock::acquire_within(path.clone(), Duration::from_secs(60), Duration::ZERO);
        assert!(second.is_err());
        drop(held);
        assert!(!path.exists());
        DirLock::acquire(path, Duration::from_secs(60)).unwrap();
    }

    #[test]
    fn dir_lock_takes_over_a_dead_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.lock");
        fs::create_dir(&path).unwrap();
        sleep(Duration::from_millis(30));
        DirLock::acquire_within(path, Duration::from_millis(10), Duration::ZERO).unwrap();
    }
}
