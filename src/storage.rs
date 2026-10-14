//! Durable replacement and OS-owned locks for xses state.
use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

pub struct Lock {
    _file: File,
}

impl Lock {
    /// The lock file is persistent. Unlinking it would allow two processes to
    /// lock different inodes. The OS releases ownership on exit, even on crash.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        fs::create_dir_all(path.parent().context("lock has no parent")?)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
        }
    }
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    fs::create_dir_all(parent)?;
    let (temporary, mut file) = loop {
        let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".xses-{}-{n}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&temporary) {
            Ok(file) => break (Temporary(temporary), file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary.0, path)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locks_exclude_other_handles_and_release_without_unlinking() {
        let path = std::env::temp_dir().join(format!("xses-lock-test-{}.lock", std::process::id()));
        let lock = Lock::try_acquire(&path).unwrap().unwrap();
        assert!(Lock::try_acquire(&path).unwrap().is_none());
        drop(lock);
        assert!(path.exists());
        let second = Lock::try_acquire(&path).unwrap().unwrap();
        drop(second);
        fs::remove_file(path).unwrap();
    }
    #[test]
    fn replacing_an_existing_file_leaves_complete_content() {
        let path =
            std::env::temp_dir().join(format!("xses-atomic-test-{}.json", std::process::id()));
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new complete content").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new complete content");
        fs::remove_file(path).unwrap();
    }
}
