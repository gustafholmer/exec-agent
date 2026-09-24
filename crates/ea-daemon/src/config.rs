//! `~/.config/exec-agent/config.toml`: everything about the daemon a human
//! might reasonably want to change without recompiling it.
//!
//! Every field has a default, and a missing file is not an error — a fresh
//! checkout starts and behaves sensibly with no configuration at all. What the
//! file is *for* is the handful of numbers that are personal: when quiet hours
//! are, how many interruptions an hour is too many, what to mute, and how much
//! model spend a day is allowed.
//!
//! # Why the Telegram credentials are awkward here
//!
//! Task 10 put the bot token in its own `telegram.token` file and refuses to
//! read it unless it is mode `0600`, because a token in a world-readable file
//! is a credential handed to every process on the machine. The brief for this
//! task asks for an optional `[telegram]` block in `config.toml` as well, so
//! both work — but a `config.toml` that *contains a token* is held to exactly
//! the same standard as the token file, and is refused if it is readable by
//! anyone else. The file-based form stays the recommended one, and is what
//! [`TelegramSettings::resolve`] falls back to.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::NaiveTime;
use chrono_tz::Tz;
use serde::Deserialize;

use crate::notify::policy::NotifyConfig;
use crate::notify::telegram::TelegramConfig;
use crate::triage::Tier0Rules;

/// Name of the file under the config directory.
pub const CONFIG_FILE: &str = "config.toml";

/// Sessions a day, across triage and chat. A ceiling, not a target: it exists
/// so that a bug in a loop costs an afternoon's tokens rather than a month's.
pub const DEFAULT_DAILY_SESSION_BUDGET: u32 = 60;

/// Consecutive failures before a scheduler job's breaker trips.
pub const DEFAULT_BREAKER_THRESHOLD: u32 = 5;

/// How often triage runs. The brief's five minutes.
pub const DEFAULT_TRIAGE_INTERVAL_SECS: u64 = 300;

/// Environment override for where connectors are discovered.
pub const CONNECTORS_DIR_ENV: &str = "EA_CONNECTORS_DIR";

/// The resolved configuration the daemon runs on.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    pub notify: NotifyConfig,
    pub tier0: Tier0Rules,
    pub daily_session_budget: u32,
    pub breaker_threshold: u32,
    pub triage_interval: Duration,
    /// Where `discover` looks for connector directories.
    pub connectors_dir: PathBuf,
    /// Present only when the `[telegram]` block was written out in full.
    pub telegram: Option<TelegramSettings>,
}

/// The `[telegram]` block, when there is one.
///
/// `token` is optional even here: naming the chat and the owner in
/// `config.toml` while leaving the token in its own `0600` file is the
/// arrangement this crate recommends.
#[derive(Clone, Deserialize)]
pub struct TelegramSettings {
    #[serde(default)]
    pub token: Option<String>,
    pub chat_id: i64,
    /// The Telegram **user** id allowed to press the buttons. Not the chat id;
    /// see `notify::telegram::OWNER_ID_FILE`.
    pub owner_id: i64,
}

/// Hand-written so a token cannot reach a log line through `{:?}`.
impl std::fmt::Debug for TelegramSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelegramSettings")
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("chat_id", &self.chat_id)
            .field("owner_id", &self.owner_id)
            .finish()
    }
}

impl TelegramSettings {
    /// Turn the block into a usable [`TelegramConfig`], reading the token from
    /// its own file when the block does not carry one.
    pub fn resolve(&self, config_dir: &Path) -> anyhow::Result<TelegramConfig> {
        match &self.token {
            Some(token) => TelegramConfig::from_parts(token.clone(), self.chat_id, self.owner_id),
            None => {
                let file = config_dir.join(crate::notify::telegram::TOKEN_FILE);
                let token = crate::notify::telegram::read_secret(&file)?;
                TelegramConfig::from_parts(token, self.chat_id, self.owner_id)
            }
        }
    }
}

// --------------------------------------------------------------------------
// The on-disk shape
// --------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    notify: RawNotify,
    #[serde(default)]
    tier0: Tier0Rules,
    #[serde(default)]
    daily_session_budget: Option<u32>,
    #[serde(default)]
    breaker_threshold: Option<u32>,
    #[serde(default)]
    triage_interval_secs: Option<u64>,
    #[serde(default)]
    connectors_dir: Option<PathBuf>,
    #[serde(default)]
    telegram: Option<TelegramSettings>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawNotify {
    #[serde(default)]
    threshold: Option<u8>,
    #[serde(default)]
    quiet_start: Option<String>,
    #[serde(default)]
    quiet_end: Option<String>,
    #[serde(default)]
    max_per_hour: Option<usize>,
    #[serde(default)]
    time_zone: Option<String>,
}

fn parse_time(field: &str, raw: &str) -> anyhow::Result<NaiveTime> {
    NaiveTime::parse_from_str(raw, "%H:%M")
        .or_else(|_| NaiveTime::parse_from_str(raw, "%H:%M:%S"))
        .with_context(|| format!("{field} must look like \"22:00\", got {raw:?}"))
}

impl RawNotify {
    fn into_config(self) -> anyhow::Result<NotifyConfig> {
        let mut config = NotifyConfig::default();
        if let Some(threshold) = self.threshold {
            config.threshold = threshold;
        }
        if let Some(raw) = &self.quiet_start {
            config.quiet_start = parse_time("notify.quiet_start", raw)?;
        }
        if let Some(raw) = &self.quiet_end {
            config.quiet_end = parse_time("notify.quiet_end", raw)?;
        }
        if let Some(max) = self.max_per_hour {
            config.max_per_hour = max;
        }
        if let Some(raw) = &self.time_zone {
            config.time_zone = raw
                .parse::<Tz>()
                .map_err(|err| anyhow::anyhow!("notify.time_zone {raw:?}: {err}"))?;
        }
        Ok(config)
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            notify: NotifyConfig::default(),
            tier0: Tier0Rules::default(),
            daily_session_budget: DEFAULT_DAILY_SESSION_BUDGET,
            breaker_threshold: DEFAULT_BREAKER_THRESHOLD,
            triage_interval: Duration::from_secs(DEFAULT_TRIAGE_INTERVAL_SECS),
            connectors_dir: default_connectors_dir(),
            telegram: None,
        }
    }
}

/// Where connectors live when nothing says otherwise.
///
/// In order: `$EA_CONNECTORS_DIR`; then `./connectors` when it exists, which
/// is the repository layout and the directory the launchd plist's
/// `WorkingDirectory` points at; then `~/.config/exec-agent`, which is where a
/// connector installed outside a checkout would put itself.
pub fn default_connectors_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(CONNECTORS_DIR_ENV) {
        return PathBuf::from(dir);
    }
    let local = PathBuf::from("connectors");
    if local.is_dir() {
        return local;
    }
    ea_core::paths::config_dir()
}

impl DaemonConfig {
    /// Load from `~/.config/exec-agent/config.toml` (or `$EA_CONFIG_DIR`).
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&ea_core::paths::config_dir())
    }

    /// Load from an explicit directory. A missing file means "all defaults".
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(CONFIG_FILE);
        if !path.exists() {
            tracing::info!(path = %path.display(), "no config file; using defaults");
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let raw: RawConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        // A config file carrying a bot token is a credential file, and is held
        // to the same standard as `telegram.token`.
        if raw
            .telegram
            .as_ref()
            .and_then(|t| t.token.as_ref())
            .is_some()
        {
            crate::notify::telegram::require_owner_only(&path)?;
        }

        let notify = raw.notify.into_config()?;
        if notify.max_per_hour == 0 {
            bail!(
                "notify.max_per_hour is 0, which would silence every notification; \
                   remove the notify block instead of setting it to zero"
            );
        }

        Ok(Self {
            notify,
            tier0: raw.tier0,
            daily_session_budget: raw
                .daily_session_budget
                .unwrap_or(DEFAULT_DAILY_SESSION_BUDGET),
            breaker_threshold: raw
                .breaker_threshold
                .unwrap_or(DEFAULT_BREAKER_THRESHOLD)
                .max(1),
            triage_interval: Duration::from_secs(
                raw.triage_interval_secs
                    .unwrap_or(DEFAULT_TRIAGE_INTERVAL_SECS)
                    .max(1),
            ),
            connectors_dir: raw.connectors_dir.unwrap_or_else(default_connectors_dir),
            telegram: raw.telegram,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tempfile::TempDir;

    use super::*;

    fn write(dir: &TempDir, body: &str) -> PathBuf {
        let path = dir.path().join(CONFIG_FILE);
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn a_missing_file_is_all_defaults() {
        let dir = TempDir::new().unwrap();
        let config = DaemonConfig::load_from(dir.path()).unwrap();
        assert_eq!(config.notify, NotifyConfig::default());
        assert_eq!(config.tier0, Tier0Rules::default());
        assert_eq!(config.daily_session_budget, DEFAULT_DAILY_SESSION_BUDGET);
        assert_eq!(
            config.triage_interval,
            Duration::from_secs(DEFAULT_TRIAGE_INTERVAL_SECS)
        );
        assert!(config.telegram.is_none());
    }

    #[test]
    fn every_block_is_read() {
        let dir = TempDir::new().unwrap();
        write(
            &dir,
            r#"
daily_session_budget = 12
triage_interval_secs = 60
connectors_dir = "/tmp/connectors"

[notify]
threshold = 75
quiet_start = "23:30"
quiet_end = "06:15"
max_per_hour = 1
time_zone = "America/New_York"

[tier0]
muted_sources = ["newsletter"]
muted_kinds = ["ping"]
keywords = ["invoice"]
"#,
        );
        let config = DaemonConfig::load_from(dir.path()).unwrap();
        assert_eq!(config.notify.threshold, 75);
        assert_eq!(
            config.notify.quiet_start,
            NaiveTime::from_hms_opt(23, 30, 0).unwrap()
        );
        assert_eq!(
            config.notify.quiet_end,
            NaiveTime::from_hms_opt(6, 15, 0).unwrap()
        );
        assert_eq!(config.notify.max_per_hour, 1);
        assert_eq!(config.notify.time_zone, chrono_tz::America::New_York);
        assert_eq!(config.tier0.muted_sources, vec!["newsletter".to_string()]);
        assert_eq!(config.tier0.keywords, vec!["invoice".to_string()]);
        assert_eq!(config.daily_session_budget, 12);
        assert_eq!(config.triage_interval, Duration::from_secs(60));
        assert_eq!(config.connectors_dir, PathBuf::from("/tmp/connectors"));
    }

    /// A partial `[notify]` block keeps the defaults for what it omits, rather
    /// than zeroing them.
    #[test]
    fn a_partial_notify_block_keeps_the_other_defaults() {
        let dir = TempDir::new().unwrap();
        write(&dir, "[notify]\nthreshold = 80\n");
        let config = DaemonConfig::load_from(dir.path()).unwrap();
        assert_eq!(config.notify.threshold, 80);
        assert_eq!(config.notify.max_per_hour, 3);
        assert_eq!(config.notify.time_zone, chrono_tz::Europe::Stockholm);
    }

    #[test]
    fn a_typo_is_an_error_rather_than_a_silently_ignored_line() {
        let dir = TempDir::new().unwrap();
        write(&dir, "[notify]\nthreshhold = 80\n");
        let err = DaemonConfig::load_from(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("threshhold"), "{err:#}");
    }

    #[test]
    fn a_bad_time_names_the_field() {
        let dir = TempDir::new().unwrap();
        write(&dir, "[notify]\nquiet_start = \"half past ten\"\n");
        let err = DaemonConfig::load_from(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("notify.quiet_start"), "{err:#}");
    }

    #[test]
    fn a_zero_rate_limit_is_refused() {
        let dir = TempDir::new().unwrap();
        write(&dir, "[notify]\nmax_per_hour = 0\n");
        let err = DaemonConfig::load_from(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("max_per_hour"), "{err:#}");
    }

    /// A `config.toml` holding a bot token is a credential file.
    #[test]
    fn a_world_readable_config_carrying_a_token_is_refused() {
        let dir = TempDir::new().unwrap();
        let path = write(
            &dir,
            "[telegram]\ntoken = \"123:AAA\"\nchat_id = 5\nowner_id = 7\n",
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = DaemonConfig::load_from(dir.path()).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("0600"), "{text}");
        assert!(!text.contains("123:AAA"), "the token must not be echoed");
    }

    #[test]
    fn a_locked_down_config_carrying_a_token_resolves() {
        let dir = TempDir::new().unwrap();
        let path = write(
            &dir,
            "[telegram]\ntoken = \"123456:AAbb-cc_dd\"\nchat_id = 5\nowner_id = 7\n",
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = DaemonConfig::load_from(dir.path()).unwrap();
        let telegram = config.telegram.as_ref().unwrap();
        assert_eq!(telegram.owner_id, 7);
        let resolved = telegram.resolve(dir.path()).unwrap();
        assert_eq!(resolved.chat_id, 5);
        assert_eq!(resolved.owner_id.0, 7);
    }

    /// The recommended arrangement: ids in `config.toml`, token in its own
    /// `0600` file.
    #[test]
    fn a_block_without_a_token_reads_the_token_file() {
        let dir = TempDir::new().unwrap();
        write(&dir, "[telegram]\nchat_id = 5\nowner_id = 7\n");
        let token = dir.path().join(crate::notify::telegram::TOKEN_FILE);
        std::fs::write(&token, "123456:AAbb-cc_dd\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();

        let config = DaemonConfig::load_from(dir.path()).unwrap();
        let resolved = config
            .telegram
            .as_ref()
            .unwrap()
            .resolve(dir.path())
            .unwrap();
        assert_eq!(resolved.chat_id, 5);
    }

    /// `{:?}` on the settings must not print the token.
    #[test]
    fn debug_redacts_the_token() {
        let settings = TelegramSettings {
            token: Some("123456:SECRET".to_string()),
            chat_id: 1,
            owner_id: 2,
        };
        let text = format!("{settings:?}");
        assert!(!text.contains("SECRET"), "{text}");
        assert!(text.contains("redacted"), "{text}");
    }
}
