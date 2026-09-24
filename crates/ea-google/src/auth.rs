//! Google OAuth 2.0: the app credentials, the per-account token store, and the
//! refresh path.
//!
//! # Why this is hand-rolled rather than built on the `oauth2` crate
//!
//! The brief asked for the decision to be recorded here, so: **hand-rolled**,
//! over `reqwest`, which is already a workspace dependency. `oauth2 5.0.0`
//! exists and is maintained; it was rejected on three specific grounds, in
//! descending order of weight.
//!
//! 1. **Its error type carries the raw response body, and derives `Debug`.**
//!    `oauth2::RequestTokenError::Parse(serde_path_to_error::Error, Vec<u8>)`
//!    keeps the unparsed bytes so callers can diagnose a bad server. That
//!    variant is reachable on a *2xx* response too — and a 2xx token response
//!    is precisely the payload that contains an access token and a refresh
//!    token. One `tracing::error!(?err)` or `anyhow!("{err:?}")` anywhere
//!    downstream would then put both in a log file. This crate's whole reason
//!    to exist is that that must be impossible, and "remember never to `Debug`
//!    this third-party error" is not a property a test can hold down. The
//!    hand-rolled path never puts a success body into an error at all: a 2xx
//!    that will not parse is reported as the `serde` error and the status,
//!    with no bytes quoted.
//! 2. **The ceremony is real and the saving is not.** What Google's three-leg
//!    flow needs is an authorization URL (query-string construction), a code
//!    exchange (form POST, JSON back) and a refresh grant (form POST, JSON
//!    back). That is roughly seventy lines here. `oauth2 5.0` spends them
//!    instead on a nine-parameter type-state `Client<TE, TR, TIR, RT, TRE,
//!    HasAuthUrl, HasDeviceAuthUrl, HasIntrospectionUrl, HasRevocationUrl,
//!    HasTokenUrl>`, and the Google-specific parts (`access_type=offline`,
//!    `prompt=consent`) still go through `add_extra_param` as raw strings.
//! 3. **Dependency cost.** It pulls `base64`, `rand`, `sha2`, `http`,
//!    `serde_path_to_error`, and `thiserror 1.0` — a second major version of a
//!    crate the workspace already pins at 2. For PKCE, device flow, token
//!    introspection and revocation, none of which this crate performs.
//!
//! The counter-argument, recorded honestly: `oauth2` would give us PKCE and a
//! `SecretString` wrapper whose `Debug` is redacted for free. PKCE buys
//! nothing for a confidential client with a client secret on a loopback
//! redirect, and the redaction is replicated below by hand for every type that
//! holds a secret.
//!
//! # Token safety
//!
//! A Google refresh token for these scopes reads the owner's calendar and
//! mail and can draft mail as them. It does not expire on its own. So:
//!
//! * [`Tokens`], [`AppConfig`], [`RefreshResponse`] and [`HttpRefreshBackend`]
//!   all have hand-written `Debug` impls printing `<redacted>`. There is a
//!   test that formats each with `{:?}` and asserts the secret is absent.
//! * Secrets travel in a POST body, never in a URL. `reqwest`'s errors carry
//!   the URL in their `Display`; every one of them additionally goes through
//!   `.without_url()` here, and the token endpoint URL is not secret anyway.
//! * No error message quotes a response body that could have been a *success*
//!   body. Error bodies (4xx/5xx) are quoted, truncated, because they carry
//!   `{"error":"invalid_grant"}` and nothing else useful.
//! * The token store's errors quote the *path* of a corrupt file, never its
//!   contents — a malformed token file is exactly where a token might sit in
//!   an unexpected place.
//! * The authorization *code* that arrives on the loopback redirect is a
//!   one-time bearer of the whole grant. `bin/authorize.rs` parses it out of
//!   the request line and never prints the request line.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};

/// The connector's name. Equal to its directory's basename
/// (`connectors/google`), which the daemon enforces at startup.
pub const CONNECTOR: &str = "google";

/// The file, inside the connector's config directory, holding the OAuth client
/// registration. Excluded from [`TokenStore::list`], which would otherwise
/// report it as an account named `app`.
pub const APP_CONFIG_FILE: &str = "app.json";

/// The scopes every account is authorised for. Read-only on Calendar and
/// Gmail, plus `gmail.compose` — which creates drafts and can send, and is the
/// only non-read scope this project ever asks for.
pub const SCOPES: [&str; 3] = [
    "https://www.googleapis.com/auth/calendar.readonly",
    "https://www.googleapis.com/auth/gmail.readonly",
    "https://www.googleapis.com/auth/gmail.compose",
];

/// Google's authorization endpoint, where the consent screen lives.
pub const DEFAULT_AUTH_URI: &str = "https://accounts.google.com/o/oauth2/v2/auth";

/// Google's token endpoint: both the code exchange and the refresh grant.
pub const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// Where the consent redirect lands. Loopback, because a desktop client has
/// nowhere else to put it, and a fixed port because Google requires every
/// redirect URI to be registered ahead of time.
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8471/callback";

/// How close to expiry counts as expired. Google's access tokens last an hour;
/// a minute of margin covers the round trip to the API plus a slow network,
/// without refreshing more than once an hour in the steady state.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Per-request deadline on a token-endpoint call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an *error* response body to quote back. Never applied to a 2xx
/// body — see the module's `# Token safety`.
const BODY_SNIPPET: usize = 300;

// ---------------------------------------------------------------------------
// Account names
// ---------------------------------------------------------------------------

/// Reject anything that is not `^[a-z0-9][a-z0-9_-]*$`, **before** a path
/// is constructed from it.
///
/// Account labels reach this crate from `connector.toml` and from tool
/// arguments a language model produces. `Path::join` with an absolute path
/// replaces the base entirely, and `..` walks out of the config directory, so
/// `TokenStore::read("../../.ssh/id_rsa")` would otherwise be a file-read
/// primitive and `write` an overwrite primitive. Checking the characters is
/// stricter and simpler than trying to canonicalise a path that does not exist
/// yet.
///
/// # Why lower-case only
///
/// A label becomes `~/.config/exec-agent/google/<label>.json`, and the
/// owner's disk is APFS, which is case-*insensitive* by default. `work` and
/// `Work` would therefore be two labels — two accounts as far as every other
/// part of this crate is concerned — sharing one file: authorising `Work`
/// would silently overwrite `work`'s grant, and a poll over both would fetch
/// the same mailbox twice and report every message as two rows. Refusing the
/// upper-case label outright is the only version of this that cannot surprise
/// anyone, and it costs the owner one keystroke at authorisation time.
pub fn validate_account(account: &str) -> anyhow::Result<()> {
    let lower = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = account.chars();
    let ok = match chars.next() {
        Some(first) if lower(first) => chars.all(|c| lower(c) || c == '_' || c == '-'),
        _ => false,
    };
    if !ok {
        let hint = if account.chars().any(|c| c.is_ascii_uppercase()) {
            format!(
                " Labels are lower-case only, because they become filenames on a \
                 case-insensitive disk where {:?} and {account:?} would be one file \
                 holding two accounts' tokens; use {:?}.",
                account.to_ascii_lowercase(),
                account.to_ascii_lowercase()
            )
        } else {
            String::new()
        };
        bail!(
            "{account:?} is not a usable Google account label. A label must match \
             ^[a-z0-9][a-z0-9_-]*$ (for example \"work\" or \"private\"); it \
             becomes a filename, so \"/\" and \"..\" are refused.{hint}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

/// One account's OAuth state, as stored on disk.
///
/// No `Debug` derive: both token fields are secrets. See the module docs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tokens {
    /// The short-lived bearer token. Roughly an hour.
    pub access_token: String,
    /// The long-lived grant. Google issues this only when the consent request
    /// carried `access_type=offline&prompt=consent`, and does not repeat it on
    /// subsequent refreshes.
    pub refresh_token: String,
    /// When [`Tokens::access_token`] stops working.
    pub expiry: DateTime<Utc>,
    /// The space-separated scope list Google actually granted, which may be
    /// narrower than [`SCOPES`] if the user unticked a box.
    pub scope: String,
}

impl fmt::Debug for Tokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expiry", &self.expiry)
            .field("scope", &self.scope)
            .finish()
    }
}

impl Tokens {
    /// True when the access token is expired, or close enough to it that a
    /// request started now might arrive after it is.
    pub fn is_stale_at(&self, now: DateTime<Utc>) -> bool {
        let margin = chrono::Duration::from_std(REFRESH_MARGIN)
            .unwrap_or_else(|_| chrono::Duration::seconds(60));
        self.expiry <= now + margin
    }
}

// ---------------------------------------------------------------------------
// The token store
// ---------------------------------------------------------------------------

/// One JSON file per account under `~/.config/exec-agent/google/`, mode `0600`.
///
/// One file per account rather than one file with a map, so that a crash, a
/// concurrent write, or a corrupt file costs one account rather than all of
/// them — and so that `write` can be a rename, which is atomic on any POSIX
/// filesystem.
#[derive(Debug, Clone)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// `dir` of `None` means `~/.config/exec-agent/google/` (or
    /// `$EA_CONFIG_DIR/google/`), created `0700` if absent.
    pub fn new(dir: Option<PathBuf>) -> Self {
        let dir = dir.unwrap_or_else(|| ea_core::paths::connector_config_dir(CONNECTOR));
        Self { dir }
    }

    /// The directory the store reads and writes.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The path an account's tokens live at. Errors rather than returning a
    /// path for a label that failed validation, so there is no way to get a
    /// path out of this type without passing the check.
    pub fn path_for(&self, account: &str) -> anyhow::Result<PathBuf> {
        validate_account(account)?;
        Ok(self.dir.join(format!("{account}.json")))
    }

    /// Read one account's tokens.
    ///
    /// A missing file is the common case for a fresh install and names both
    /// the account and the command that fixes it.
    pub fn read(&self, account: &str) -> anyhow::Result<Tokens> {
        use std::os::unix::fs::PermissionsExt;

        let path = self.path_for(account)?;

        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no Google tokens for account {account:?} at {}. \
                     Authorise it with:\n  {}",
                    path.display(),
                    authorize_command(account)
                );
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", path.display()));
            }
        };

        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is mode {mode:04o}; it holds a Google refresh token and must be \
                 0600 (run: chmod 600 {})",
                path.display(),
                path.display()
            );
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        // The serde error is quoted; the file is not. A malformed token file
        // is exactly the case where a token may be sitting somewhere
        // unexpected inside it.
        serde_json::from_str(&text).map_err(|err| {
            anyhow::anyhow!(
                "{} is not a readable Google token file ({err}). Delete it and \
                 re-authorise with:\n  {}",
                path.display(),
                authorize_command(account)
            )
        })
    }

    /// Write one account's tokens, atomically, mode `0600`.
    ///
    /// Temp file in the same directory (so `rename` stays within one
    /// filesystem and is therefore atomic), `fsync`, then `rename`. A crash at
    /// any point leaves either the old file or the new one, never a truncated
    /// one — and a truncated token file means a manual re-authorisation.
    pub fn write(&self, account: &str, tokens: &Tokens) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let path = self.path_for(account)?;

        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));

        // Unique per process and per call: two daemons, or two tasks, must not
        // share a temp file and interleave their bytes.
        let tmp = self.dir.join(format!(
            ".{account}.json.tmp.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));

        // 0600 from the moment the file exists — not set afterwards, which
        // would leave a window in which the token is world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;

        let body = serde_json::to_vec_pretty(tokens).context("serialising Google tokens")?;

        let result = (|| -> std::io::Result<()> {
            file.write_all(&body)?;
            file.write_all(b"\n")?;
            file.sync_all()
        })();
        drop(file);

        if let Err(err) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("writing {}", tmp.display()));
        }

        // Re-assert the mode: `create_new(...).mode(..)` is masked by the
        // process umask, so a umask of 0 is the only case where it is exactly
        // 0600 already, and `rename` preserves whatever the temp file has.
        if let Err(err) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("setting mode 0600 on {}", tmp.display()));
        }

        if let Err(err) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err)
                .with_context(|| format!("renaming {} onto {}", tmp.display(), path.display()));
        }

        Ok(())
    }

    /// The account labels that have token files, sorted.
    ///
    /// A missing directory is an empty list, not an error: nothing has been
    /// authorised yet. `app.json` and the dotted temp files are skipped, as is
    /// anything whose stem would not pass [`validate_account`] — such a file
    /// was not written by [`TokenStore::write`].
    pub fn list(&self) -> anyhow::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.dir.display()));
            }
        };

        let mut accounts = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", self.dir.display()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name == APP_CONFIG_FILE {
                continue;
            }
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if validate_account(stem).is_err() {
                continue;
            }
            accounts.push(stem.to_string());
        }
        accounts.sort();
        Ok(accounts)
    }
}

/// The exact command a person should run to (re-)authorise an account. Every
/// message that reports a missing or dead grant ends with this string, and it
/// has to be copy-pasteable — which is why the binary is named
/// `ea-google-authorize` rather than `authorize`.
pub fn authorize_command(account: &str) -> String {
    format!("ea-google-authorize {account}")
}

// ---------------------------------------------------------------------------
// The application's OAuth client registration
// ---------------------------------------------------------------------------

/// `~/.config/exec-agent/google/app.json` — the OAuth *client*, shared by every
/// account. Not a per-account secret, but a secret: it is half of what a code
/// exchange needs.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppConfig {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
    #[serde(default = "default_auth_uri")]
    pub auth_uri: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_redirect_uri() -> String {
    DEFAULT_REDIRECT_URI.to_string()
}
fn default_auth_uri() -> String {
    DEFAULT_AUTH_URI.to_string()
}
fn default_token_uri() -> String {
    DEFAULT_TOKEN_URI.to_string()
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .field("redirect_uri", &self.redirect_uri)
            .field("auth_uri", &self.auth_uri)
            .field("token_uri", &self.token_uri)
            .finish()
    }
}

/// What to print when `app.json` is missing or unusable. The reader is looking
/// at a daemon that will not start a connector, not at this file.
fn how_to_create_app_config(path: &Path) -> String {
    let dir = path.parent().unwrap_or(Path::new(".")).display();
    format!(
        "Create it with:\n  \
         mkdir -p {dir}\n  \
         printf '{{\"clientId\":\"<id>.apps.googleusercontent.com\",\
         \"clientSecret\":\"<secret>\"}}' > {path}\n  \
         chmod 600 {path}\n\
         Get the pair from https://console.cloud.google.com/apis/credentials -> \
         \"Create credentials\" -> \"OAuth client ID\" -> \"Desktop app\", and register \
         {DEFAULT_REDIRECT_URI} as an authorised redirect URI.",
        path = path.display()
    )
}

impl AppConfig {
    /// Load from `~/.config/exec-agent/google/app.json` (or
    /// `$EA_CONFIG_DIR/google/app.json`).
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&ea_core::paths::connector_config_dir(CONNECTOR))
    }

    /// Load from an explicit directory. Used by the tests, and by anyone
    /// running two client registrations out of one checkout.
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join(APP_CONFIG_FILE);

        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no Google OAuth client at {}.\n{}",
                    path.display(),
                    how_to_create_app_config(&path)
                );
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };

        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is mode {mode:04o}; it holds the OAuth client secret and must be \
                 0600 (run: chmod 600 {})",
                path.display(),
                path.display()
            );
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        // Contents are not quoted: the client secret is in there.
        let config: Self = serde_json::from_str(&text).map_err(|err| {
            anyhow::anyhow!(
                "{} is not the expected JSON ({err}). It must be an object with \
                 \"clientId\" and \"clientSecret\".\n{}",
                path.display(),
                how_to_create_app_config(&path)
            )
        })?;

        if config.client_id.trim().is_empty() {
            bail!("{} has an empty \"clientId\"", path.display());
        }
        if config.client_secret.trim().is_empty() {
            bail!("{} has an empty \"clientSecret\"", path.display());
        }
        Url::parse(&config.token_uri)
            .with_context(|| format!("{} has an unparseable \"tokenUri\"", path.display()))?;
        Url::parse(&config.auth_uri)
            .with_context(|| format!("{} has an unparseable \"authUri\"", path.display()))?;
        Url::parse(&config.redirect_uri)
            .with_context(|| format!("{} has an unparseable \"redirectUri\"", path.display()))?;

        Ok(config)
    }

    /// The consent URL for one account.
    ///
    /// `access_type=offline` is what makes Google willing to issue a refresh
    /// token at all; `prompt=consent` is what makes it issue one *again* for
    /// an account that has already consented once. Without the second, a
    /// re-authorisation silently returns an access token and no refresh token,
    /// which is the failure `bin/authorize.rs` refuses to write.
    ///
    /// `state` is echoed back on the redirect and checked, so a stray request
    /// to the loopback listener cannot inject a code.
    pub fn authorization_url(&self, state: &str) -> anyhow::Result<Url> {
        Url::parse_with_params(
            &self.auth_uri,
            &[
                ("client_id", self.client_id.as_str()),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("response_type", "code"),
                ("scope", SCOPES.join(" ").as_str()),
                ("access_type", "offline"),
                ("prompt", "consent"),
                ("state", state),
            ],
        )
        .with_context(|| format!("building an authorization URL from {:?}", self.auth_uri))
    }
}

// ---------------------------------------------------------------------------
// The refresh backend
// ---------------------------------------------------------------------------

/// What the token endpoint answers a refresh (or a code exchange) with.
///
/// No `Debug` derive: `access_token` and `refresh_token` are the secrets this
/// whole crate exists to keep out of logs.
#[derive(Clone, Deserialize)]
pub struct RefreshResponse {
    pub access_token: String,
    /// Absent on a refresh — Google issues a refresh token once, at consent,
    /// and expects the client to keep it. Present on a code exchange, and its
    /// absence there is a hard error.
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds until [`RefreshResponse::access_token`] expires.
    pub expires_in: i64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl fmt::Debug for RefreshResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RefreshResponse")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Why a refresh failed.
///
/// The split exists for one reason: `invalid_grant` means the grant is gone
/// and only a human at a browser can fix it, whereas everything else is
/// transient and must not be reported as a revoked grant. Telling somebody to
/// re-authorise because their wifi dropped trains them to re-authorise for
/// every hiccup.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// Google answered `{"error":"invalid_grant"}`: revoked, expired, or from
    /// a different client.
    #[error("Google rejected the refresh token (invalid_grant)")]
    InvalidGrant,
    /// Anything else: a network failure, a 5xx, an unparseable response.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// The refresh grant, behind a trait so [`Auth`] can be tested without HTTP
/// and so a future connector can substitute a service account.
///
/// Hand-rolled boxed future rather than `async fn` in trait, because `Auth`
/// holds an `Arc<dyn RefreshBackend>` and async-fn-in-trait is not
/// `dyn`-compatible. That is the whole reason not to reach for `async_trait`.
pub trait RefreshBackend: Send + Sync {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RefreshResponse, RefreshError>> + Send + 'a>>;
}

/// The real thing: a form POST to Google's token endpoint.
///
/// No `Debug` derive — it holds the client secret.
#[derive(Clone)]
pub struct HttpRefreshBackend {
    client: reqwest::Client,
    token_uri: String,
    client_id: String,
    client_secret: String,
}

impl fmt::Debug for HttpRefreshBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRefreshBackend")
            .field("token_uri", &self.token_uri)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl HttpRefreshBackend {
    pub fn new(config: &AppConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // This client carries a refresh token and a client secret in every
            // request body. `reqwest`'s default redirect policy strips the
            // `Authorization` header across origins but knows nothing about a
            // body, so a redirect would re-POST both to wherever the server
            // pointed. The token endpoint has no reason to redirect.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the HTTP client for Google's token endpoint")?;
        Ok(Self {
            client,
            token_uri: config.token_uri.clone(),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
        })
    }

    /// Exchange an authorization code for tokens. Used only by
    /// `bin/authorize.rs`; a refresh never needs it.
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> anyhow::Result<RefreshResponse> {
        let form = [
            ("code", code),
            ("client_id", self.client_id.as_str()),
            ("client_secret", self.client_secret.as_str()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ];
        self.post_form(&form).await.map_err(|err| match err {
            RefreshError::InvalidGrant => anyhow::anyhow!(
                "Google rejected the authorization code (invalid_grant). Codes are \
                 single-use and expire within minutes; run the command again."
            ),
            RefreshError::Other(err) => err,
        })
    }

    async fn post_form(&self, form: &[(&str, &str)]) -> Result<RefreshResponse, RefreshError> {
        let response = self
            .client
            .post(&self.token_uri)
            .form(form)
            .send()
            .await
            // `.without_url()` on every `reqwest` error, as a habit rather
            // than because this URL is secret: the habit is what keeps a
            // future endpoint with a token in its path from leaking.
            .map_err(|err| {
                RefreshError::Other(
                    anyhow::Error::new(err.without_url())
                        .context("POST to Google's token endpoint failed"),
                )
            })?;

        let status = response.status();

        if !status.is_success() {
            // An error body is safe to quote: by definition it did not carry a
            // token. It is truncated anyway.
            let body = response.text().await.unwrap_or_default();
            if is_invalid_grant(&body) {
                return Err(RefreshError::InvalidGrant);
            }
            let snippet: String = body.chars().take(BODY_SNIPPET).collect();
            return Err(RefreshError::Other(anyhow::anyhow!(
                "Google's token endpoint answered {status}: {snippet}"
            )));
        }

        let body = response.text().await.map_err(|err| {
            RefreshError::Other(
                anyhow::Error::new(err.without_url())
                    .context("reading Google's token endpoint response"),
            )
        })?;

        // The body is NOT quoted here. This is the success path, so the body is
        // exactly the document that contains an access token and possibly a
        // refresh token — the one thing that must never reach a log.
        serde_json::from_str::<RefreshResponse>(&body).map_err(|err| {
            RefreshError::Other(anyhow::anyhow!(
                "Google's token endpoint answered {status} with {} bytes that are not \
                 the expected JSON ({err}). The body is deliberately not quoted: a \
                 success response carries tokens.",
                body.len()
            ))
        })
    }
}

/// Google reports a dead grant as a 400 with `{"error":"invalid_grant", ...}`.
/// Matched on the parsed field rather than a substring of the whole body, so a
/// `error_description` that merely mentions the phrase cannot be mistaken for
/// it.
fn is_invalid_grant(body: &str) -> bool {
    #[derive(Deserialize)]
    struct ErrorBody {
        error: Option<String>,
    }
    serde_json::from_str::<ErrorBody>(body)
        .ok()
        .and_then(|b| b.error)
        .is_some_and(|e| e == "invalid_grant")
}

impl RefreshBackend for HttpRefreshBackend {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<RefreshResponse, RefreshError>> + Send + 'a>> {
        Box::pin(async move {
            let form = [
                ("client_id", self.client_id.as_str()),
                ("client_secret", self.client_secret.as_str()),
                ("refresh_token", refresh_token),
                ("grant_type", "refresh_token"),
            ];
            self.post_form(&form).await
        })
    }
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

/// Hands out access tokens, refreshing and persisting as needed.
///
/// No `Debug` impl at all: it reaches a token store and a backend holding the
/// client secret, and there is no reason for it to appear in a log line.
pub struct Auth {
    store: TokenStore,
    /// One mutex per account, created on first use. The outer mutex is held
    /// only long enough to look one up or insert it — never across an
    /// `await` on the network — so a slow refresh for `work` cannot block a
    /// cached-token read for `private`.
    locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    backend: Arc<dyn RefreshBackend>,
}

impl Auth {
    /// The production constructor: refreshes over HTTP against the endpoint in
    /// `config`.
    pub fn new(config: AppConfig, store: TokenStore) -> anyhow::Result<Self> {
        let backend = Arc::new(HttpRefreshBackend::new(&config)?);
        Ok(Self::with_backend(store, backend))
    }

    /// Inject a backend. Used by the tests; also the seam a service-account
    /// flow would slot into.
    pub fn with_backend(store: TokenStore, backend: Arc<dyn RefreshBackend>) -> Self {
        Self {
            store,
            locks: tokio::sync::Mutex::new(HashMap::new()),
            backend,
        }
    }

    pub fn store(&self) -> &TokenStore {
        &self.store
    }

    /// A usable access token for `account`, refreshing first if it is within
    /// [`REFRESH_MARGIN`] of expiry.
    pub async fn access_token(&self, account: &str) -> anyhow::Result<String> {
        self.access_token_at(account, Utc::now()).await
    }

    /// [`Auth::access_token`] against an explicit clock, so the expiry tests do
    /// not race the wall clock.
    pub async fn access_token_at(
        &self,
        account: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        validate_account(account)?;

        let lock = self.lock_for(account).await;
        let _guard = lock.lock().await;

        // Re-read inside the lock. This is the line that makes two concurrent
        // callers cost one refresh: the second one arrives here after the
        // first has already written the new token to disk, sees a healthy
        // expiry, and returns without touching the network.
        let tokens = self.store.read(account)?;
        if !tokens.is_stale_at(now) {
            return Ok(tokens.access_token);
        }

        let response = match self.backend.refresh(&tokens.refresh_token).await {
            Ok(response) => response,
            Err(RefreshError::InvalidGrant) => {
                bail!(
                    "Google rejected the stored refresh token for account {account:?} \
                     (invalid_grant): it has been revoked, expired, or was issued to a \
                     different OAuth client. Re-authorise with:\n  {}",
                    authorize_command(account)
                );
            }
            // Deliberately unchanged, with no added context. A connection
            // reset is not a revoked grant, and dressing it up as one sends
            // the reader to a browser to fix their network.
            Err(RefreshError::Other(err)) => return Err(err),
        };

        let refreshed = Tokens {
            access_token: response.access_token,
            // Google returns no `refresh_token` on a refresh. Keeping the
            // existing one is not an optimisation: overwriting it with an
            // empty string would end the account's grant at the next restart.
            refresh_token: response.refresh_token.unwrap_or(tokens.refresh_token),
            expiry: now + chrono::Duration::seconds(response.expires_in),
            scope: response.scope.unwrap_or(tokens.scope),
        };

        // Persist before returning. A refresh that lives only in memory means
        // every daemon restart burns a fresh grant, and — worse — if Google
        // ever *does* rotate the refresh token, the rotated one would be lost
        // and the account would be dead.
        self.store.write(account, &refreshed)?;

        Ok(refreshed.access_token)
    }

    async fn lock_for(&self, account: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().await;
        Arc::clone(
            locks
                .entry(account.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Every test here is offline. The HTTP ones point a real `HttpRefreshBackend`
// at a `wiremock` server on loopback; the one test that needs a *failed*
// connection binds a port and immediately drops it. Nothing in this file can
// reach accounts.google.com.

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{body_string_contains, method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Distinctive, so `assert!(!text.contains(..))` means something.
    const REFRESH_TOKEN: &str = "1//REFRESH-SECRET-do-not-log-me";
    const ACCESS_TOKEN: &str = "ya29.ACCESS-SECRET-do-not-log-me";
    const CLIENT_SECRET: &str = "GOCSPX-CLIENT-SECRET-do-not-log-me";

    fn tokens_expiring_at(expiry: DateTime<Utc>) -> Tokens {
        Tokens {
            access_token: ACCESS_TOKEN.to_string(),
            refresh_token: REFRESH_TOKEN.to_string(),
            expiry,
            scope: SCOPES.join(" "),
        }
    }

    fn store_in(dir: &Path) -> TokenStore {
        TokenStore::new(Some(dir.to_path_buf()))
    }

    fn app_config_for(token_uri: &str) -> AppConfig {
        AppConfig {
            client_id: "123.apps.googleusercontent.com".to_string(),
            client_secret: CLIENT_SECRET.to_string(),
            redirect_uri: DEFAULT_REDIRECT_URI.to_string(),
            auth_uri: DEFAULT_AUTH_URI.to_string(),
            token_uri: token_uri.to_string(),
        }
    }

    fn json(body: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json")
    }

    // -----------------------------------------------------------------------
    // The token store
    // -----------------------------------------------------------------------

    #[test]
    fn tokens_round_trip_through_the_store() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let expiry = Utc::now() + chrono::Duration::minutes(45);
        let written = tokens_expiring_at(expiry);

        store.write("work", &written).unwrap();
        let read = store.read("work").unwrap();

        assert_eq!(read.access_token, written.access_token);
        assert_eq!(read.refresh_token, written.refresh_token);
        assert_eq!(read.scope, written.scope);
        // Through RFC 3339 and back; chrono keeps nanoseconds, so this is exact.
        assert_eq!(read.expiry, expiry);
    }

    #[test]
    fn a_written_token_file_is_mode_0600() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        store
            .write("work", &tokens_expiring_at(Utc::now()))
            .unwrap();

        let path = store.path_for("work").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} holds a Google refresh token and must be owner-read-write only",
            path.display()
        );
    }

    /// Overwriting must not loosen the mode either — the second write goes
    /// through a fresh temp file and a rename, so the mode comes from the temp
    /// file rather than from the file being replaced.
    #[test]
    fn overwriting_an_account_keeps_mode_0600_and_leaves_no_temp_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        store
            .write("work", &tokens_expiring_at(Utc::now()))
            .unwrap();
        store
            .write(
                "work",
                &tokens_expiring_at(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();

        let path = store.path_for("work").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn two_accounts_do_not_share_a_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        let mut work = tokens_expiring_at(Utc::now());
        work.access_token = "work-access".into();
        work.refresh_token = "work-refresh".into();
        let mut private = tokens_expiring_at(Utc::now());
        private.access_token = "private-access".into();
        private.refresh_token = "private-refresh".into();

        store.write("work", &work).unwrap();
        store.write("private", &private).unwrap();

        assert_eq!(store.read("work").unwrap().access_token, "work-access");
        assert_eq!(
            store.read("private").unwrap().access_token,
            "private-access"
        );
        assert_ne!(
            store.path_for("work").unwrap(),
            store.path_for("private").unwrap()
        );
    }

    #[test]
    fn list_returns_the_account_labels() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        store
            .write("work", &tokens_expiring_at(Utc::now()))
            .unwrap();
        store
            .write("private", &tokens_expiring_at(Utc::now()))
            .unwrap();
        // Neither of these is an account.
        std::fs::write(tmp.path().join(APP_CONFIG_FILE), "{}").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "hello").unwrap();

        assert_eq!(
            store.list().unwrap(),
            vec!["private".to_string(), "work".to_string()]
        );
    }

    #[test]
    fn list_is_empty_before_anything_is_authorised() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(&tmp.path().join("not-created-yet"));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn a_missing_account_names_the_account_and_the_authorize_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        let err = store.read("work").unwrap_err().to_string();

        assert!(err.contains("work"), "{err}");
        assert!(
            err.contains("authorize work"),
            "the message must be a command the reader can paste: {err}"
        );
        assert!(err.contains(tmp.path().to_str().unwrap()), "{err}");
    }

    /// The path-traversal case. Not theoretical: account labels arrive from
    /// `connector.toml` and from tool arguments a model writes.
    #[test]
    fn an_account_name_containing_a_slash_or_dot_dot_is_rejected_before_any_path_is_built() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tmp.path().join("google");
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&config).unwrap();
        std::fs::create_dir_all(&outside).unwrap();

        // A real, readable, perfectly-shaped token file outside the store. If
        // validation were missing, the traversal below would read it.
        let planted = outside.join("secret.json");
        let planted_body = serde_json::to_string(&tokens_expiring_at(Utc::now())).unwrap();
        std::fs::write(&planted, &planted_body).unwrap();
        std::fs::set_permissions(&planted, std::fs::Permissions::from_mode(0o600)).unwrap();

        let store = store_in(&config);

        for bad in [
            "../outside/secret",
            "..",
            "../..",
            "work/../../outside/secret",
            "/etc/passwd",
            "sub/work",
            ".hidden",
            "-leading-dash",
            "",
            "work name",
            "work\0",
            "work\n",
        ] {
            assert!(
                store.path_for(bad).is_err(),
                "path_for({bad:?}) must not produce a path"
            );

            let err = store.read(bad).unwrap_err().to_string();
            assert!(
                err.contains("^[a-z0-9][a-z0-9_-]*$"),
                "read({bad:?}) must fail validation, not the filesystem: {err}"
            );
            assert!(
                !err.contains(REFRESH_TOKEN),
                "read({bad:?}) leaked the planted token"
            );

            assert!(
                store.write(bad, &tokens_expiring_at(Utc::now())).is_err(),
                "write({bad:?}) must not produce a path"
            );
        }

        // And the planted file is untouched by all of that.
        assert_eq!(std::fs::read_to_string(&planted).unwrap(), planted_body);
        assert_eq!(store.list().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn ordinary_account_labels_are_accepted() {
        for good in ["work", "private", "work-2", "work_2", "a", "a1"] {
            validate_account(good).unwrap_or_else(|e| panic!("{good:?} should be valid: {e}"));
        }
    }

    /// The owner's volume is case-insensitive: `work` and `Work` would be two
    /// labels sharing `work.json`, so authorising the second would overwrite
    /// the first account's grant and a poll over both would report every
    /// message twice. The refusal has to name the lower-case label, because
    /// the person reading it is halfway through authorising.
    #[test]
    fn an_upper_case_account_label_is_refused_and_the_error_says_what_to_type() {
        for bad in ["Work", "WORK", "wOrk", "Private"] {
            let err = validate_account(bad).unwrap_err().to_string();
            assert!(
                err.contains("lower-case only"),
                "{bad:?} must be refused for its case, not obscurely: {err}"
            );
            assert!(
                err.contains(&format!("{:?}", bad.to_ascii_lowercase())),
                "the error must name the label to type instead: {err}"
            );
        }
    }

    #[test]
    fn a_corrupt_token_file_errors_with_the_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let path = store.path_for("work").unwrap();

        std::fs::write(&path, format!("{{\"accessToken\": \"{ACCESS_TOKEN}\"")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let err = store.read("work").unwrap_err().to_string();

        assert!(
            err.contains(path.to_str().unwrap()),
            "the path is the actionable part: {err}"
        );
        assert!(err.contains("authorize work"), "{err}");
        // The contents are not quoted — a malformed token file is exactly
        // where a token may be sitting somewhere unexpected.
        assert!(!err.contains(ACCESS_TOKEN), "corrupt file leaked a token");
    }

    #[test]
    fn a_world_readable_token_file_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store
            .write("work", &tokens_expiring_at(Utc::now()))
            .unwrap();
        let path = store.path_for("work").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = store.read("work").unwrap_err().to_string();

        assert!(err.contains("0644"), "{err}");
        assert!(err.contains("chmod 600"), "{err}");
        assert!(!err.contains(REFRESH_TOKEN));
    }

    // -----------------------------------------------------------------------
    // The app config
    // -----------------------------------------------------------------------

    #[test]
    fn app_config_loads_with_google_defaults_for_the_endpoints() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(APP_CONFIG_FILE);
        std::fs::write(
            &path,
            format!(
                "{{\"clientId\":\"123.apps.googleusercontent.com\",\
                  \"clientSecret\":\"{CLIENT_SECRET}\"}}"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let config = AppConfig::load_from(tmp.path()).unwrap();

        assert_eq!(config.auth_uri, DEFAULT_AUTH_URI);
        assert_eq!(config.token_uri, DEFAULT_TOKEN_URI);
        assert_eq!(config.redirect_uri, DEFAULT_REDIRECT_URI);
    }

    #[test]
    fn a_missing_app_config_says_how_to_create_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = AppConfig::load_from(tmp.path()).unwrap_err().to_string();
        assert!(err.contains(APP_CONFIG_FILE), "{err}");
        assert!(err.contains("console.cloud.google.com"), "{err}");
    }

    #[test]
    fn a_world_readable_app_config_is_refused_without_quoting_it() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join(APP_CONFIG_FILE);
        std::fs::write(
            &path,
            format!("{{\"clientId\":\"x\",\"clientSecret\":\"{CLIENT_SECRET}\"}}"),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = AppConfig::load_from(tmp.path()).unwrap_err().to_string();

        assert!(err.contains("0644"), "{err}");
        assert!(!err.contains(CLIENT_SECRET));
    }

    /// `access_type=offline` is what makes Google willing to issue a refresh
    /// token; `prompt=consent` is what makes it do so a second time. Without
    /// both, re-authorising an account silently yields no refresh token.
    #[test]
    fn the_authorization_url_forces_offline_access_and_a_fresh_consent() {
        let config = app_config_for(DEFAULT_TOKEN_URI);
        let url = config.authorization_url("state-123").unwrap();

        let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            params.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(params.get("prompt").map(String::as_str), Some("consent"));
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("state-123"));

        let scope = params.get("scope").unwrap();
        for wanted in SCOPES {
            assert!(
                scope.contains(wanted),
                "scope {scope:?} is missing {wanted}"
            );
        }

        // The consent URL is printed to a terminal and pasted into a browser.
        // The client secret has no business being in it.
        let text = url.to_string();
        assert!(!text.contains(CLIENT_SECRET), "{text}");
        assert!(!text.contains(REFRESH_TOKEN), "{text}");
    }

    // -----------------------------------------------------------------------
    // Refresh
    // -----------------------------------------------------------------------

    /// Counts calls and can be told to dawdle, so the concurrency test has a
    /// window in which both callers are genuinely in flight.
    struct CountingBackend {
        calls: AtomicUsize,
        delay: Duration,
    }

    impl CountingBackend {
        fn new(delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                delay,
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl RefreshBackend for CountingBackend {
        fn refresh<'a>(
            &'a self,
            _refresh_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<RefreshResponse, RefreshError>> + Send + 'a>>
        {
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::time::sleep(self.delay).await;
                Ok(RefreshResponse {
                    access_token: format!("refreshed-{n}"),
                    refresh_token: None,
                    expires_in: 3600,
                    scope: None,
                })
            })
        }
    }

    #[tokio::test]
    async fn a_healthy_token_is_returned_without_a_refresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        // Comfortably outside the 60-second margin.
        store
            .write(
                "work",
                &tokens_expiring_at(now + chrono::Duration::minutes(30)),
            )
            .unwrap();

        let backend = CountingBackend::new(Duration::ZERO);
        let auth = Auth::with_backend(store, Arc::clone(&backend) as Arc<dyn RefreshBackend>);

        let token = auth.access_token_at("work", now).await.unwrap();

        assert_eq!(token, ACCESS_TOKEN);
        assert_eq!(backend.calls(), 0, "a healthy token must not be refreshed");
    }

    /// The 60-second margin is the point of the test: a token that is still
    /// technically valid for another 30 seconds is refreshed anyway, because a
    /// request started now may arrive after it is not.
    #[tokio::test]
    async fn a_token_inside_the_sixty_second_margin_is_refreshed() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now + chrono::Duration::seconds(30)),
            )
            .unwrap();

        let backend = CountingBackend::new(Duration::ZERO);
        let auth = Auth::with_backend(store, Arc::clone(&backend) as Arc<dyn RefreshBackend>);

        assert_eq!(
            auth.access_token_at("work", now).await.unwrap(),
            "refreshed-1"
        );
        assert_eq!(backend.calls(), 1);
    }

    /// Review Focus #1. Asserted by reading the file back, not by trusting the
    /// returned value: a refresh that is not persisted means the next daemon
    /// restart re-authorises, and nothing in memory would show it.
    #[tokio::test]
    async fn a_near_expiry_token_is_refreshed_and_the_new_token_reaches_disk() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(json(
                r#"{"access_token":"ya29.NEW-ACCESS","expires_in":3599,
                    "scope":"https://www.googleapis.com/auth/calendar.readonly",
                    "token_type":"Bearer"}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let config = app_config_for(&format!("{}/token", server.uri()));
        let auth = Auth::new(config, store_in(tmp.path())).unwrap();

        let returned = auth.access_token_at("work", now).await.unwrap();
        assert_eq!(returned, "ya29.NEW-ACCESS");

        // The assertion that matters: read it back off the disk.
        let persisted = store.read("work").unwrap();
        assert_eq!(persisted.access_token, "ya29.NEW-ACCESS");
        assert_eq!(
            persisted.scope,
            "https://www.googleapis.com/auth/calendar.readonly"
        );
        assert_eq!(persisted.expiry, now + chrono::Duration::seconds(3599));
        assert!(!persisted.is_stale_at(now));

        // And the file is still owner-only after the rename.
        let mode = std::fs::metadata(store.path_for("work").unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);

        server.verify().await;
    }

    /// Google issues a refresh token once, at consent, and omits it from every
    /// later response. Taking the response at face value would blank it.
    #[tokio::test]
    async fn google_omitting_the_refresh_token_keeps_the_existing_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(json(
                r#"{"access_token":"ya29.NEW-ACCESS","expires_in":3599,"token_type":"Bearer"}"#,
            ))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let auth = Auth::new(
            app_config_for(&format!("{}/token", server.uri())),
            store_in(tmp.path()),
        )
        .unwrap();
        auth.access_token_at("work", now).await.unwrap();

        let persisted = store.read("work").unwrap();
        assert_eq!(
            persisted.refresh_token, REFRESH_TOKEN,
            "the long-lived grant must survive a response that does not repeat it"
        );
        // The response also omitted `scope`; the granted scope is not
        // narrowed by Google declining to restate it.
        assert_eq!(persisted.scope, SCOPES.join(" "));
    }

    /// A rotated refresh token, if Google ever sends one, must be kept —
    /// losing it would kill the account at the next refresh.
    #[tokio::test]
    async fn a_rotated_refresh_token_replaces_the_stored_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(json(
                r#"{"access_token":"ya29.NEW","refresh_token":"1//ROTATED",
                    "expires_in":3599,"token_type":"Bearer"}"#,
            ))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let auth = Auth::new(
            app_config_for(&format!("{}/token", server.uri())),
            store_in(tmp.path()),
        )
        .unwrap();
        auth.access_token_at("work", now).await.unwrap();

        assert_eq!(store.read("work").unwrap().refresh_token, "1//ROTATED");
    }

    /// Review Focus #2. The only fix for a revoked grant is a human at a
    /// browser, so the message has to be the command, not a diagnosis.
    #[tokio::test]
    async fn an_invalid_grant_becomes_a_message_containing_authorize_work() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400).set_body_raw(
                    r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
                    "application/json",
                ),
            )
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let auth = Auth::new(
            app_config_for(&format!("{}/token", server.uri())),
            store_in(tmp.path()),
        )
        .unwrap();

        let err = format!("{:#}", auth.access_token_at("work", now).await.unwrap_err());

        assert!(err.contains("authorize work"), "{err}");
        assert!(err.contains("invalid_grant"), "{err}");
        assert!(!err.contains(REFRESH_TOKEN), "the dead token leaked: {err}");
        assert!(
            !err.contains(CLIENT_SECRET),
            "the client secret leaked: {err}"
        );

        // The stored tokens are left alone: only a re-authorisation replaces
        // them, and deleting them here would lose the scope record too.
        assert_eq!(store.read("work").unwrap().refresh_token, REFRESH_TOKEN);
    }

    /// The other half of Review Focus #2, and the one that is easy to get
    /// wrong: every failure is not a revoked grant. Sending somebody to a
    /// browser because their network blipped trains them to re-authorise
    /// reflexively.
    #[tokio::test]
    async fn an_unrelated_transport_error_propagates_unchanged() {
        // Bind and immediately release a port, so the connection is refused
        // rather than merely slow. No server ever listens here.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_uri = format!("http://{}/token", dead.local_addr().unwrap());
        drop(dead);

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let auth = Auth::new(app_config_for(&dead_uri), store_in(tmp.path())).unwrap();
        let err = format!("{:#}", auth.access_token_at("work", now).await.unwrap_err());

        assert!(
            err.contains("token endpoint"),
            "the transport error must survive: {err}"
        );
        assert!(
            !err.contains("invalid_grant") && !err.contains("authorize work"),
            "a connection failure must not be reported as a revoked grant: {err}"
        );
        assert!(!err.contains(REFRESH_TOKEN), "{err}");
        assert!(!err.contains(CLIENT_SECRET), "{err}");
    }

    /// A 5xx is transient too, and must not be mislabelled either.
    #[tokio::test]
    async fn a_server_error_is_not_mislabelled_as_a_revoked_grant() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_raw("upstream down", "text/plain"))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let auth = Auth::new(
            app_config_for(&format!("{}/token", server.uri())),
            store_in(tmp.path()),
        )
        .unwrap();
        let err = format!("{:#}", auth.access_token_at("work", now).await.unwrap_err());

        assert!(err.contains("503"), "{err}");
        assert!(!err.contains("authorize work"), "{err}");
        assert!(!err.contains(REFRESH_TOKEN), "{err}");
    }

    /// Review Focus #5. Two callers, one expired token, one refresh — because
    /// the second waiter re-reads from disk inside the lock rather than
    /// repeating the work the first one already did. Two grants would waste
    /// one and, on a provider that rotates refresh tokens, invalidate the
    /// other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_concurrent_calls_on_an_expired_token_trigger_exactly_one_refresh() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::hours(2)),
            )
            .unwrap();

        // Long enough that the second caller is certainly waiting on the lock
        // while the first is in the backend.
        let backend = CountingBackend::new(Duration::from_millis(150));
        let auth = Arc::new(Auth::with_backend(
            store_in(tmp.path()),
            Arc::clone(&backend) as Arc<dyn RefreshBackend>,
        ));

        let a = tokio::spawn({
            let auth = Arc::clone(&auth);
            async move { auth.access_token_at("work", now).await }
        });
        let b = tokio::spawn({
            let auth = Arc::clone(&auth);
            async move { auth.access_token_at("work", now).await }
        });

        let (a, b) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());

        assert_eq!(
            backend.calls(),
            1,
            "two concurrent callers must cost exactly one refresh"
        );
        assert_eq!(a, "refreshed-1");
        assert_eq!(
            b, "refreshed-1",
            "the second caller must see the first's token"
        );
        assert_eq!(store.read("work").unwrap().access_token, "refreshed-1");
    }

    /// The flip side of the per-account mutex: it is per account. A slow
    /// refresh for `work` must not hold up `private`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_accounts_refresh_independently() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        for account in ["work", "private"] {
            store
                .write(
                    account,
                    &tokens_expiring_at(now - chrono::Duration::hours(2)),
                )
                .unwrap();
        }

        let backend = CountingBackend::new(Duration::from_millis(50));
        let auth = Arc::new(Auth::with_backend(
            store_in(tmp.path()),
            Arc::clone(&backend) as Arc<dyn RefreshBackend>,
        ));

        let a = tokio::spawn({
            let auth = Arc::clone(&auth);
            async move { auth.access_token_at("work", now).await }
        });
        let b = tokio::spawn({
            let auth = Arc::clone(&auth);
            async move { auth.access_token_at("private", now).await }
        });
        a.await.unwrap().unwrap();
        b.await.unwrap().unwrap();

        assert_eq!(backend.calls(), 2, "each account refreshes for itself");
        assert_ne!(
            store.read("work").unwrap().access_token,
            store.read("private").unwrap().access_token,
            "the two accounts must not have been handed the same token"
        );
    }

    #[tokio::test]
    async fn refreshing_an_account_that_was_never_authorised_names_the_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = CountingBackend::new(Duration::ZERO);
        let auth = Auth::with_backend(
            store_in(tmp.path()),
            Arc::clone(&backend) as Arc<dyn RefreshBackend>,
        );

        let err = auth.access_token("work").await.unwrap_err().to_string();

        assert!(err.contains("authorize work"), "{err}");
        assert_eq!(backend.calls(), 0);
    }

    #[tokio::test]
    async fn a_traversing_account_name_never_reaches_the_backend() {
        let tmp = tempfile::TempDir::new().unwrap();
        let backend = CountingBackend::new(Duration::ZERO);
        let auth = Auth::with_backend(
            store_in(tmp.path()),
            Arc::clone(&backend) as Arc<dyn RefreshBackend>,
        );

        let err = auth
            .access_token("../../.ssh/id_rsa")
            .await
            .unwrap_err()
            .to_string();

        assert!(err.contains("^[a-z0-9][a-z0-9_-]*$"), "{err}");
        assert_eq!(backend.calls(), 0);
    }

    // -----------------------------------------------------------------------
    // Token safety
    // -----------------------------------------------------------------------

    /// The derived `Debug` impls would put every secret in this crate into the
    /// first `tracing` line that ever formatted one. This test is the reason
    /// they are hand-written.
    #[test]
    fn no_debug_impl_in_this_crate_prints_a_secret() {
        let tokens = tokens_expiring_at(Utc::now());
        let config = app_config_for(DEFAULT_TOKEN_URI);
        let backend = HttpRefreshBackend::new(&config).unwrap();
        let response = RefreshResponse {
            access_token: ACCESS_TOKEN.to_string(),
            refresh_token: Some(REFRESH_TOKEN.to_string()),
            expires_in: 3599,
            scope: None,
        };

        let rendered = [
            format!("{tokens:?}"),
            format!("{tokens:#?}"),
            format!("{config:?}"),
            format!("{config:#?}"),
            format!("{backend:?}"),
            format!("{backend:#?}"),
            format!("{response:?}"),
            format!("{response:#?}"),
        ];

        for text in &rendered {
            for secret in [ACCESS_TOKEN, REFRESH_TOKEN, CLIENT_SECRET] {
                assert!(
                    !text.contains(secret),
                    "{secret} leaked through Debug: {text}"
                );
            }
            assert!(text.contains("<redacted>"), "{text}");
        }
    }

    /// The one response body that must never be quoted: a 2xx that will not
    /// parse is still a *success* body, so it is the document that carries the
    /// tokens.
    #[tokio::test]
    async fn an_unparseable_success_body_is_reported_without_being_quoted() {
        let server = MockServer::start().await;
        // Shaped like a token response, missing `expires_in`, so it parses as
        // JSON but not as a `RefreshResponse`.
        Mock::given(method("POST"))
            .respond_with(json(&format!(
                r#"{{"access_token":"{ACCESS_TOKEN}","refresh_token":"{REFRESH_TOKEN}"}}"#
            )))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        let now = Utc::now();
        store
            .write(
                "work",
                &tokens_expiring_at(now - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let auth = Auth::new(
            app_config_for(&format!("{}/token", server.uri())),
            store_in(tmp.path()),
        )
        .unwrap();
        let err = auth.access_token_at("work", now).await.unwrap_err();

        // Both `Display` and `Debug` — `anyhow`'s `Debug` prints the whole
        // chain, and a `?err` in a tracing macro is the realistic mistake.
        for text in [format!("{err:#}"), format!("{err:?}")] {
            assert!(!text.contains(ACCESS_TOKEN), "{text}");
            assert!(!text.contains(REFRESH_TOKEN), "{text}");
            assert!(text.contains("not the expected JSON"), "{text}");
        }
    }

    /// `is_invalid_grant` matches the parsed `error` field, so an
    /// `error_description` that merely says the words is not mistaken for one.
    #[test]
    fn only_the_error_field_makes_a_response_an_invalid_grant() {
        assert!(is_invalid_grant(r#"{"error":"invalid_grant"}"#));
        assert!(!is_invalid_grant(
            r#"{"error":"temporarily_unavailable","error_description":"not invalid_grant"}"#
        ));
        assert!(!is_invalid_grant("invalid_grant"));
        assert!(!is_invalid_grant("<html>502</html>"));
        assert!(!is_invalid_grant(""));
    }

    #[test]
    fn the_scope_list_is_the_three_the_project_asked_for() {
        assert_eq!(
            SCOPES,
            [
                "https://www.googleapis.com/auth/calendar.readonly",
                "https://www.googleapis.com/auth/gmail.readonly",
                "https://www.googleapis.com/auth/gmail.compose",
            ]
        );
        // Nothing here may ask for write access to the calendar or for
        // `gmail.send`; `gmail.compose` is the widest scope this project takes.
        assert!(!SCOPES.iter().any(|s| s.ends_with("gmail.send")));
    }
}
