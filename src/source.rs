//! The only place that opens mail sources. Everything here is read-only.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// A whole source file captured in memory under a shared lock.
#[derive(Debug)]
pub struct Snapshot {
    pub bytes: Vec<u8>,
    pub dev: u64,
    pub ino: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    /// Size or times moved while we held the lock, i.e. a writer that ignores fcntl locks.
    pub changed_during_read: bool,
}

fn open_readonly(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true)
        .custom_flags(libc::O_NOATIME | libc::O_CLOEXEC);
    match opts.open(path) {
        // O_NOATIME is refused for files we don't own.
        Err(e) if e.raw_os_error() == Some(libc::EPERM) => OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(path),
        r => r,
    }
}

/// Whole-file fcntl read lock, as Dovecot's default `mbox_read_locks = fcntl` takes.
fn try_read_lock(file: &File) -> io::Result<bool> {
    let fl = libc::flock {
        l_type: libc::F_RDLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
        l_start: 0,
        l_len: 0,
        l_pid: 0,
    };
    // SAFETY: valid fd and a fully initialised flock struct.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &fl) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(libc::EAGAIN) | Some(libc::EACCES) => Ok(false),
        _ => Err(err),
    }
}

fn jitter() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    Duration::from_micros(100_000 + u64::from(nanos % 100_000))
}

fn time_ns(sec: i64, nsec: i64) -> i64 {
    sec.saturating_mul(1_000_000_000).saturating_add(nsec)
}

/// Read an mbox (or any single-file source) the way a Dovecot reader would:
/// shared fcntl lock, read everything, close. Closing the fd drops the lock,
/// so the file must not be opened elsewhere in this process meanwhile.
pub fn read_locked(path: &Path, timeout: Duration) -> io::Result<Snapshot> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut file = open_readonly(path)?;
        while !try_read_lock(&file)? {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("timed out waiting for read lock on {}", path.display()),
                ));
            }
            sleep(jitter());
        }

        let before = file.metadata()?;
        // Dovecot reopens if the path was replaced while it waited; so do we.
        let current = std::fs::metadata(path)?;
        if (current.dev(), current.ino()) != (before.dev(), before.ino()) {
            continue;
        }

        let mut bytes = Vec::with_capacity(before.len() as usize);
        file.read_to_end(&mut bytes)?;
        let after = file.metadata()?;
        drop(file);

        let changed_during_read = bytes.len() as u64 != before.len()
            || after.len() != before.len()
            || after.mtime() != before.mtime()
            || after.mtime_nsec() != before.mtime_nsec()
            || after.ctime() != before.ctime()
            || after.ctime_nsec() != before.ctime_nsec();

        return Ok(Snapshot {
            bytes,
            dev: before.dev(),
            ino: before.ino(),
            mtime_ns: time_ns(before.mtime(), before.mtime_nsec()),
            ctime_ns: time_ns(before.ctime(), before.ctime_nsec()),
            changed_during_read,
        });
    }
}
