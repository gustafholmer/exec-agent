//! `~/.config/exec-agent/fortnox/app.json` — the Fortnox integration's client
//! id and secret.
//!
//! Fortnox's OAuth client is registered once in the Developer Portal and is
//! shared by the authorize command and by this server. The *tokens* live
//! beside it in `tokens.json`, written by
//! [`ea_fortnox::auth::FileTokenStore`]; this file is only the registration.
//!
//! The same standard as `ea-google`'s `AppConfig`, and for the same reason:
//! the client secret is half of what a code exchange needs. Mode `0600` is
//! required, the file's contents are never quoted in an error, and the
//! hand-written [`fmt::Debug`] prints `<redacted>`.

use std::fmt;
use std::path::Path;

use anyhow::{bail, Context};
use ea_fortnox::auth::{AUTHORIZE_COMMAND, CONNECTOR};
use serde::{Deserialize, Serialize};

/// The file, inside the connector's config directory.
pub const APP_CONFIG_FILE: &str = "app.json";

/// The redirect URI the authorize command listens on, and the one that must be
/// registered in the Fortnox Developer Portal. Fortnox matches it exactly.
pub const DEFAULT_REDIRECT_URI: &str = "http://localhost:8723/callback";

/// The scopes this connector asks for. `bookkeeping` covers vouchers and
/// accounts, `invoice` and `supplierinvoice` the two unpaid-invoice lists,
/// `archive` the receipt upload, `settings` the financial years.
pub const SCOPES: &[&str] = &[
    "bookkeeping",
    "invoice",
    "supplierinvoice",
    "archive",
    "settings",
];

/// The registered integration, as stored on disk.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
}

fn default_redirect_uri() -> String {
    DEFAULT_REDIRECT_URI.to_string()
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

/// What to print when `app.json` is missing or unusable. The reader is looking
/// at a daemon that will not start a connector, not at this file.
fn how_to_create(path: &Path) -> String {
    let dir = path.parent().unwrap_or(Path::new(".")).display();
    format!(
        "Create it with:\n  \
         mkdir -p {dir}\n  \
         printf '{{\"clientId\":\"<id>\",\"clientSecret\":\"<secret>\"}}' > {path}\n  \
         chmod 600 {path}\n\
         Register the integration at https://developer.fortnox.se/ and add \
         {DEFAULT_REDIRECT_URI} as its redirect URI — Fortnox matches it exactly. \
         Then run {AUTHORIZE_COMMAND}.",
        path = path.display()
    )
}

impl AppConfig {
    /// Load from `~/.config/exec-agent/fortnox/app.json` (or
    /// `$EA_CONFIG_DIR/fortnox/app.json`).
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&ea_core::paths::connector_config_dir(CONNECTOR))
    }

    /// Load from an explicit directory. The seam the tests point at a temp
    /// directory.
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(APP_CONFIG_FILE);

        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no Fortnox integration at {}.\n{}",
                    path.display(),
                    how_to_create(&path)
                );
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };

        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is mode {mode:04o}; it holds the Fortnox client secret and must be \
                 0600 (run: chmod 600 {})",
                path.display(),
                path.display()
            );
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        // The text is not quoted: the client secret is in it.
        let config: Self = serde_json::from_str(&text).map_err(|err| {
            anyhow::anyhow!(
                "{} is not the expected JSON ({err}). It must be an object with \
                 \"clientId\" and \"clientSecret\".\n{}",
                path.display(),
                how_to_create(&path)
            )
        })?;

        if config.client_id.trim().is_empty() {
            bail!("{} has an empty \"clientId\"", path.display());
        }
        if config.client_secret.trim().is_empty() {
            bail!("{} has an empty \"clientSecret\"", path.display());
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    fn write(dir: &Path, body: &str, mode: u32) {
        let path = dir.join(APP_CONFIG_FILE);
        std::fs::write(&path, body).expect("writing app.json");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .expect("chmod app.json");
    }

    #[test]
    fn a_missing_file_names_the_path_and_the_authorize_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = format!("{:#}", AppConfig::load_from(dir.path()).unwrap_err());
        assert!(err.contains("app.json"), "{err}");
        assert!(err.contains(AUTHORIZE_COMMAND), "{err}");
    }

    #[test]
    fn a_world_readable_file_is_refused() {
        let dir = tempfile::TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"clientId":"id","clientSecret":"sh"}"#,
            0o644,
        );
        let err = format!("{:#}", AppConfig::load_from(dir.path()).unwrap_err());
        assert!(err.contains("0644"), "{err}");
        assert!(err.contains("chmod 600"), "{err}");
    }

    /// The point of the hand-written `Debug`: a secret must not reach a log
    /// line, a panic message or a `{:?}`.
    #[test]
    fn the_debug_impl_redacts_the_secret() {
        let dir = tempfile::TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"clientId":"public-id","clientSecret":"SUPER-SECRET"}"#,
            0o600,
        );
        let config = AppConfig::load_from(dir.path()).unwrap();
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("SUPER-SECRET"), "{rendered}");
        assert!(rendered.contains("public-id"), "{rendered}");
        assert_eq!(config.redirect_uri, DEFAULT_REDIRECT_URI);
    }

    /// A malformed file must not quote its own contents back: a half-written
    /// `app.json` is exactly where a secret sits.
    #[test]
    fn a_malformed_file_is_not_quoted_back() {
        let dir = tempfile::TempDir::new().unwrap();
        write(
            dir.path(),
            r#"{"clientId":"id","clientSecret":"LEAK"#,
            0o600,
        );
        let err = format!("{:#}", AppConfig::load_from(dir.path()).unwrap_err());
        assert!(!err.contains("LEAK"), "{err}");
        assert!(err.contains("app.json"), "{err}");
    }
}
