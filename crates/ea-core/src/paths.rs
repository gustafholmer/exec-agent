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
/// already exist, and re-assert `0700` if it does. This directory holds the
/// daemon's database (approval queue) and its unix socket, so it must never
/// be group- or world-accessible.
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

/// `~/.config/exec-agent`, or `$EA_CONFIG_DIR`. Created if absent.
pub fn config_dir() -> PathBuf {
    let dir = from_env_or("EA_CONFIG_DIR", || {
        home().join(".config").join("exec-agent")
    });
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn connector_config_dir(connector: &str) -> PathBuf {
    let dir = config_dir().join(connector);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn database_path() -> PathBuf {
    state_dir().join("state.db")
}

pub fn socket_path() -> PathBuf {
    state_dir().join("daemon.sock")
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
