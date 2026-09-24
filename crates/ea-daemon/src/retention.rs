//! The retention pass: the scheduled job that keeps this daemon's footprint
//! bounded rather than merely slow-growing.
//!
//! Two halves, because there are two things that grow without limit.
//!
//! * **The database.** [`RetentionStore::prune`] deletes terminal rows past
//!   their window; that module owns the policy and, more importantly, the
//!   exclusions — an action a human has not decided and the conversation the
//!   owner is talking in are never pruned at any age.
//! * **The launchd logs.** `StandardOutPath` and `StandardErrorPath` in the
//!   plist are plain files that `launchd` appends to forever; there is no
//!   built-in rotation, and `newsyslog` would need root. So the daemon rotates
//!   its own, here.
//!
//! # Why copy-then-truncate rather than rename
//!
//! `launchd` opens those files once, before `exec`, and hands the daemon the
//! descriptors as fd 1 and 2. Renaming the file does not move the descriptor:
//! the daemon would go on writing into the renamed file, and the fresh one
//! would stay empty until the next restart — rotation that silently loses
//! every subsequent line, which is precisely the class of failure this review
//! is about. Copying the contents aside and then truncating the original
//! **in place** keeps the same inode, so the descriptor the daemon is holding
//! keeps working and the next line lands at the top of an empty file.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use ea_core::store::retention::{PruneSummary, RetentionPolicy, RetentionStore};

use crate::scheduler::Job;

/// The name `ea status` shows and `ea resume <job>` takes.
pub const RETENTION_JOB: &str = "retention";

/// How often the pass runs when nothing says otherwise.
///
/// Daily. The windows are measured in months, so running more often would
/// delete the same nothing repeatedly; running less often would let the logs
/// overshoot their cap between passes. It also means the first pass happens
/// within a second of startup (the scheduler runs every job on its first
/// tick), so a daemon that has been restarted after a long outage tidies up
/// immediately rather than a day later.
pub const DEFAULT_RETENTION_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Rotate a launchd log once it passes this size.
///
/// 8 MiB. At `RUST_LOG=info` this daemon writes on the order of a few hundred
/// bytes per poll and a few lines per triage pass — call it a megabyte a week
/// in ordinary operation — so 8 MiB is around two months of history in the
/// live file, and a crash-looping connector that writes far more still cannot
/// run away. With one kept generation per stream the whole ceiling is four
/// files at 8 MiB: 32 MiB, bounded and predictable.
pub const DEFAULT_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// The files `scripts/install-launchd.sh` points `StandardOutPath` and
/// `StandardErrorPath` at, relative to the state directory.
pub const LAUNCHD_LOGS: &[&str] = &["daemon.out.log", "daemon.err.log"];

/// Everything one retention pass needs.
pub struct RetentionDeps {
    pub store: RetentionStore,
    pub policy: RetentionPolicy,
    /// Where the launchd logs live: the state directory.
    pub log_dir: PathBuf,
    pub log_max_bytes: u64,
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetentionSummary {
    pub pruned: PruneSummary,
    /// Log files rotated this pass.
    pub rotated: Vec<PathBuf>,
}

/// One retention pass: prune the database, then rotate the logs.
///
/// The log rotation is not allowed to fail the pass. A read-only log directory
/// or a file somebody has moved is a nuisance; letting it count as a job
/// failure would eventually trip the breaker and stop the pruning too, which
/// is the more important half.
pub fn run_retention(deps: &RetentionDeps) -> anyhow::Result<RetentionSummary> {
    let pruned = deps
        .store
        .prune(&deps.policy, Utc::now())
        .context("pruning old rows")?;
    if pruned.total() > 0 {
        tracing::info!(?pruned, "retention: pruned old rows");
    }

    let mut rotated = Vec::new();
    for name in LAUNCHD_LOGS {
        let path = deps.log_dir.join(name);
        match rotate_if_large(&path, deps.log_max_bytes) {
            Ok(true) => {
                tracing::info!(log = %path.display(), "rotated a full log file");
                rotated.push(path);
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(
                log = %path.display(),
                error = %format!("{err:#}"),
                "could not rotate this log file"
            ),
        }
    }

    Ok(RetentionSummary { pruned, rotated })
}

/// Rotate `path` to `path.1` and empty it, if it is larger than `max_bytes`.
/// Returns whether it rotated. A file that does not exist is not an error:
/// running outside `launchd` (a smoke test, a terminal) simply has no such
/// log.
pub fn rotate_if_large(path: &Path, max_bytes: u64) -> anyhow::Result<bool> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };
    if !metadata.is_file() || metadata.len() <= max_bytes {
        return Ok(false);
    }

    let previous = previous_generation(path);
    std::fs::copy(path, &previous)
        .with_context(|| format!("copying {} to {}", path.display(), previous.display()))?;

    // In place, on the same inode: see the module docs. `set_len(0)` on a
    // handle opened for writing truncates without replacing the file, so the
    // descriptor launchd gave this process as stdout/stderr keeps working.
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("opening {} to truncate it", path.display()))?
        .set_len(0)
        .with_context(|| format!("truncating {}", path.display()))?;

    Ok(true)
}

/// `daemon.err.log` -> `daemon.err.log.1`. Only one generation is kept: the
/// previous file is overwritten, which is what bounds the total.
fn previous_generation(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// The scheduler job wrapper.
///
/// Registered like any other job, so it is visible in `ea status`, contained
/// by the same circuit breaker, and stopped by `ea pause` — a retention pass
/// is a write, and "stop doing things" has to mean this too.
pub fn retention_job(interval: Duration, deps: Arc<RetentionDeps>) -> Job {
    Job::new(RETENTION_JOB, interval, move || {
        let deps = Arc::clone(&deps);
        async move {
            // Synchronous, like every other database access in this daemon:
            // one `Arc<Mutex<Connection>>` shared by everything, and a prune
            // of a few hundred rows is far shorter than the connector calls
            // that already hold it.
            let summary = run_retention(&deps)?;
            tracing::debug!(?summary, "retention pass");
            Ok(())
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use rusqlite::Connection;
    use tempfile::TempDir;

    use super::*;

    fn deps(dir: &TempDir, max_bytes: u64) -> (Arc<Mutex<Connection>>, RetentionDeps) {
        let conn = Arc::new(Mutex::new(
            ea_core::db::open(&dir.path().join("state.db")).unwrap(),
        ));
        let deps = RetentionDeps {
            store: RetentionStore::new(Arc::clone(&conn)),
            policy: RetentionPolicy::default(),
            log_dir: dir.path().to_path_buf(),
            log_max_bytes: max_bytes,
        };
        (conn, deps)
    }

    fn write_log(dir: &TempDir, name: &str, bytes: usize) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, "x".repeat(bytes)).unwrap();
        path
    }

    #[test]
    fn a_log_under_the_cap_is_left_alone() {
        let dir = TempDir::new().unwrap();
        let path = write_log(&dir, "daemon.err.log", 100);
        assert!(!rotate_if_large(&path, 1024).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 100);
        assert!(!dir.path().join("daemon.err.log.1").exists());
    }

    #[test]
    fn a_log_over_the_cap_is_copied_aside_and_emptied() {
        let dir = TempDir::new().unwrap();
        let path = write_log(&dir, "daemon.err.log", 2048);

        assert!(rotate_if_large(&path, 1024).unwrap());

        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            0,
            "the live file must be empty again"
        );
        let previous = dir.path().join("daemon.err.log.1");
        assert_eq!(std::fs::metadata(&previous).unwrap().len(), 2048);
    }

    /// The reason for copy-then-truncate rather than rename: `launchd` holds
    /// the descriptor, and a rename would leave it pointed at the archived
    /// file. Same inode before and after is the property that matters.
    #[test]
    fn rotation_keeps_the_same_inode_so_an_open_descriptor_still_works() {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;

        let dir = TempDir::new().unwrap();
        let path = write_log(&dir, "daemon.err.log", 2048);
        let inode_before = std::fs::metadata(&path).unwrap().ino();

        // Stand in for the descriptor launchd hands over as stderr.
        let mut held = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();

        assert!(rotate_if_large(&path, 1024).unwrap());
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), inode_before);

        held.write_all(b"after rotation\n").unwrap();
        held.flush().unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains("after rotation"),
            "a descriptor held across the rotation must still write to the live file: {contents:?}"
        );
    }

    /// Only one generation is kept, so the ceiling is two files per stream.
    #[test]
    fn a_second_rotation_overwrites_the_previous_generation() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("daemon.err.log");
        std::fs::write(&path, "first".repeat(1000)).unwrap();
        assert!(rotate_if_large(&path, 1024).unwrap());
        std::fs::write(&path, "second".repeat(1000)).unwrap();
        assert!(rotate_if_large(&path, 1024).unwrap());

        let previous = std::fs::read_to_string(dir.path().join("daemon.err.log.1")).unwrap();
        assert!(
            previous.starts_with("second"),
            "only one generation is kept"
        );
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("daemon.err"))
                .count(),
            2
        );
    }

    #[test]
    fn a_missing_log_is_not_an_error() {
        let dir = TempDir::new().unwrap();
        assert!(!rotate_if_large(&dir.path().join("nothing.log"), 1).unwrap());
    }

    #[test]
    fn a_pass_prunes_and_rotates_together() {
        let dir = TempDir::new().unwrap();
        let (conn, deps) = deps(&dir, 1024);
        conn.lock()
            .unwrap()
            .execute(
                "INSERT INTO runs (kind, prompt, outcome, started_at, finished_at)
                 VALUES ('chat','p','ok',?1,?1)",
                rusqlite::params![(Utc::now() - chrono::Duration::days(400)).to_rfc3339()],
            )
            .unwrap();
        write_log(&dir, "daemon.err.log", 4096);
        write_log(&dir, "daemon.out.log", 10);

        let summary = run_retention(&deps).unwrap();

        assert_eq!(summary.pruned.runs, 1);
        assert_eq!(summary.rotated.len(), 1, "only the oversized one");
        assert!(summary.rotated[0].ends_with("daemon.err.log"));
    }

    /// The prune is the half that matters; a log directory the daemon cannot
    /// write to must not take it down, or the breaker would eventually stop
    /// the pruning as well.
    #[test]
    fn a_log_that_cannot_be_rotated_does_not_fail_the_pass() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        let (_conn, deps) = deps(&dir, 1024);
        let path = write_log(&dir, "daemon.err.log", 4096);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();

        let summary = run_retention(&deps).expect("a rotation failure must not fail the pass");
        assert!(summary.rotated.is_empty());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
}
