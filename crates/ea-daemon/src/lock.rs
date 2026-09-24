//! One daemon per state directory.
//!
//! Nothing else in this process tree is safe to run twice. Two daemons sharing
//! a state directory share the SQLite file, the session budget and the
//! connectors' credentials, and — because [`ipc::Server::spawn`] unlinks a
//! stale socket unconditionally — the second one silently takes the control
//! socket away from the first. The first keeps polling, keeps triaging, keeps
//! spending sessions and keeps executing approved actions; it simply stops
//! being reachable. `ea status` then answers from the newcomer and looks
//! healthy, which is the worst possible presentation of "there are two of me".
//!
//! An advisory `flock(2)` on a file in the state directory is the conventional
//! shape and the right one here:
//!
//! * the kernel releases it when the process dies, however it dies, so there
//!   is no stale-lock problem to reason about after a crash or a `kill -9`;
//! * it is per open file description, so it also catches the case of one
//!   process trying to start twice;
//! * it costs nothing and needs no cleanup.
//!
//! The pid inside the file is *advice for the human*, not the mechanism: the
//! lock is held by the open descriptor, and the number is written only so the
//! error message can name the process to stop. A file whose contents are
//! unreadable or nonsense still refuses the second start.
//!
//! [`ipc::Server::spawn`]: crate::ipc::Server::spawn

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};

/// The lockfile's name inside the state directory.
pub const LOCK_FILE: &str = "daemon.lock";

/// A held single-instance lock. Dropping it (or exiting, however abruptly)
/// releases it.
#[derive(Debug)]
pub struct InstanceLock {
    /// Holding the `File` is what holds the lock: the flock lives on this open
    /// file description and is released when the descriptor closes.
    file: File,
    path: PathBuf,
}

impl InstanceLock {
    /// Take the lock, or explain who has it.
    ///
    /// The file is deliberately *not* removed on release. Unlinking it would
    /// open a race in which a third process creates a new file, locks that,
    /// and both believe they are the only daemon — the classic lockfile bug
    /// that flock exists to avoid. A leftover zero-byte file is harmless: the
    /// next acquire truncates it and writes its own pid.
    pub fn acquire(path: &Path) -> anyhow::Result<Self> {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("opening the lockfile {}", path.display()))?;

        // SAFETY: `file` owns a valid open descriptor for the whole call, and
        // flock(2) has no other precondition. LOCK_NB makes this a try-lock:
        // a daemon that cannot have the state directory must say so and exit,
        // never block waiting for the other one to finish.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if locked != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                bail!(
                    "another ea-daemon is already running ({}) and holds {}; \
                     stop it before starting a second one \
                     (`launchctl unload ~/Library/LaunchAgents/dev.gustaf.exec-agent.plist`), \
                     or point this one at a different EA_STATE_DIR",
                    describe_holder(path),
                    path.display(),
                );
            }
            return Err(err).with_context(|| format!("locking {}", path.display()));
        }

        // Ours. Record the pid for whoever reads the next refusal message.
        // Best effort by design: failing to write the hint must not stop a
        // daemon that legitimately holds the lock.
        if let Err(err) = write_pid(&mut file) {
            tracing::warn!(
                path = %path.display(),
                error = %format!("{err:#}"),
                "could not record this daemon's pid in the lockfile"
            );
        }

        tracing::info!(path = %path.display(), pid = std::process::id(), "single-instance lock held");
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // Closing the descriptor releases the flock; this is only here so the
        // file is not left holding a pid that no longer exists.
        let _ = self.file.set_len(0);
    }
}

fn write_pid(file: &mut File) -> anyhow::Result<()> {
    file.set_len(0)?;
    use std::io::Seek;
    file.rewind()?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(())
}

/// `pid 1234`, or an honest admission that the file did not say.
fn describe_holder(path: &Path) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => match text.trim().parse::<u32>() {
            Ok(pid) => format!("pid {pid}"),
            Err(_) => "pid unknown".to_string(),
        },
        Err(_) => "pid unknown".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    /// The whole point: a second daemon must refuse, and say which process to
    /// stop. `flock` is per open file description, so two acquires in one
    /// process exercise exactly the same kernel path two processes would.
    #[test]
    fn a_second_instance_is_refused_and_named() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(LOCK_FILE);

        let _first = InstanceLock::acquire(&path).expect("the first instance must get the lock");
        let err = InstanceLock::acquire(&path)
            .expect_err("a second instance must not get the lock")
            .to_string();

        assert!(err.contains("already running"), "{err}");
        assert!(
            err.contains(&format!("pid {}", std::process::id())),
            "the refusal must name the holder: {err}"
        );
        assert!(err.contains(LOCK_FILE), "{err}");
    }

    #[test]
    fn releasing_the_lock_lets_the_next_instance_start() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(LOCK_FILE);

        let first = InstanceLock::acquire(&path).unwrap();
        drop(first);

        InstanceLock::acquire(&path).expect("a released lock must be re-acquirable");
    }

    #[test]
    fn the_lockfile_records_the_holders_pid() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(LOCK_FILE);
        let _lock = InstanceLock::acquire(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            std::process::id().to_string()
        );
    }

    /// A leftover file from a previous run — with a pid that is long gone —
    /// must not stop a daemon starting. The kernel already released the lock.
    #[test]
    fn a_stale_lockfile_from_a_dead_process_does_not_block_startup() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(LOCK_FILE);
        std::fs::write(&path, "999999").unwrap();
        InstanceLock::acquire(&path).expect("a stale lockfile must not block startup");
    }

    #[test]
    fn the_lockfile_is_owner_only() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join(LOCK_FILE);
        let _lock = InstanceLock::acquire(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "the lockfile must not be group- or world-readable"
        );
    }
}
