use std::path::PathBuf;

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

/// `~/.local/state/exec-agent`, or `$EA_STATE_DIR`. Created if absent.
pub fn state_dir() -> PathBuf {
    let dir = from_env_or("EA_STATE_DIR", || {
        home().join(".local").join("state").join("exec-agent")
    });
    let _ = std::fs::create_dir_all(&dir);
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
