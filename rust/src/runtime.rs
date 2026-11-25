//! Advisory locks for the daemon and microphone. Keep the files in place so
//! processes always lock the same inodes. The open handles own the leases.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::Path;

pub fn lock_runtime(data_dir: &Path) -> io::Result<File> {
    lock(
        data_dir,
        "runtime.lock",
        "a Voiceless runtime is already active for this data directory",
    )
}

pub(crate) fn lock_capture() -> io::Result<File> {
    lock(
        &crate::models::default_data_dir(),
        "capture.lock",
        "Voiceless is already recording with this data directory",
    )
}

fn lock(data_dir: &Path, name: &str, busy: &str) -> io::Result<File> {
    fs::create_dir_all(data_dir)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(data_dir.join(name))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(io::Error::new(io::ErrorKind::WouldBlock, busy)),
        Err(TryLockError::Error(error)) => Err(error),
    }
}
