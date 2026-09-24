use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};

fn from_env_or(var: &str, fallback: impl FnOnce() -> PathBuf) -> PathBuf {
    match std::env::var_os(var) {
        Some(value) => PathBuf::from(value),
        None => fallback(),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .expect("HOME must be set")
}

/// Create `dir` (and any missing parents) with mode `0700` if it doesn't
/// already exist, and re-assert `0700` if it does. These directories hold the
/// daemon's database (approval queue), its unix socket, and every connector's
/// credentials, so none of them may be group- or world-accessible.
fn ensure_private_dir(dir: &Path) {
    if !dir.exists() {
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
    }
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// `~/.local/state/exec-agent`, or `$EA_STATE_DIR`. Created (mode `0700`) if
/// absent.
pub fn state_dir() -> PathBuf {
    let dir = from_env_or("EA_STATE_DIR", || {
        home().join(".local").join("state").join("exec-agent")
    });
    ensure_private_dir(&dir);
    dir
}

/// [`ensure_private_dir`], returning the directory, so the two config
/// accessors below cannot drift from the state directory's guarantee.
fn private_dir(dir: PathBuf) -> PathBuf {
    ensure_private_dir(&dir);
    dir
}

/// `~/.config/exec-agent`, or `$EA_CONFIG_DIR`. Created (mode `0700`) if
/// absent.
///
/// Owner-only for the same reason the state directory is, and it was not:
/// `create_dir_all` takes the ambient umask, which on this machine yields
/// `0755`. What lives here is the Telegram bot token, the owner's chat id, and
/// — under [`connector_config_dir`] — whatever credential each connector keeps
/// beside its manifest. A world-readable directory does not by itself expose a
/// `0600` token file, but it does list the filenames, and the asymmetry
/// between the two directories was an accident rather than a decision.
pub fn config_dir() -> PathBuf {
    private_dir(from_env_or("EA_CONFIG_DIR", || {
        home().join(".config").join("exec-agent")
    }))
}

/// One connector's configuration directory, `0700` like its parent.
pub fn connector_config_dir(connector: &str) -> PathBuf {
    private_dir(config_dir().join(connector))
}

pub fn database_path() -> PathBuf {
    state_dir().join("state.db")
}

pub fn socket_path() -> PathBuf {
    state_dir().join("daemon.sock")
}

/// The single-instance lockfile. See `ea_daemon::lock`.
pub fn lock_path() -> PathBuf {
    state_dir().join("daemon.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercises the private `ensure_private_dir` helper directly rather than
    // going through `state_dir()` + `$EA_STATE_DIR`, since mutating process
    // env vars would need `unsafe`, which this crate forbids crate-wide,
    // and would also race other tests running in the same process.
    #[test]
    fn ensure_private_dir_creates_a_missing_directory_as_owner_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("nested").join("exec-agent");
        assert!(!target.exists());

        ensure_private_dir(&target);

        assert!(target.is_dir());
        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "state dir must be owner-only");
    }

    /// The config directory holds the bot token and the connectors'
    /// credentials; it must be as private as the state directory. Exercised
    /// through the shared helper the public accessors now go through, for the
    /// same reason as the test above.
    #[test]
    fn a_config_directory_is_owner_only_like_the_state_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = private_dir(tmp.path().join("exec-agent"));
        let connector = private_dir(config.join("canvas"));

        for dir in [&config, &connector] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} must be owner-only", dir.display());
        }
    }

    /// And a directory left at the ambient umask by an older version is
    /// tightened on the next start rather than left as it was.
    #[test]
    fn an_existing_world_readable_config_directory_is_tightened() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("exec-agent");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        let dir = private_dir(dir);

        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn ensure_private_dir_tightens_an_existing_looser_directory() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("exec-agent");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();

        ensure_private_dir(&target);

        let mode = std::fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "existing state dir must be tightened to owner-only"
        );
    }
}
