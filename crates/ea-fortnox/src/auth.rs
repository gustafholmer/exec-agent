//! Fortnox OAuth 2.0: the authorization URL, the code exchange, the token
//! file, and the manager that hands out a live access token.
//!
//! # The refresh token rotates, and that is the whole point of this file
//!
//! Google issues a refresh token once and never mentions it again. Fortnox
//! does the opposite: **every** refresh returns a *new* refresh token and
//! kills the one that was used. Two consequences follow, and both of them
//! have already cost this project a working integration once.
//!
//! 1. A rotation that is not persisted is fatal, silently and later. The
//!    access token that came back with it works for an hour, so nothing
//!    fails today; the next refresh presents a refresh token Fortnox retired
//!    an hour ago and the integration is dead with no signal but a 400.
//!    [`TokenManager`] therefore persists **before** it returns, and a failure
//!    to persist is a loud, specific error rather than a warning — at that
//!    point the grant really is gone and only a person at a browser can fix
//!    it.
//! 2. A refresh token unused for 45 days lapses. Nothing here can prevent
//!    that; what it can do is make the resulting message say "re-run the
//!    authorize command" rather than "400 Bad Request".
//!
//! # Credentials
//!
//! Same standard as `ea-google`'s `auth` and `ea-canvas`'s client, because
//! this one holds the keys to a company's accounting system:
//!
//! * [`StoredTokens`] and [`FortnoxTokenResponse`] have hand-written `Debug`
//!   impls printing `<redacted>`; so does [`OAuthClient`], which holds the
//!   client secret. No derived `Debug` anywhere near a secret.
//! * Credentials travel in headers and form bodies, never in a URL, and every
//!   `reqwest` error goes through `.without_url()`.
//! * The token endpoint's *error* bodies are quoted (truncated); its *success*
//!   bodies never are, because a success body is exactly the document holding
//!   both tokens. A success body that will not parse is reported by its length
//!   and the serde error's classification, not its text.
//! * The HTTP client follows no redirects. `reqwest`'s default policy strips
//!   the `Authorization` header across origins but knows nothing about a
//!   request *body*, and this crate's token requests carry the client secret
//!   and the refresh token in the body.
//! * The token file is mode `0600` and is written atomically.

use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};

use crate::errors::{snippet, FortnoxError};

/// The connector's name — the config directory uses it.
pub const CONNECTOR: &str = "fortnox";

/// The token file inside the connector's config directory.
pub const TOKENS_FILE: &str = "tokens.json";

/// Fortnox's consent endpoint.
pub const AUTH_URL: &str = "https://apps.fortnox.se/oauth-v1/auth";

/// Fortnox's token endpoint: code exchange and refresh both POST here.
pub const TOKEN_URL: &str = "https://apps.fortnox.se/oauth-v1/token";

/// How long before an access token's stated expiry to refresh it anyway. A
/// request begun at expiry-minus-nothing can easily arrive after it.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// Per-request deadline for the token endpoint. Same value and reasoning as
/// every other HTTP client in this workspace.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// The exact command a person should run to (re-)authorise Fortnox. Every
/// message about a missing, unreadable or refused grant ends with this.
///
/// The binary itself arrives with the connector (a later task); the string is
/// fixed here so that every error message in this crate names the same one.
pub const AUTHORIZE_COMMAND: &str = "ea-fortnox-authorize";

// ---------------------------------------------------------------------------
// The authorization URL
// ---------------------------------------------------------------------------

/// The inputs to [`build_authorize_url`].
#[derive(Debug, Clone, Copy)]
pub struct AuthorizeParams<'a> {
    pub client_id: &'a str,
    pub redirect_uri: &'a str,
    /// Fortnox scopes, e.g. `bookkeeping`. Joined with spaces on the wire.
    pub scopes: &'a [&'a str],
    /// The CSRF token the callback must echo back.
    pub state: &'a str,
}

/// The URL to send the owner's browser to.
///
/// Nothing secret goes in it: the client *id* is public by design, and the
/// client secret is only ever presented at the token endpoint.
pub fn build_authorize_url(params: &AuthorizeParams<'_>) -> Result<Url, FortnoxError> {
    let mut url = Url::parse(AUTH_URL).map_err(|err| {
        FortnoxError::Auth(format!("the Fortnox authorize URL is not a URL ({err})"))
    })?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("client_id", params.client_id);
        pairs.append_pair("response_type", "code");
        pairs.append_pair("state", params.state);
        // Without this Fortnox issues no refresh token at all and the
        // integration lasts exactly one hour.
        pairs.append_pair("access_type", "offline");
        pairs.append_pair("scope", &params.scopes.join(" "));
        pairs.append_pair("redirect_uri", params.redirect_uri);
    }
    Ok(url)
}

// ---------------------------------------------------------------------------
// The tokens
// ---------------------------------------------------------------------------

/// The OAuth state as persisted by a [`TokenStore`].
///
/// No `Debug` derive: two of the four fields are secrets.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredTokens {
    /// The bearer token. Fortnox's last an hour.
    pub access_token: String,
    /// The grant. Fortnox replaces this on **every** refresh — see the module
    /// docs — and retires the one presented.
    pub refresh_token: String,
    /// When [`StoredTokens::access_token`] stops working.
    pub expires_at: DateTime<Utc>,
    /// The space-separated scopes Fortnox actually granted.
    pub scope: String,
}

impl fmt::Debug for StoredTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

impl StoredTokens {
    /// True when the access token is expired, or close enough to it that a
    /// request started now might arrive after it is.
    pub fn is_stale_at(&self, now: DateTime<Utc>) -> bool {
        let margin = chrono::Duration::from_std(EXPIRY_MARGIN)
            .unwrap_or_else(|_| chrono::Duration::seconds(60));
        self.expires_at <= now + margin
    }
}

/// What Fortnox's token endpoint answers a code exchange or a refresh with.
///
/// `refresh_token` is **not** optional, unlike Google's. Fortnox always sends
/// one, and a response without one would mean the rotation happened and we
/// cannot know the new value — better to fail the refresh loudly than to
/// persist an empty string.
///
/// No `Debug` derive: see the module docs.
#[derive(Clone, Deserialize)]
pub struct FortnoxTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until `access_token` expires.
    pub expires_in: i64,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub token_type: String,
}

impl fmt::Debug for FortnoxTokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FortnoxTokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .field("scope", &self.scope)
            .field("token_type", &self.token_type)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

/// Where tokens live. File-backed here; the trait exists so [`TokenManager`]
/// can be tested without a filesystem, and so a future deployment can put
/// them somewhere else.
///
/// Synchronous on purpose: every implementation is a small local read or
/// write, and a blocking trait keeps `TokenManager`'s locking honest — the
/// store is never touched across a network await.
pub trait TokenStore: Send + Sync {
    /// The stored tokens, or `None` if there are none *or* they are
    /// unreadable. An error means the storage itself failed.
    fn load(&self) -> anyhow::Result<Option<StoredTokens>>;
    /// Persist, durably and atomically.
    fn save(&self, tokens: &StoredTokens) -> anyhow::Result<()>;
}

/// One JSON file, mode `0600`, written atomically.
#[derive(Debug, Clone)]
pub struct FileTokenStore {
    path: PathBuf,
}

impl FileTokenStore {
    /// A store backed by `path`. The parent directory is created on first
    /// save.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// `~/.config/exec-agent/fortnox/tokens.json` (or under `$EA_CONFIG_DIR`).
    pub fn default_path() -> PathBuf {
        ea_core::paths::connector_config_dir(CONNECTOR).join(TOKENS_FILE)
    }

    /// A store at [`FileTokenStore::default_path`].
    pub fn at_default_path() -> Self {
        Self::new(Self::default_path())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl TokenStore for FileTokenStore {
    /// Read the tokens.
    ///
    /// A missing file is `None`: nothing has been authorised yet. A file that
    /// will not parse is **also** `None`, deliberately — logged at `warn` and
    /// treated as absent. The alternative is an error on every single call,
    /// which turns one bad file into a daemon that crash-loops forever;
    /// `None` reaches the caller as "no stored Fortnox tokens, run the
    /// authorize command", which is both true and actionable.
    ///
    /// Neither the file's contents nor the `serde` message's text is logged:
    /// a half-written token file is exactly where a token may be sitting.
    fn load(&self) -> anyhow::Result<Option<StoredTokens>> {
        use std::os::unix::fs::PermissionsExt;

        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.path.display()));
            }
        };

        // A loose mode is a warning, not a refusal: refusing would deny
        // service over something the owner can fix with one command, and the
        // token has already been readable for however long it has been there.
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                tracing::warn!(
                    path = %self.path.display(),
                    mode = format!("{mode:04o}"),
                    "the Fortnox token file is readable by other users; run: chmod 600 {}",
                    self.path.display()
                );
            }
        }

        match serde_json::from_str::<StoredTokens>(&text) {
            Ok(tokens) => Ok(Some(tokens)),
            Err(err) => {
                tracing::warn!(
                    path = %self.path.display(),
                    problem = ?err.classify(),
                    line = err.line(),
                    column = err.column(),
                    "the Fortnox token file is not readable JSON and is being ignored; \
                     re-authorise with `{AUTHORIZE_COMMAND}` (its contents are not logged: \
                     it may hold a token)"
                );
                Ok(None)
            }
        }
    }

    /// Write the tokens atomically, mode `0600`.
    ///
    /// Temp file in the same directory (so `rename` stays on one filesystem
    /// and is therefore atomic), `fsync`, then `rename`. A crash at any point
    /// leaves either the old file or the new one, never a truncated one —
    /// and with a rotating refresh token, a truncated file is a dead grant.
    fn save(&self, tokens: &StoredTokens) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let dir = self
            .path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));

        let stem = self
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| TOKENS_FILE.to_string());
        // Unique per process and per call: two daemons must not share a temp
        // file and interleave their bytes.
        let tmp = dir.join(format!(
            ".{stem}.tmp.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));

        // 0600 from the moment the file exists, not set afterwards — which
        // would leave a window in which the refresh token is world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;

        let body = serde_json::to_vec_pretty(tokens).context("serialising the Fortnox tokens")?;

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

        // `create_new(..).mode(..)` is masked by the process umask, so this
        // re-assertion is what actually guarantees 0600; `rename` preserves
        // whatever mode the temp file has.
        if let Err(err) = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).with_context(|| format!("setting mode 0600 on {}", tmp.display()));
        }

        if let Err(err) = std::fs::rename(&tmp, &self.path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(err).with_context(|| {
                format!("renaming {} onto {}", tmp.display(), self.path.display())
            });
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The refresh grant
// ---------------------------------------------------------------------------

/// The refresh half of OAuth, behind a trait so [`TokenManager`] can be
/// tested without HTTP.
///
/// Hand-rolled boxed future rather than `async fn` in trait: `TokenManager`
/// holds an `Arc<dyn RefreshBackend>`, and async-fn-in-trait is not
/// `dyn`-compatible.
pub trait RefreshBackend: Send + Sync {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<FortnoxTokenResponse, FortnoxError>> + Send + 'a>>;
}

/// The real token endpoint.
///
/// No `Debug` derive — it holds the client secret.
#[derive(Clone)]
pub struct OAuthClient {
    http: reqwest::Client,
    token_url: String,
    client_id: String,
    client_secret: String,
}

impl fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthClient")
            .field("token_url", &self.token_url)
            .field("client_id", &self.client_id)
            .field("client_secret", &"<redacted>")
            .finish()
    }
}

impl OAuthClient {
    /// Against the real Fortnox token endpoint.
    pub fn new(client_id: &str, client_secret: &str) -> Result<Self, FortnoxError> {
        Self::with_token_url(client_id, client_secret, TOKEN_URL)
    }

    /// Against an explicit token endpoint. The seam the tests point at a
    /// `wiremock` server.
    pub fn with_token_url(
        client_id: &str,
        client_secret: &str,
        token_url: &str,
    ) -> Result<Self, FortnoxError> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            // See the module docs: the body carries the client secret and the
            // refresh token, and no redirect policy protects a body.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|err| {
                FortnoxError::Transport(format!(
                    "building the HTTP client for Fortnox's token endpoint failed: {}",
                    err.without_url()
                ))
            })?;
        Ok(Self {
            http,
            token_url: token_url.to_string(),
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
        })
    }

    /// Exchange an authorization code for the first pair of tokens.
    pub async fn exchange_code(
        &self,
        code: &str,
        redirect_uri: &str,
    ) -> Result<FortnoxTokenResponse, FortnoxError> {
        self.post_form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
        ])
        .await
    }

    /// Present the stored refresh token and get a rotated pair back.
    ///
    /// The refresh token presented here is **dead** the moment Fortnox
    /// answers 200, whatever the caller then does with the reply. See
    /// [`TokenManager`], which is the only thing that should call this in
    /// anger.
    pub async fn refresh_tokens(
        &self,
        refresh_token: &str,
    ) -> Result<FortnoxTokenResponse, FortnoxError> {
        self.post_form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .await
    }

    async fn post_form(&self, form: &[(&str, &str)]) -> Result<FortnoxTokenResponse, FortnoxError> {
        let response = self
            .http
            .post(&self.token_url)
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .header(reqwest::header::ACCEPT, "application/json")
            .form(form)
            .send()
            .await
            // `.without_url()` on every `reqwest` error, as a habit rather
            // than because this URL is secret: the habit is what keeps a
            // future endpoint with a token in its path from leaking.
            .map_err(|err| {
                FortnoxError::Transport(format!(
                    "POST to Fortnox's token endpoint failed: {}",
                    err.without_url()
                ))
            })?;

        let status = response.status();

        if !status.is_success() {
            // Safe to quote, truncated: by definition an error response did
            // not carry a token.
            let body = response.text().await.unwrap_or_default();
            return Err(FortnoxError::Api {
                status: status.as_u16(),
                body: snippet(&body),
            });
        }

        let body = response.text().await.map_err(|err| {
            FortnoxError::Transport(format!(
                "reading Fortnox's token endpoint reply failed: {}",
                err.without_url()
            ))
        })?;

        // The body is NOT quoted here, and neither is the `serde` message's
        // text — only its classification and position. This is the success
        // path, so the body is exactly the document holding the access token
        // and the rotated refresh token.
        serde_json::from_str::<FortnoxTokenResponse>(&body).map_err(|err| FortnoxError::Api {
            status: status.as_u16(),
            body: format!(
                "the token endpoint answered {} bytes that are not the expected JSON \
                 ({:?} at line {}, column {}). The body is deliberately not quoted: a \
                 success response carries both tokens.",
                body.len(),
                err.classify(),
                err.line(),
                err.column()
            ),
        })
    }
}

impl RefreshBackend for OAuthClient {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<FortnoxTokenResponse, FortnoxError>> + Send + 'a>> {
        Box::pin(self.refresh_tokens(refresh_token))
    }
}

// ---------------------------------------------------------------------------
// The token manager
// ---------------------------------------------------------------------------

/// Hands out a live access token, refreshing and persisting as needed.
///
/// No `Debug` impl at all: it reaches a token store and a backend holding the
/// client secret, and it has no business in a log line.
pub struct TokenManager {
    store: Arc<dyn TokenStore>,
    refresh: Arc<dyn RefreshBackend>,
    /// Held across the whole read-refresh-save sequence. Two callers that
    /// arrive together cost one refresh: the second takes the lock after the
    /// first has already written, re-reads, and sees a healthy token.
    ///
    /// This replaces the upstream's shared in-flight promise. The observable
    /// behaviour is the same and a mutex does not need a cancellation story.
    gate: tokio::sync::Mutex<()>,
    /// Bumped on every completed refresh. A `force` caller that arrives while
    /// another refresh is in flight compares this against what it saw before
    /// queueing: if it changed, somebody else has already done the forced
    /// refresh it wanted, and doing a second one would be the thundering herd
    /// against a revoked grant that this whole design exists to avoid.
    generation: AtomicU64,
}

impl TokenManager {
    pub fn new(store: Arc<dyn TokenStore>, refresh: Arc<dyn RefreshBackend>) -> Self {
        Self {
            store,
            refresh,
            gate: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(0),
        }
    }

    /// A usable access token.
    ///
    /// `force` skips the not-yet-expired fast path: the client passes it after
    /// a 401, because Fortnox can retire a token long before its stated
    /// expiry.
    pub async fn access_token(&self, force: bool) -> Result<String, FortnoxError> {
        self.access_token_at(force, Utc::now()).await
    }

    /// [`TokenManager::access_token`] against an explicit clock, so the expiry
    /// tests do not race the wall clock.
    pub async fn access_token_at(
        &self,
        force: bool,
        now: DateTime<Utc>,
    ) -> Result<String, FortnoxError> {
        // Read before queueing, compare after: see `generation`.
        let seen = self.generation.load(Ordering::SeqCst);
        let _guard = self.gate.lock().await;
        let refreshed_while_waiting = self.generation.load(Ordering::SeqCst) != seen;

        // Read inside the lock. This is the line that makes three concurrent
        // callers cost one refresh: the second arrives after the first has
        // written, sees a healthy expiry, and never touches the network.
        let stored = self.store.load().map_err(|err| {
            FortnoxError::Auth(format!(
                "the stored Fortnox tokens could not be read ({err:#}). {}",
                rerun_authorize()
            ))
        })?;
        let Some(stored) = stored else {
            return Err(FortnoxError::Auth(format!(
                "No stored Fortnox tokens. {}",
                rerun_authorize()
            )));
        };

        if (!force || refreshed_while_waiting) && !stored.is_stale_at(now) {
            return Ok(stored.access_token);
        }

        let response = self
            .refresh
            .refresh(&stored.refresh_token)
            .await
            .map_err(|err| {
                FortnoxError::Auth(format!(
                    "the Fortnox token refresh failed ({err}). A refresh token unused for \
                     45 days lapses, and Fortnox retires the one it was last presented. {}",
                    rerun_authorize()
                ))
            })?;

        // An EMPTY rotation is not a rotation. `FortnoxTokenResponse` requires
        // the field, but `""` deserialises happily, and saving it would
        // overwrite the one credential that keeps this integration alive with
        // nothing. The grant is already in trouble either way — Fortnox has
        // retired the presented token — but the two outcomes are not equal:
        // persisting the blank hides the breakage until the access token
        // expires an hour later and the failure surfaces somewhere unrelated,
        // which is exactly how this owner's last grant lapsed unnoticed.
        // Refuse, keep what is stored, and say so now.
        // `bin/authorize.rs` guards the same case on the initial exchange.
        if response.refresh_token.trim().is_empty() {
            return Err(FortnoxError::Auth(format!(
                "Fortnox answered the refresh with an EMPTY refresh token. Nothing was \
                 saved — overwriting the stored one with a blank would kill the grant \
                 silently. The stored refresh token is the one Fortnox just retired, so \
                 the integration is down until someone re-authorises. {}",
                rerun_authorize()
            )));
        }

        let updated = StoredTokens {
            access_token: response.access_token,
            // ROTATED. Fortnox has just retired `stored.refresh_token`; if
            // this value is not persisted the grant is gone at the next
            // refresh, an hour or a restart from now.
            refresh_token: response.refresh_token,
            expires_at: now + chrono::Duration::seconds(response.expires_in),
            scope: if response.scope.is_empty() {
                stored.scope
            } else {
                response.scope
            },
        };

        // Persist BEFORE returning, and treat a failure as fatal rather than
        // as a warning: at this point Fortnox has already rotated, the old
        // refresh token is dead on their side, and the new one exists only in
        // this stack frame.
        self.store.save(&updated).map_err(|err| {
            FortnoxError::Auth(format!(
                "Fortnox rotated the refresh token but the NEW token could NOT be saved \
                 ({err:#}). The previous token is now invalid on Fortnox's side, so the \
                 integration is down until someone re-authorises. {}",
                rerun_authorize()
            ))
        })?;

        self.generation.fetch_add(1, Ordering::SeqCst);
        Ok(updated.access_token)
    }
}

/// The sentence every unrecoverable auth failure ends with.
fn rerun_authorize() -> String {
    format!("Re-authorise with:\n  {AUTHORIZE_COMMAND}")
}

// ---------------------------------------------------------------------------
// Test doubles, shared with `crate::client`'s tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    // -----------------------------------------------------------------------
    // Test doubles
    // -----------------------------------------------------------------------

    /// An in-memory store, plus a switch to make `save` fail.
    pub(crate) struct MemStore {
        current: Mutex<Option<StoredTokens>>,
        pub(crate) saves: AtomicUsize,
        fail_save: bool,
    }

    impl MemStore {
        pub(crate) fn new(initial: Option<StoredTokens>) -> Arc<Self> {
            Arc::new(Self {
                current: Mutex::new(initial),
                saves: AtomicUsize::new(0),
                fail_save: false,
            })
        }

        pub(crate) fn failing_save(initial: Option<StoredTokens>) -> Arc<Self> {
            Arc::new(Self {
                current: Mutex::new(initial),
                saves: AtomicUsize::new(0),
                fail_save: true,
            })
        }

        pub(crate) fn current(&self) -> Option<StoredTokens> {
            self.current.lock().expect("mem store").clone()
        }
    }

    impl TokenStore for MemStore {
        fn load(&self) -> anyhow::Result<Option<StoredTokens>> {
            Ok(self.current.lock().expect("mem store").clone())
        }

        fn save(&self, tokens: &StoredTokens) -> anyhow::Result<()> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            if self.fail_save {
                anyhow::bail!("disk full");
            }
            *self.current.lock().expect("mem store") = Some(tokens.clone());
            Ok(())
        }
    }

    /// Counts refreshes, records the refresh tokens it was presented, and
    /// hands back a rotated pair.
    pub(crate) struct CountingRefresh {
        calls: AtomicUsize,
        presented: Mutex<Vec<String>>,
        fail: bool,
        /// Makes concurrent callers actually overlap.
        delay: Duration,
        /// What the rotation hands back as the new refresh token.
        rotated: &'static str,
    }

    impl CountingRefresh {
        pub(crate) fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                presented: Mutex::new(Vec::new()),
                fail: false,
                delay: Duration::ZERO,
                rotated: "rotatedRefresh",
            })
        }

        pub(crate) fn slow() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                presented: Mutex::new(Vec::new()),
                fail: false,
                delay: Duration::from_millis(50),
                rotated: "rotatedRefresh",
            })
        }

        pub(crate) fn failing() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                presented: Mutex::new(Vec::new()),
                fail: true,
                delay: Duration::ZERO,
                rotated: "rotatedRefresh",
            })
        }

        /// A 200 whose `refresh_token` is blank. Fortnox is not supposed to
        /// do this; the guard in `access_token_at` exists because nothing
        /// stops it from deserialising if it does.
        pub(crate) fn blank_rotation() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                presented: Mutex::new(Vec::new()),
                fail: false,
                delay: Duration::ZERO,
                rotated: "   ",
            })
        }

        pub(crate) fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        pub(crate) fn presented(&self) -> Vec<String> {
            self.presented.lock().expect("presented").clone()
        }
    }

    impl RefreshBackend for CountingRefresh {
        fn refresh<'a>(
            &'a self,
            refresh_token: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<FortnoxTokenResponse, FortnoxError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.presented
                    .lock()
                    .expect("presented")
                    .push(refresh_token.to_string());
                if !self.delay.is_zero() {
                    tokio::time::sleep(self.delay).await;
                }
                if self.fail {
                    return Err(FortnoxError::Api {
                        status: 400,
                        body: "invalid_grant".to_string(),
                    });
                }
                Ok(FortnoxTokenResponse {
                    access_token: "newAccess".to_string(),
                    refresh_token: self.rotated.to_string(),
                    expires_in: 3600,
                    scope: "bookkeeping".to_string(),
                    token_type: "bearer".to_string(),
                })
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Every test here is offline. The HTTP ones point a real `OAuthClient` at a
// `wiremock` server on loopback; the connection-failure test binds a port and
// drops it. Nothing in this file can reach apps.fortnox.se.

#[cfg(test)]
mod tests {
    use super::*;

    use super::test_support::{CountingRefresh, MemStore};

    use std::collections::HashMap;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("test timestamp")
            .with_timezone(&Utc)
    }

    fn tokens(access: &str, refresh: &str, expires_at: &str) -> StoredTokens {
        StoredTokens {
            access_token: access.to_string(),
            refresh_token: refresh.to_string(),
            expires_at: at(expires_at),
            scope: "bookkeeping".to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // buildAuthorizeUrl — translated from `auth/oauth.test.ts`
    // -----------------------------------------------------------------------

    #[test]
    fn the_authorize_url_carries_every_required_param_and_space_delimited_scopes() {
        let url = build_authorize_url(&AuthorizeParams {
            client_id: "cid",
            redirect_uri: "http://localhost:8910/callback",
            scopes: &["bookkeeping", "invoice"],
            state: "st8",
        })
        .expect("a static URL parses");

        assert_eq!(
            format!(
                "{}://{}{}",
                url.scheme(),
                url.host_str().unwrap_or(""),
                url.path()
            ),
            "https://apps.fortnox.se/oauth-v1/auth"
        );
        let params: HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert_eq!(params.get("client_id").map(String::as_str), Some("cid"));
        assert_eq!(
            params.get("response_type").map(String::as_str),
            Some("code")
        );
        assert_eq!(params.get("state").map(String::as_str), Some("st8"));
        assert_eq!(
            params.get("access_type").map(String::as_str),
            Some("offline")
        );
        assert_eq!(
            params.get("scope").map(String::as_str),
            Some("bookkeeping invoice")
        );
        assert_eq!(
            params.get("redirect_uri").map(String::as_str),
            Some("http://localhost:8910/callback")
        );
    }

    // -----------------------------------------------------------------------
    // exchangeCode / refreshTokens — translated from `auth/oauth.test.ts`
    // -----------------------------------------------------------------------

    fn token_json(access: &str, refresh: &str) -> String {
        format!(
            r#"{{"access_token":"{access}","refresh_token":"{refresh}","expires_in":3600,"scope":"bookkeeping","token_type":"bearer"}}"#
        )
    }

    /// Parse an `application/x-www-form-urlencoded` request body.
    fn form_pairs(body: &[u8]) -> HashMap<String, String> {
        let text = std::str::from_utf8(body).expect("the form body is UTF-8");
        Url::parse(&format!("http://form.invalid/?{text}"))
            .expect("a form body parses as a query string")
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect()
    }

    #[tokio::test]
    async fn exchange_code_posts_a_basic_authed_form_and_returns_the_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth-v1/token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(token_json("at", "rt"), "application/json"),
            )
            .mount(&server)
            .await;

        let client =
            OAuthClient::with_token_url("cid", "sec", &format!("{}/oauth-v1/token", server.uri()))
                .expect("client");
        let res = client
            .exchange_code("theCode", "http://localhost:8910/callback")
            .await
            .expect("the exchange succeeds");

        assert_eq!(res.access_token, "at");
        assert_eq!(res.refresh_token, "rt");

        let requests = server.received_requests().await.expect("requests");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(
            request
                .headers
                .get("authorization")
                .and_then(|v| v.to_str().ok()),
            // base64("cid:sec")
            Some("Basic Y2lkOnNlYw==")
        );
        assert_eq!(
            request
                .headers
                .get("content-type")
                .and_then(|v| v.to_str().ok()),
            Some("application/x-www-form-urlencoded")
        );
        let body = form_pairs(&request.body);
        assert_eq!(
            body.get("grant_type").map(String::as_str),
            Some("authorization_code")
        );
        assert_eq!(body.get("code").map(String::as_str), Some("theCode"));
        assert_eq!(
            body.get("redirect_uri").map(String::as_str),
            Some("http://localhost:8910/callback")
        );
    }

    #[tokio::test]
    async fn exchange_code_errors_on_a_non_2xx_and_quotes_the_error_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_raw(r#"{"error":"invalid_grant"}"#, "application/json"),
            )
            .mount(&server)
            .await;

        let client = OAuthClient::with_token_url("cid", "sec", &server.uri()).expect("client");
        let err = client
            .exchange_code("x", "http://localhost:8910/callback")
            .await
            .expect_err("a 400 is an error");

        assert_eq!(err.status(), Some(400));
        assert!(err.to_string().contains("invalid_grant"), "{err}");
    }

    #[tokio::test]
    async fn refresh_posts_grant_type_refresh_token_and_returns_the_rotated_pair() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(token_json("at2", "rt2"), "application/json"),
            )
            .mount(&server)
            .await;

        let client = OAuthClient::with_token_url("cid", "sec", &server.uri()).expect("client");
        let res = client.refresh_tokens("rtOld").await.expect("refresh");

        assert_eq!(res.refresh_token, "rt2");
        assert_eq!(res.access_token, "at2");

        let requests = server.received_requests().await.expect("requests");
        let body = form_pairs(&requests[0].body);
        assert_eq!(
            body.get("grant_type").map(String::as_str),
            Some("refresh_token")
        );
        assert_eq!(body.get("refresh_token").map(String::as_str), Some("rtOld"));
    }

    // ---- Beyond the upstream file -----------------------------------------

    /// A 200 that is not the JSON we expect must be reported without its
    /// body: a 200 from the token endpoint is the one document that holds
    /// both tokens.
    #[tokio::test]
    async fn an_unparseable_success_body_is_reported_without_being_quoted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"access_token":"SUPERSECRET","expires_in":"not a number"}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let client = OAuthClient::with_token_url("cid", "sec", &server.uri()).expect("client");
        let err = client
            .refresh_tokens("rtOld")
            .await
            .expect_err("unparseable");

        let rendered = format!("{err} / {err:?}");
        assert!(
            !rendered.contains("SUPERSECRET"),
            "the success body must never be quoted: {rendered}"
        );
        assert!(rendered.contains("not quoted"), "{rendered}");
    }

    #[tokio::test]
    async fn a_connection_failure_is_a_transport_error_and_names_no_credential() {
        // Bind a port, learn its number, drop it: nothing is listening there.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);

        let client =
            OAuthClient::with_token_url("cid", "sec", &format!("http://127.0.0.1:{port}/token"))
                .expect("client");
        let err = client
            .refresh_tokens("rtOldSecret")
            .await
            .expect_err("nothing is listening");

        assert!(
            matches!(err, FortnoxError::Transport(_)),
            "a dead connection is transport, not API: {err:?}"
        );
        let rendered = format!("{err} / {err:?}");
        assert!(!rendered.contains("rtOldSecret"), "{rendered}");
        assert!(!rendered.contains("sec\""), "{rendered}");
    }

    /// `reqwest`'s default policy would re-POST the client secret and the
    /// refresh token to whatever the server pointed at.
    #[tokio::test]
    async fn a_redirect_from_the_token_endpoint_is_not_followed() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(307).insert_header("location", elsewhere.uri().as_str()),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(token_json("leaked", "leaked"), "application/json"),
            )
            .mount(&elsewhere)
            .await;

        let client = OAuthClient::with_token_url("cid", "sec", &server.uri()).expect("client");
        let err = client
            .refresh_tokens("rtOld")
            .await
            .expect_err("a redirect is not a token response");

        assert_eq!(err.status(), Some(307));
        assert_eq!(
            elsewhere.received_requests().await.expect("requests").len(),
            0,
            "the credentials must not have been re-POSTed to the redirect target"
        );
    }

    // -----------------------------------------------------------------------
    // TokenManager — translated from `auth/tokenManager.test.ts`
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_still_valid_token_is_returned_without_refreshing() {
        let store = MemStore::new(Some(tokens("good", "r", "2026-01-01T12:00:00Z")));
        let refresh = CountingRefresh::new();
        let manager = TokenManager::new(store.clone(), refresh.clone());

        let token = manager
            .access_token_at(false, at("2026-01-01T11:00:00Z"))
            .await
            .expect("the stored token is live");

        assert_eq!(token, "good");
        assert_eq!(refresh.calls(), 0, "no refresh was needed");
    }

    /// **Review Focus #2.** Fortnox retires the refresh token it was
    /// presented. The assertion that matters is on what is in the *store*
    /// afterwards, not on what the call returned: an implementation that
    /// returns the right access token and forgets to persist the rotation
    /// passes every other test here and dies six weeks later.
    #[tokio::test]
    async fn a_refresh_persists_the_rotated_refresh_token() {
        let store = MemStore::new(Some(tokens("old", "oldRefresh", "2026-01-01T12:00:00Z")));
        let refresh = CountingRefresh::new();
        let manager = TokenManager::new(store.clone(), refresh.clone());

        // 30s before expiry: inside the 60s margin, so this refreshes.
        let now = at("2026-01-01T11:59:30Z");
        let token = manager.access_token_at(false, now).await.expect("refresh");

        assert_eq!(token, "newAccess");
        assert_eq!(refresh.presented(), vec!["oldRefresh".to_string()]);

        let persisted = store.current().expect("something was persisted");
        assert_eq!(
            persisted.refresh_token, "rotatedRefresh",
            "the ROTATED refresh token must be what is on disk; the old one is \
             already dead on Fortnox's side"
        );
        assert_eq!(persisted.access_token, "newAccess");
        assert_eq!(persisted.expires_at, now + chrono::Duration::seconds(3600));
        assert_eq!(persisted.scope, "bookkeeping");
    }

    /// The same property, proven through the real file store: the rotation
    /// has to survive the process, not just the `TokenManager` instance.
    #[tokio::test]
    async fn the_rotated_refresh_token_survives_on_disk_for_the_next_process() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("tokens.json");
        let store = FileTokenStore::new(path.clone());
        store
            .save(&tokens("old", "oldRefresh", "2026-01-01T12:00:00Z"))
            .expect("seed");

        let refresh = CountingRefresh::new();
        let manager = TokenManager::new(Arc::new(store), refresh.clone());
        let now = at("2026-01-01T11:59:30Z");
        manager.access_token_at(false, now).await.expect("refresh");

        // A *fresh* store over the same path: this is what the next daemon
        // start will see.
        let reopened = FileTokenStore::new(path)
            .load()
            .expect("readable")
            .expect("present");
        assert_eq!(reopened.refresh_token, "rotatedRefresh");
        assert_eq!(reopened.access_token, "newAccess");
    }

    #[tokio::test]
    async fn no_stored_tokens_errors_naming_the_authorize_command() {
        let manager = TokenManager::new(MemStore::new(None), CountingRefresh::new());
        let err = manager
            .access_token_at(false, at("2026-01-01T11:00:00Z"))
            .await
            .expect_err("nothing is stored");

        assert!(matches!(err, FortnoxError::Auth(_)), "{err:?}");
        assert!(
            err.to_string().contains(AUTHORIZE_COMMAND),
            "the message must say what to run: {err}"
        );
    }

    #[tokio::test]
    async fn a_failed_refresh_errors_naming_the_authorize_command() {
        let store = MemStore::new(Some(tokens("old", "oldRefresh", "2026-01-01T12:00:00Z")));
        let manager = TokenManager::new(store, CountingRefresh::failing());

        let err = manager
            .access_token_at(false, at("2026-01-01T11:59:30Z"))
            .await
            .expect_err("the refresh failed");

        assert!(matches!(err, FortnoxError::Auth(_)), "{err:?}");
        assert!(err.to_string().contains(AUTHORIZE_COMMAND), "{err}");
    }

    /// Upstream's fourth case. Fortnox has already rotated by the time `save`
    /// runs, so a failed save means the grant is gone — the message has to say
    /// so rather than look like a transient disk hiccup.
    #[tokio::test]
    async fn a_failed_save_after_a_rotation_is_a_loud_specific_error() {
        let store =
            MemStore::failing_save(Some(tokens("old", "oldRefresh", "2026-01-01T12:00:00Z")));
        let refresh = CountingRefresh::new();
        let manager = TokenManager::new(store, refresh.clone());

        let err = manager
            .access_token_at(false, at("2026-01-01T11:59:30Z"))
            .await
            .expect_err("the save failed");

        let rendered = err.to_string();
        assert!(matches!(err, FortnoxError::Auth(_)), "{err:?}");
        assert!(rendered.contains("could NOT be saved"), "{rendered}");
        assert!(rendered.contains(AUTHORIZE_COMMAND), "{rendered}");
        // The refresh really happened: the old token is dead on Fortnox's side.
        assert_eq!(refresh.presented(), vec!["oldRefresh".to_string()]);
    }

    /// An empty rotation must never be persisted.
    ///
    /// Without the guard this test fails in the quietest possible way: the
    /// call succeeds, returns `newAccess`, and leaves a blank refresh token in
    /// the store. Nothing goes wrong for another hour, and when it does the
    /// failure surfaces somewhere unrelated, weeks away from what caused it.
    /// That is how the owner's previous grant lapsed. So: refuse, keep what
    /// was stored, and say it out loud.
    #[tokio::test]
    async fn an_empty_rotated_refresh_token_is_refused_and_never_overwrites_the_stored_one() {
        let store = MemStore::new(Some(tokens("old", "oldRefresh", "2026-01-01T12:00:00Z")));
        let refresh = CountingRefresh::blank_rotation();
        let manager = TokenManager::new(store.clone(), refresh.clone());

        let err = manager
            .access_token_at(false, at("2026-01-01T11:59:30Z"))
            .await
            .expect_err("a blank rotation must not be accepted");

        let rendered = err.to_string();
        assert!(matches!(err, FortnoxError::Auth(_)), "{err:?}");
        assert!(rendered.contains("EMPTY refresh token"), "{rendered}");
        assert!(rendered.contains(AUTHORIZE_COMMAND), "{rendered}");

        // Nothing was written, and the stored grant is untouched.
        assert_eq!(
            store.saves.load(Ordering::SeqCst),
            0,
            "nothing may be saved"
        );
        let stored = store.current().expect("the old tokens are still there");
        assert_eq!(stored.refresh_token, "oldRefresh");
        assert_eq!(stored.access_token, "old");

        // The refresh did happen — the failure is Fortnox's answer, not a
        // short-circuit before the call.
        assert_eq!(refresh.presented(), vec!["oldRefresh".to_string()]);
    }

    // ---- Beyond the upstream file -----------------------------------------

    #[tokio::test]
    async fn three_concurrent_callers_on_an_expired_token_cause_exactly_one_refresh() {
        let store = MemStore::new(Some(tokens("old", "oldRefresh", "2026-01-01T12:00:00Z")));
        let refresh = CountingRefresh::slow();
        let manager = Arc::new(TokenManager::new(store.clone(), refresh.clone()));
        let now = at("2026-01-01T11:59:30Z");

        let calls = (0..3).map(|_| {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.access_token_at(false, now).await })
        });
        for call in futures_join(calls).await {
            assert_eq!(call.expect("no panic").expect("token"), "newAccess");
        }

        assert_eq!(refresh.calls(), 1, "exactly one refresh for three callers");
        assert_eq!(store.saves.load(Ordering::SeqCst), 1, "one write");
    }

    /// The same coalescing for the forced path, which is what a burst of 401s
    /// looks like. Three forced refreshes against a revoked grant is exactly
    /// the hammering that gets an integration rate-limited.
    #[tokio::test]
    async fn three_concurrent_forced_callers_also_cause_exactly_one_refresh() {
        let store = MemStore::new(Some(tokens("old", "oldRefresh", "2026-01-01T12:00:00Z")));
        let refresh = CountingRefresh::slow();
        let manager = Arc::new(TokenManager::new(store, refresh.clone()));
        // Well inside validity: only `force` makes these refresh at all.
        let now = at("2026-01-01T11:00:00Z");

        let calls = (0..3).map(|_| {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move { manager.access_token_at(true, now).await })
        });
        for call in futures_join(calls).await {
            assert_eq!(call.expect("no panic").expect("token"), "newAccess");
        }

        assert_eq!(refresh.calls(), 1, "exactly one forced refresh");
    }

    #[tokio::test]
    async fn force_refreshes_a_token_that_has_not_expired_yet() {
        let store = MemStore::new(Some(tokens("old", "oldRefresh", "2026-01-01T12:00:00Z")));
        let refresh = CountingRefresh::new();
        let manager = TokenManager::new(store, refresh.clone());

        let token = manager
            .access_token_at(true, at("2026-01-01T11:00:00Z"))
            .await
            .expect("forced refresh");

        assert_eq!(token, "newAccess");
        assert_eq!(refresh.calls(), 1);
    }

    /// Join a set of `JoinHandle`s without pulling in `futures`.
    async fn futures_join<T>(
        handles: impl Iterator<Item = tokio::task::JoinHandle<T>>,
    ) -> Vec<Result<T, tokio::task::JoinError>> {
        let handles: Vec<_> = handles.collect();
        let mut out = Vec::with_capacity(handles.len());
        for handle in handles {
            out.push(handle.await);
        }
        out
    }

    // -----------------------------------------------------------------------
    // FileTokenStore
    // -----------------------------------------------------------------------

    #[test]
    fn a_saved_token_file_round_trips() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store = FileTokenStore::new(dir.path().join("nested").join("tokens.json"));
        let saved = tokens("a", "r", "2026-01-01T12:00:00Z");

        store.save(&saved).expect("save");
        let loaded = store.load().expect("load").expect("present");

        assert_eq!(loaded, saved);
    }

    #[test]
    fn a_saved_token_file_is_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("tokens.json");
        FileTokenStore::new(path.clone())
            .save(&tokens("a", "r", "2026-01-01T12:00:00Z"))
            .expect("save");

        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "mode was {mode:04o}");
    }

    #[test]
    fn a_missing_token_file_reads_as_none() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store = FileTokenStore::new(dir.path().join("tokens.json"));
        assert!(store.load().expect("no error for a missing file").is_none());
    }

    /// A corrupt file must not crash-loop the daemon. It reads as `None`, so
    /// the caller reaches the "no stored tokens, run authorize" message.
    #[test]
    fn a_corrupt_token_file_reads_as_none_rather_than_erroring() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("tokens.json");
        std::fs::write(&path, "{ this is not json").expect("write");

        let store = FileTokenStore::new(path);
        assert!(
            store
                .load()
                .expect("a corrupt file is not an error")
                .is_none(),
            "a corrupt file reads as absent"
        );
    }

    /// A save over an existing file leaves the directory with exactly the one
    /// file — no temp file survives a successful rename.
    #[test]
    fn a_save_replaces_the_file_and_leaves_no_temp_behind() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let path = dir.path().join("tokens.json");
        let store = FileTokenStore::new(path.clone());

        store
            .save(&tokens("first", "r1", "2026-01-01T12:00:00Z"))
            .expect("save");
        store
            .save(&tokens("second", "r2", "2026-01-01T13:00:00Z"))
            .expect("save");

        let entries: Vec<String> = std::fs::read_dir(dir.path())
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["tokens.json".to_string()], "{entries:?}");
        assert_eq!(
            store.load().expect("load").expect("present").access_token,
            "second"
        );
    }

    // -----------------------------------------------------------------------
    // Redaction
    // -----------------------------------------------------------------------

    #[test]
    fn debug_never_prints_a_token_or_a_secret() {
        let stored = tokens("ACCESSSECRET", "REFRESHSECRET", "2026-01-01T12:00:00Z");
        let rendered = format!("{stored:?}");
        assert!(!rendered.contains("ACCESSSECRET"), "{rendered}");
        assert!(!rendered.contains("REFRESHSECRET"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");

        let response = FortnoxTokenResponse {
            access_token: "ACCESSSECRET".to_string(),
            refresh_token: "REFRESHSECRET".to_string(),
            expires_in: 3600,
            scope: "bookkeeping".to_string(),
            token_type: "bearer".to_string(),
        };
        let rendered = format!("{response:?}");
        assert!(!rendered.contains("ACCESSSECRET"), "{rendered}");
        assert!(!rendered.contains("REFRESHSECRET"), "{rendered}");

        let client = OAuthClient::with_token_url("cid", "CLIENTSECRET", TOKEN_URL).expect("client");
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("CLIENTSECRET"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }
}
