//! Microsoft Entra ID OAuth 2.0 for KTH mailboxes: the app registration, the
//! per-account token store, and the refresh path.
//!
//! This is `ea_google::auth` with three deliberate differences, each of which
//! is a property of Microsoft's identity platform rather than a preference.
//! Everything else — one file per account at mode `0600`, an atomic write, a
//! path-traversal-rejecting label check, a per-account mutex, a remembered
//! `invalid_grant`, hand-written redacting `Debug` impls, `.without_url()` on
//! every `reqwest` error, and never quoting a 2xx body — is the same, on
//! purpose: the Google connector's shapes were reviewed, and reinventing them
//! here would only produce a second set of bugs.
//!
//! # Difference 1: a public client, so there is no client secret
//!
//! The Google connector registers a *confidential* desktop client and keeps a
//! `clientSecret` in `app.json`. This one registers an Entra **public client**
//! and uses **PKCE** instead. That is not a stylistic choice. The owner is a
//! student: they cannot register an application inside KTH's own tenant, so
//! the registration lives in a tenant of their own and is marked
//! multitenant. A secret issued there would be a secret sitting on a laptop
//! protecting nothing that the PKCE `code_verifier` does not already protect
//! for a loopback redirect, and Entra treats public clients as the correct
//! shape for a desktop app. So [`AppConfig`] holds a client id and a tenant,
//! and neither is a credential — an Entra application (client) id is a public
//! identifier by design. [`AppConfig`] therefore uses
//! `#[serde(deny_unknown_fields)]`: a `clientSecret` typed into that file
//! would otherwise be silently ignored, and the owner would believe it was
//! being used.
//!
//! # Difference 2: Microsoft **rotates** the refresh token
//!
//! Google issues one refresh token at consent and returns none on a refresh,
//! so `refresh_token.unwrap_or(existing)` is the right rule there. Microsoft
//! returns a **new** refresh token on every successful refresh and expects
//! the client to replace the stored one. Keeping the old one would work until
//! Microsoft decided to invalidate it, and then the account would be dead
//! with no local sign of why.
//!
//! So [`Auth::refresh_locked`] persists whatever came back, and
//! `a_refresh_persists_the_rotated_refresh_token` reads it back **off disk**
//! rather than off the in-memory value — the write is the part that matters.
//! The one refusal: a response whose `refresh_token` is present but *blank*
//! is rejected rather than saved, because saving it would erase the grant.
//! (Phase 3's Fortnox connector made exactly that mistake and it was caught in
//! review; the rule is copied here rather than re-learned.)
//!
//! # Difference 3: the tenant is pinned
//!
//! The authority is `https://login.microsoftonline.com/<tenant>/oauth2/v2.0/`
//! with `<tenant>` defaulting to [`KTH_TENANT_ID`], the GUID KTH's OpenID
//! configuration document resolves `kth.se` to. The obvious alternative,
//! `/common`, would let *any* Microsoft account — a personal outlook.com
//! address, a different university — complete the consent and be written to
//! disk under the label `kth`, and nothing downstream could tell. A pinned
//! tenant makes that a login error instead of a silently wrong mailbox.
//!
//! # Token safety
//!
//! A Microsoft refresh token for `Mail.Read` reads the owner's entire KTH
//! mailbox. The rules are `ea_google::auth`'s, unchanged:
//!
//! * [`Tokens`], [`TokenResponse`] and [`HttpTokenBackend`] have hand-written
//!   `Debug` impls printing `<redacted>`, pinned by a test that formats each
//!   with `{:?}` and asserts the secret is absent.
//! * Secrets travel in a POST body, never a URL, and every `reqwest` error
//!   goes through `.without_url()`.
//! * No error quotes a body that could have been a *success* body. Error
//!   bodies are quoted, truncated: they carry `AADSTS…` codes and nothing
//!   else.
//! * The token store's errors quote the *path* of a corrupt file, never its
//!   contents.
//! * The PKCE `code_verifier` and the authorization code are one-time bearers
//!   of the whole grant; `bin/authorize.rs` never prints either.
//!
//! # Two ways a token dies
//!
//! An access token expires on a clock (Entra: roughly an hour), and
//! [`Auth::access_token`] refreshes it a minute early. It can also simply
//! *stop working* — a password change, a revoked session, a Conditional
//! Access policy that now demands a compliant device — and that looks like a
//! 401 on a token that has not expired. [`Auth::refresh_after_unauthorized`]
//! is the recovery: one forced refresh, which the caller retries its request
//! against exactly once.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::{DateTime, Utc};
use reqwest::Url;
use serde::{Deserialize, Serialize};

/// The connector's name. Equal to its directory's basename
/// (`connectors/kth`) and to its `policy.toml` section, both of which the
/// daemon enforces at startup.
pub const CONNECTOR: &str = "kth";

/// The file, inside the connector's config directory, holding the Entra
/// application registration. Excluded from [`TokenStore::list`], which would
/// otherwise report it as an account named `app`.
pub const APP_CONFIG_FILE: &str = "app.json";

/// KTH's Microsoft Entra tenant id.
///
/// Measured, not assumed:
/// `https://login.microsoftonline.com/kth.se/.well-known/openid-configuration`
/// resolves to this GUID. Pinned rather than using `/common` — see the module
/// docs on why a login by the wrong identity must be an error and not a
/// silently wrong mailbox.
pub const KTH_TENANT_ID: &str = "3db27ecc-1791-4dda-9b51-798adfa4a3ca";

/// The scopes every account is authorised for, and the whole of what this
/// connector can do.
///
/// `Mail.Read` is **read-only**: delegated, it grants reading mail in the
/// signed-in user's mailbox and nothing else. `offline_access` is what makes
/// Entra willing to issue a refresh token at all.
///
/// Deliberately absent, and each for a reason that outlives this file:
///
/// * `Mail.ReadWrite` — this connector implements no draft tool, so asking
///   for the ability to write into the owner's mailbox would buy a capability
///   nothing uses. If a draft tool is ever added, this is the scope to add,
///   and `connectors/kth/policy.toml` already denies `create_draft` by name
///   so the gate is shut before the door exists.
/// * `Mail.Send` — never. The Google connector's rule holds here: nothing in
///   this project sends mail as the owner, and the scope list is the second
///   of two locks (the first being that no such tool exists, the third being
///   the policy's `send_mail = "deny"`).
///
/// The Graph resource is spelled out in full. Entra's v2.0 endpoint refuses a
/// request mixing scopes from two resources, and `offline_access` is one of
/// the reserved scopes that may be combined with any of them.
pub const SCOPES: [&str; 2] = ["offline_access", "https://graph.microsoft.com/Mail.Read"];

/// Where the consent redirect lands. Loopback, because a desktop client has
/// nowhere else to put it. The port differs from the Google connector's 8471
/// so both `authorize` commands can be mid-flight at once without one failing
/// to bind.
pub const DEFAULT_REDIRECT_URI: &str = "http://127.0.0.1:8473/callback";

/// How close to expiry counts as expired. Entra's access tokens last about an
/// hour; a minute of margin covers the round trip plus a slow network without
/// refreshing more than once an hour in the steady state.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(60);

/// Per-request deadline on a token-endpoint call.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How much of an *error* response body to quote back. Never applied to a 2xx
/// body — see the module's `# Token safety`.
const BODY_SNIPPET: usize = 300;

/// The authority for a tenant: everything before `authorize` / `token`.
pub fn authority(tenant: &str) -> String {
    format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0")
}

// ---------------------------------------------------------------------------
// Account names
// ---------------------------------------------------------------------------

/// Reject anything that is not `^[a-z0-9][a-z0-9_-]*$`, **before** a path is
/// constructed from it.
///
/// Verbatim in effect from `ea_google::auth::validate_account`, for the same
/// two reasons. Account labels reach this crate from tool arguments a
/// language model produces, and `Path::join` with an absolute path replaces
/// the base entirely while `..` walks out of the config directory — so
/// `TokenStore::read("../../.ssh/id_rsa")` would otherwise be a file-read
/// primitive. And the labels become filenames on APFS, which is
/// case-*insensitive* by default, so `kth` and `KTH` would be two labels
/// sharing one file: authorising the second would silently overwrite the
/// first's grant.
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
            "{account:?} is not a usable KTH account label. A label must match \
             ^[a-z0-9][a-z0-9_-]*$ (for example \"kth\"); it becomes a filename, so \
             \"/\" and \"..\" are refused.{hint}"
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
    /// The long-lived grant. Microsoft **rotates** this: every successful
    /// refresh returns a replacement, and the replacement is what gets
    /// written back.
    pub refresh_token: String,
    /// When [`Tokens::access_token`] stops working.
    pub expiry: DateTime<Utc>,
    /// The space-separated scope list Entra actually granted, which can be
    /// narrower than [`SCOPES`].
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

/// One JSON file per account under `~/.config/exec-agent/kth/`, mode `0600`.
///
/// One file per account rather than one file with a map, so a crash, a
/// concurrent write or a corrupt file costs one account rather than all of
/// them — and so `write` can be a rename, which is atomic on any POSIX
/// filesystem.
#[derive(Debug, Clone)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// `dir` of `None` means `~/.config/exec-agent/kth/` (or
    /// `$EA_CONFIG_DIR/kth/`), created `0700` if absent.
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
    pub fn read(&self, account: &str) -> anyhow::Result<Tokens> {
        use std::os::unix::fs::PermissionsExt;

        let path = self.path_for(account)?;

        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no KTH tokens for account {account:?} at {}. Authorise it with:\n  {}",
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
                "{} is mode {mode:04o}; it holds a Microsoft refresh token for the \
                 owner's KTH mailbox and must be 0600 (run: chmod 600 {})",
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
                "{} is not a readable KTH token file ({err}). Delete it and \
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

        let body = serde_json::to_vec_pretty(tokens).context("serialising KTH tokens")?;

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
/// `ea-kth-authorize` rather than `authorize`.
pub fn authorize_command(account: &str) -> String {
    format!("ea-kth-authorize {account}")
}

// ---------------------------------------------------------------------------
// The application registration
// ---------------------------------------------------------------------------

/// `~/.config/exec-agent/kth/app.json` — the Entra application, shared by
/// every account.
///
/// Note what is **not** in here: a client secret. This is a public client
/// using PKCE (see the module docs), so nothing in this struct is a
/// credential. `deny_unknown_fields` is therefore load-bearing rather than
/// tidy: an owner who pasted a `clientSecret` into this file would otherwise
/// get a connector that silently ignored it and a belief that a secret was
/// protecting something.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppConfig {
    /// The Entra application (client) id — a GUID. Public by design.
    pub client_id: String,
    /// The tenant the consent is performed against. Defaults to
    /// [`KTH_TENANT_ID`]; override only to point the tests or a second
    /// university at a different authority.
    #[serde(default = "default_tenant")]
    pub tenant: String,
    #[serde(default = "default_redirect_uri")]
    pub redirect_uri: String,
    /// Overridden only by the tests, which point it at a `wiremock` server.
    /// Absent from a real `app.json`, where it is derived from `tenant`.
    #[serde(default)]
    pub authority: Option<String>,
}

fn default_tenant() -> String {
    KTH_TENANT_ID.to_string()
}
fn default_redirect_uri() -> String {
    DEFAULT_REDIRECT_URI.to_string()
}

impl fmt::Debug for AppConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Everything here is a public identifier; there is nothing to redact.
        // The impl is hand-written anyway so that adding a secret field later
        // forces somebody past this comment.
        f.debug_struct("AppConfig")
            .field("client_id", &self.client_id)
            .field("tenant", &self.tenant)
            .field("redirect_uri", &self.redirect_uri)
            .field("authority", &self.authority)
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
         printf '{{\"clientId\":\"<application-client-id>\"}}' > {path}\n  \
         chmod 600 {path}\n\
         The client id comes from an Entra app registration you own: \
         https://entra.microsoft.com -> App registrations -> New registration, \
         \"Accounts in any organizational directory\" (multitenant), platform \
         \"Mobile and desktop applications\", redirect URI {DEFAULT_REDIRECT_URI}. \
         It is a public client: there is no client secret, and this connector \
         would not use one. See connectors/kth/README.md.",
        path = path.display()
    )
}

impl AppConfig {
    /// Load from `~/.config/exec-agent/kth/app.json` (or
    /// `$EA_CONFIG_DIR/kth/app.json`).
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&ea_core::paths::connector_config_dir(CONNECTOR))
    }

    /// Load from an explicit directory. Used by the tests.
    ///
    /// Unlike the Google connector, the file's mode is **not** checked. That
    /// is deliberate and is the one place this crate knowingly diverges: there
    /// is no secret in it, so refusing to start over a mode would be
    /// superstition rather than defence. The token files, which do hold
    /// secrets, are checked as strictly as Google's.
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        let path = dir.join(APP_CONFIG_FILE);

        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no KTH app registration at {}.\n{}",
                    path.display(),
                    how_to_create_app_config(&path)
                );
            }
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };

        let config: Self = serde_json::from_str(&text).map_err(|err| {
            anyhow::anyhow!(
                "{} is not the expected JSON ({err}). It must be an object with \
                 \"clientId\", and optionally \"tenant\" and \"redirectUri\". A \
                 \"clientSecret\" is refused rather than ignored: this is a public \
                 client using PKCE and has no use for one.\n{}",
                path.display(),
                how_to_create_app_config(&path)
            )
        })?;

        if config.client_id.trim().is_empty() {
            bail!("{} has an empty \"clientId\"", path.display());
        }
        if config.tenant.trim().is_empty() {
            bail!("{} has an empty \"tenant\"", path.display());
        }
        Url::parse(&config.redirect_uri)
            .with_context(|| format!("{} has an unparseable \"redirectUri\"", path.display()))?;
        Url::parse(&config.authority_base())
            .with_context(|| format!("{} produces an unparseable authority", path.display()))?;

        Ok(config)
    }

    /// The authority base — `.../oauth2/v2.0`, with no trailing slash.
    pub fn authority_base(&self) -> String {
        match &self.authority {
            Some(explicit) => explicit.trim_end_matches('/').to_string(),
            None => authority(&self.tenant),
        }
    }

    pub fn auth_uri(&self) -> String {
        format!("{}/authorize", self.authority_base())
    }

    pub fn token_uri(&self) -> String {
        format!("{}/token", self.authority_base())
    }

    /// The consent URL for one account.
    ///
    /// `state` is echoed back on the redirect and checked, so a stray request
    /// to the loopback listener cannot inject a code. `code_challenge` is the
    /// S256 hash of the verifier the same process will present at the token
    /// endpoint — the substitute for the client secret a confidential client
    /// would use.
    ///
    /// `prompt=select_account` rather than `consent`: Google needs
    /// `prompt=consent` because it refuses to re-issue a refresh token
    /// otherwise, while Entra issues one whenever `offline_access` is granted.
    /// What actually goes wrong here is signing in as the wrong identity —
    /// a browser already logged into a personal Microsoft account will
    /// otherwise sail straight past the account picker — so the prompt that
    /// earns its place is the one that makes the owner choose.
    pub fn authorization_url(&self, state: &str, code_challenge: &str) -> anyhow::Result<Url> {
        Url::parse_with_params(
            &self.auth_uri(),
            &[
                ("client_id", self.client_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", self.redirect_uri.as_str()),
                ("response_mode", "query"),
                ("scope", SCOPES.join(" ").as_str()),
                ("state", state),
                ("code_challenge", code_challenge),
                ("code_challenge_method", "S256"),
                ("prompt", "select_account"),
            ],
        )
        .with_context(|| format!("building an authorization URL from {:?}", self.auth_uri()))
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// A fresh PKCE verifier: 64 hex characters, from two v4 UUIDs.
///
/// RFC 7636 wants 43–128 characters from the unreserved set and at least 256
/// bits of entropy; two UUIDs give 256 bits of the operating system's
/// randomness (v4 UUIDs are generated from `getrandom`) in 64 characters, all
/// of them unreserved. Written this way rather than pulling in `rand` for one
/// call, since `uuid` is already a dependency of every connector here.
pub fn new_code_verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The S256 challenge for a verifier: `BASE64URL(SHA256(ASCII(verifier)))`,
/// unpadded, exactly as RFC 7636 §4.2 specifies.
pub fn code_challenge_s256(verifier: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
}

// ---------------------------------------------------------------------------
// The token backend
// ---------------------------------------------------------------------------

/// What Entra's token endpoint answers a refresh (or a code exchange) with.
///
/// No `Debug` derive: `access_token` and `refresh_token` are the secrets this
/// whole module exists to keep out of logs.
#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    /// Present on a code exchange **and** on every refresh: Microsoft rotates
    /// the refresh token. `Option` rather than `String` because a response
    /// without one is a real (if unexpected) wire shape that must be handled
    /// rather than fail to deserialise — see [`Auth::refresh_locked`].
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Seconds until [`TokenResponse::access_token`] expires.
    pub expires_in: i64,
    #[serde(default)]
    pub scope: Option<String>,
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
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
    /// Entra answered `{"error":"invalid_grant", ...}`: revoked, expired,
    /// password changed, or blocked by a Conditional Access policy.
    #[error("Microsoft rejected the refresh token (invalid_grant)")]
    InvalidGrant,
    /// Anything else: a network failure, a 5xx, an unparseable response.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// The refresh grant, behind a trait so [`Auth`] can be tested without HTTP.
///
/// Hand-rolled boxed future rather than `async fn` in trait, because `Auth`
/// holds an `Arc<dyn RefreshBackend>` and async-fn-in-trait is not
/// `dyn`-compatible.
pub trait RefreshBackend: Send + Sync {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, RefreshError>> + Send + 'a>>;
}

/// The real thing: a form POST to Entra's token endpoint.
///
/// No `Debug` derive — a future secret field must not slip into a log line
/// just because nobody looked.
#[derive(Clone)]
pub struct HttpTokenBackend {
    client: reqwest::Client,
    token_uri: String,
    client_id: String,
}

impl fmt::Debug for HttpTokenBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpTokenBackend")
            .field("token_uri", &self.token_uri)
            .field("client_id", &self.client_id)
            .finish()
    }
}

impl HttpTokenBackend {
    pub fn new(config: &AppConfig) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            // Not optional; see `ea_core::http` for the 403 that proved it.
            .user_agent(ea_core::http::USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            // This client carries a refresh token (and, at consent time, a
            // PKCE verifier) in every request body. `reqwest`'s default
            // redirect policy strips the `Authorization` header across origins
            // but knows nothing about a body, so a redirect would re-POST both
            // to wherever the server pointed. The token endpoint has no reason
            // to redirect.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building the HTTP client for Microsoft's token endpoint")?;
        Ok(Self {
            client,
            token_uri: config.token_uri(),
            client_id: config.client_id.clone(),
        })
    }

    /// Exchange an authorization code for tokens, proving possession of the
    /// PKCE verifier. Used only by `bin/authorize.rs`.
    pub async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        redirect_uri: &str,
        scopes: &str,
    ) -> anyhow::Result<TokenResponse> {
        let form = [
            ("client_id", self.client_id.as_str()),
            ("scope", scopes),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
            ("code_verifier", code_verifier),
        ];
        self.post_form(&form).await.map_err(|err| match err {
            RefreshError::InvalidGrant => anyhow::anyhow!(
                "Microsoft rejected the authorization code (invalid_grant). Codes are \
                 single-use and expire within minutes; run the command again."
            ),
            RefreshError::Other(err) => err,
        })
    }

    async fn post_form(&self, form: &[(&str, &str)]) -> Result<TokenResponse, RefreshError> {
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
                        .context("POST to Microsoft's token endpoint failed"),
                )
            })?;

        let status = response.status();

        if !status.is_success() {
            // An error body is safe to quote: by definition it did not carry a
            // token. It is truncated anyway, and it is where the AADSTS code
            // that explains a refusal lives.
            let body = response.text().await.unwrap_or_default();
            if is_invalid_grant(&body) {
                return Err(RefreshError::InvalidGrant);
            }
            let snippet: String = body.chars().take(BODY_SNIPPET).collect();
            return Err(RefreshError::Other(anyhow::anyhow!(
                "Microsoft's token endpoint answered {status}: {snippet}"
            )));
        }

        let body = response.text().await.map_err(|err| {
            RefreshError::Other(
                anyhow::Error::new(err.without_url())
                    .context("reading Microsoft's token endpoint response"),
            )
        })?;

        // The body is NOT quoted here. This is the success path, so the body is
        // exactly the document that contains an access token and a refresh
        // token — the one thing that must never reach a log.
        serde_json::from_str::<TokenResponse>(&body).map_err(|err| {
            RefreshError::Other(anyhow::anyhow!(
                "Microsoft's token endpoint answered {status} with {} bytes that are not \
                 the expected JSON ({err}). The body is deliberately not quoted: a \
                 success response carries tokens.",
                body.len()
            ))
        })
    }
}

/// Entra reports a dead grant as a 400 with `{"error":"invalid_grant", ...}`.
/// Matched on the parsed field rather than a substring of the whole body, so
/// an `error_description` that merely mentions the phrase cannot be mistaken
/// for it.
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

impl RefreshBackend for HttpTokenBackend {
    fn refresh<'a>(
        &'a self,
        refresh_token: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, RefreshError>> + Send + 'a>> {
        Box::pin(async move {
            let scopes = SCOPES.join(" ");
            let form = [
                ("client_id", self.client_id.as_str()),
                ("scope", scopes.as_str()),
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
/// No `Debug` impl at all: it reaches a token store, and there is no reason
/// for it to appear in a log line.
pub struct Auth {
    store: TokenStore,
    /// One mutex per account, created on first use. The outer mutex is held
    /// only long enough to look one up or insert it — never across an `await`
    /// on the network.
    locks: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    backend: Arc<dyn RefreshBackend>,
    /// Accounts whose grant Microsoft has said is *gone*, keyed by account.
    ///
    /// This is what keeps [`Auth::refresh_after_unauthorized`] from becoming a
    /// retry loop. A revoked grant answers `invalid_grant` to every refresh,
    /// and every request in a poll would otherwise see a 401, force a refresh,
    /// and be refused again — POSTs to Microsoft's token endpoint on every
    /// poll for an account that will never come back, which is how an
    /// integration gets throttled.
    ///
    /// Only `invalid_grant` is remembered: it is the one answer that means
    /// *this will not work again until a human re-authorises*. A connection
    /// reset or a 500 is retried normally.
    ///
    /// The entry is keyed to the access token that was in the store when the
    /// refresh failed, so re-authorising (which writes a new one) clears the
    /// memo implicitly. It is stored as a hash, not the token.
    dead: std::sync::Mutex<HashMap<String, DeadGrant>>,
}

/// One remembered `invalid_grant`. See [`Auth::dead`].
struct DeadGrant {
    fingerprint: u64,
    /// The exact error text to replay, so a dead account produces the *same*
    /// message every poll rather than a slightly different one each time —
    /// the daemon keys an event's identity on its payload, and a message that
    /// churns would re-notify the owner every poll.
    message: String,
}

/// A non-cryptographic fingerprint of an access token, used only to tell "the
/// same token as last time" from "a different one". A hash rather than the
/// token itself because [`Auth`] keeps this for as long as the process lives.
fn token_fingerprint(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

impl Auth {
    /// The production constructor: refreshes over HTTP against the authority
    /// in `config`.
    pub fn new(config: AppConfig, store: TokenStore) -> anyhow::Result<Self> {
        let backend = Arc::new(HttpTokenBackend::new(&config)?);
        Ok(Self::with_backend(store, backend))
    }

    /// Inject a backend. Used by the tests.
    pub fn with_backend(store: TokenStore, backend: Arc<dyn RefreshBackend>) -> Self {
        Self {
            store,
            locks: tokio::sync::Mutex::new(HashMap::new()),
            backend,
            dead: std::sync::Mutex::new(HashMap::new()),
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
        // callers cost one refresh: the second arrives after the first has
        // already written the new token to disk, sees a healthy expiry, and
        // returns without touching the network.
        let tokens = self.store.read(account)?;
        if !tokens.is_stale_at(now) {
            return Ok(tokens.access_token);
        }

        self.refresh_locked(account, tokens, now).await
    }

    /// Refresh `account` *now*, because Microsoft refused `rejected` with a
    /// 401, and return the replacement.
    ///
    /// [`Auth::access_token`] alone cannot recover from this: it refreshes on
    /// **local** expiry, and a token can be dead long before it expires.
    ///
    /// Exactly one refresh, and at most one: callers retry the request once
    /// with what comes back and then give up, and this function never loops.
    ///
    /// `rejected` is the token that got the 401. If the store no longer holds
    /// it, some other in-flight request has already refreshed, and the fresh
    /// token is returned without touching the network.
    pub async fn refresh_after_unauthorized(
        &self,
        account: &str,
        rejected: &str,
    ) -> anyhow::Result<String> {
        self.refresh_after_unauthorized_at(account, rejected, Utc::now())
            .await
    }

    /// [`Auth::refresh_after_unauthorized`] against an explicit clock.
    pub async fn refresh_after_unauthorized_at(
        &self,
        account: &str,
        rejected: &str,
        now: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        validate_account(account)?;

        let lock = self.lock_for(account).await;
        let _guard = lock.lock().await;

        let tokens = self.store.read(account)?;
        if tokens.access_token != rejected {
            // Somebody already replaced the token Microsoft refused. Handing
            // back the new one costs nothing and is what the caller wanted.
            return Ok(tokens.access_token);
        }

        self.refresh_locked(account, tokens, now).await
    }

    /// The refresh itself. The per-account lock is held by the caller, and
    /// `tokens` was read from the store inside it.
    async fn refresh_locked(
        &self,
        account: &str,
        tokens: Tokens,
        now: DateTime<Utc>,
    ) -> anyhow::Result<String> {
        if let Some(message) = self.remembered_invalid_grant(account, &tokens.access_token) {
            bail!("{message}");
        }

        let response = match self.backend.refresh(&tokens.refresh_token).await {
            Ok(response) => response,
            Err(RefreshError::InvalidGrant) => {
                let message = format!(
                    "Microsoft rejected the stored refresh token for KTH account \
                     {account:?} (invalid_grant): it has been revoked or expired, the \
                     account's password changed, or a Conditional Access policy now \
                     refuses this application. Re-authorise with:\n  {}",
                    authorize_command(account)
                );
                self.remember_invalid_grant(account, &tokens.access_token, &message);
                bail!("{message}");
            }
            // Deliberately unchanged, with no added context. A connection
            // reset is not a revoked grant, and dressing it up as one sends
            // the reader to a browser to fix their network.
            Err(RefreshError::Other(err)) => return Err(err),
        };

        // Microsoft rotates: the response carries a *replacement* refresh
        // token and the old one should be considered spent. Three cases, and
        // the middle one is the one that has bitten this project before.
        let rotated = match response.refresh_token.as_deref() {
            // Present and blank. Saving it would erase the grant and the next
            // poll would report a mailbox that cannot be read, with nothing
            // saying why. Refusing leaves the previous token in place, which
            // is at worst already dead — and then the error is honest.
            Some(blank) if blank.trim().is_empty() => bail!(
                "Microsoft's token endpoint returned an empty refresh token for KTH \
                 account {account:?}. Nothing was written: storing it would erase the \
                 grant and the next poll would fail with no explanation. If this \
                 persists, re-authorise with:\n  {}",
                authorize_command(account)
            ),
            Some(fresh) => fresh.to_string(),
            // Absent. Not the documented Entra behaviour, but keeping the
            // existing token is strictly better than clearing it: the worst
            // case is that the next refresh fails with invalid_grant, which
            // is already handled and says what to do.
            None => tokens.refresh_token.clone(),
        };

        let refreshed = Tokens {
            access_token: response.access_token,
            refresh_token: rotated,
            expiry: now + chrono::Duration::seconds(response.expires_in),
            scope: response.scope.unwrap_or(tokens.scope),
        };

        // Persist before returning. A rotated refresh token that lives only in
        // memory is the worst of both worlds: Microsoft has retired the one on
        // disk, so the next daemon restart finds a grant that no longer works.
        self.store.write(account, &refreshed)?;
        self.forget_invalid_grant(account);

        Ok(refreshed.access_token)
    }

    /// The remembered `invalid_grant` message for `account`, if the store
    /// still holds the same access token it held when the refresh was refused.
    fn remembered_invalid_grant(&self, account: &str, current: &str) -> Option<String> {
        let dead = match self.dead.lock() {
            Ok(dead) => dead,
            Err(poisoned) => poisoned.into_inner(),
        };
        dead.get(account)
            .filter(|entry| entry.fingerprint == token_fingerprint(current))
            .map(|entry| entry.message.clone())
    }

    fn remember_invalid_grant(&self, account: &str, current: &str, message: &str) {
        let mut dead = match self.dead.lock() {
            Ok(dead) => dead,
            Err(poisoned) => poisoned.into_inner(),
        };
        dead.insert(
            account.to_string(),
            DeadGrant {
                fingerprint: token_fingerprint(current),
                message: message.to_string(),
            },
        );
    }

    fn forget_invalid_grant(&self, account: &str) {
        let mut dead = match self.dead.lock() {
            Ok(dead) => dead,
            Err(poisoned) => poisoned.into_inner(),
        };
        dead.remove(account);
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
// Every test here is offline. The HTTP ones point a real `HttpTokenBackend` at
// a `wiremock` server on loopback. Nothing in this file can reach
// login.microsoftonline.com.

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use wiremock::matchers::{body_string_contains, method, path as path_matcher};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Distinctive, so `assert!(!text.contains(..))` means something.
    const REFRESH_TOKEN: &str = "0.AXoA-REFRESH-SECRET-do-not-log-me";
    const ACCESS_TOKEN: &str = "eyJ0eXAi-ACCESS-SECRET-do-not-log-me";
    const ROTATED_REFRESH: &str = "0.AXoA-ROTATED-SECRET-do-not-log-me";

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

    fn app_config_for(authority: &str) -> AppConfig {
        AppConfig {
            client_id: "11111111-2222-3333-4444-555555555555".to_string(),
            tenant: KTH_TENANT_ID.to_string(),
            redirect_uri: DEFAULT_REDIRECT_URI.to_string(),
            authority: Some(authority.to_string()),
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

        store.write("kth", &written).unwrap();
        let read = store.read("kth").unwrap();

        assert_eq!(read.access_token, written.access_token);
        assert_eq!(read.refresh_token, written.refresh_token);
        assert_eq!(read.scope, written.scope);
        assert_eq!(read.expiry, expiry);
    }

    #[test]
    fn a_written_token_file_is_mode_0600() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        store.write("kth", &tokens_expiring_at(Utc::now())).unwrap();

        let path = store.path_for("kth").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode,
            0o600,
            "{} holds a Microsoft refresh token and must be owner-read-write only",
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

        store.write("kth", &tokens_expiring_at(Utc::now())).unwrap();
        store
            .write(
                "kth",
                &tokens_expiring_at(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();

        let path = store.path_for("kth").unwrap();
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

        let mut student = tokens_expiring_at(Utc::now());
        student.access_token = "student-access".into();
        student.refresh_token = "student-refresh".into();
        let mut staff = tokens_expiring_at(Utc::now());
        staff.access_token = "staff-access".into();
        staff.refresh_token = "staff-refresh".into();

        store.write("kth", &student).unwrap();
        store.write("kth-staff", &staff).unwrap();

        assert_eq!(store.read("kth").unwrap().access_token, "student-access");
        assert_eq!(
            store.read("kth-staff").unwrap().access_token,
            "staff-access"
        );
        assert_ne!(
            store.path_for("kth").unwrap(),
            store.path_for("kth-staff").unwrap()
        );
    }

    #[test]
    fn list_returns_the_account_labels() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());

        store.write("kth", &tokens_expiring_at(Utc::now())).unwrap();
        store
            .write("kth-staff", &tokens_expiring_at(Utc::now()))
            .unwrap();
        // Neither of these is an account.
        std::fs::write(tmp.path().join(APP_CONFIG_FILE), "{}").unwrap();
        std::fs::write(tmp.path().join("notes.txt"), "hello").unwrap();

        assert_eq!(
            store.list().unwrap(),
            vec!["kth".to_string(), "kth-staff".to_string()]
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

        let err = store.read("kth").unwrap_err().to_string();

        assert!(err.contains("kth"), "{err}");
        assert!(
            err.contains("ea-kth-authorize kth"),
            "the message must be a command the reader can paste: {err}"
        );
        assert!(err.contains(tmp.path().to_str().unwrap()), "{err}");
    }

    #[test]
    fn a_token_file_a_group_can_read_is_refused() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store.write("kth", &tokens_expiring_at(Utc::now())).unwrap();

        let path = store.path_for("kth").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let err = store.read("kth").unwrap_err().to_string();
        assert!(err.contains("0640"), "{err}");
        assert!(err.contains("chmod 600"), "{err}");
    }

    /// The path-traversal case. Not theoretical: account labels arrive from
    /// tool arguments a model writes.
    #[test]
    fn an_account_name_containing_a_slash_or_dot_dot_is_rejected_before_any_path_is_built() {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = tmp.path().join("kth");
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
            "kth/../../outside/secret",
            "/etc/passwd",
            "sub/kth",
            ".hidden",
            "-leading-dash",
            "",
            "kth name",
            "kth\0",
            "kth\n",
            "KTH",
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
    }

    // -----------------------------------------------------------------------
    // The app registration
    // -----------------------------------------------------------------------

    #[test]
    fn an_app_config_defaults_to_kths_tenant_and_the_loopback_redirect() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(APP_CONFIG_FILE),
            r#"{"clientId":"11111111-2222-3333-4444-555555555555"}"#,
        )
        .unwrap();

        let config = AppConfig::load_from(tmp.path()).unwrap();
        assert_eq!(config.tenant, KTH_TENANT_ID);
        assert_eq!(config.redirect_uri, DEFAULT_REDIRECT_URI);
        assert_eq!(
            config.token_uri(),
            format!("https://login.microsoftonline.com/{KTH_TENANT_ID}/oauth2/v2.0/token")
        );
    }

    /// The tenant is pinned so a personal Microsoft account cannot complete
    /// the consent and be written to disk as the owner's KTH mailbox.
    #[test]
    fn the_default_authority_is_the_kth_tenant_and_not_common() {
        assert!(
            !authority(KTH_TENANT_ID).contains("/common"),
            "a /common authority would accept any Microsoft account"
        );
        assert!(authority(KTH_TENANT_ID).contains(KTH_TENANT_ID));
    }

    /// A pasted `clientSecret` must be an error, not a silently ignored field:
    /// this is a public client and a secret there protects nothing.
    #[test]
    fn an_app_config_carrying_a_client_secret_is_refused_rather_than_ignored() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join(APP_CONFIG_FILE),
            r#"{"clientId":"abc","clientSecret":"shhh"}"#,
        )
        .unwrap();

        let err = AppConfig::load_from(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("public client"), "{err}");
        assert!(
            !err.contains("shhh"),
            "the refusal must not echo the value back: {err}"
        );
    }

    #[test]
    fn a_missing_app_config_says_how_to_make_one() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = AppConfig::load_from(tmp.path()).unwrap_err().to_string();
        assert!(err.contains("app.json"), "{err}");
        assert!(err.contains("App registrations"), "{err}");
        assert!(err.contains(DEFAULT_REDIRECT_URI), "{err}");
    }

    // -----------------------------------------------------------------------
    // Scopes
    // -----------------------------------------------------------------------

    /// The minimum, pinned. Widening this list is a deliberate act with a
    /// README paragraph behind it, not something that happens while adding a
    /// feature.
    #[test]
    fn the_scopes_are_offline_access_and_read_only_mail() {
        assert_eq!(
            SCOPES,
            ["offline_access", "https://graph.microsoft.com/Mail.Read"]
        );
        for scope in SCOPES {
            assert!(
                !scope.contains("Mail.Send"),
                "nothing in this project may send mail as the owner"
            );
            assert!(
                !scope.contains("ReadWrite"),
                "no tool here writes to the mailbox, so no write scope is requested"
            );
        }
    }

    // -----------------------------------------------------------------------
    // PKCE
    // -----------------------------------------------------------------------

    /// RFC 7636 §4.1: 43–128 characters from the unreserved set, and enough
    /// entropy that guessing it is not a way past the missing client secret.
    #[test]
    fn a_code_verifier_is_rfc_7636_shaped_and_fresh_every_time() {
        let a = new_code_verifier();
        let b = new_code_verifier();
        assert_ne!(a, b, "a reused verifier would defeat PKCE entirely");
        for verifier in [&a, &b] {
            assert!(
                (43..=128).contains(&verifier.len()),
                "{verifier} is {} characters",
                verifier.len()
            );
            assert!(
                verifier
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-._~".contains(c)),
                "{verifier} contains a character outside RFC 7636's unreserved set"
            );
        }
    }

    /// The one vector in RFC 7636 appendix B, so the implementation is checked
    /// against the specification rather than against itself.
    #[test]
    fn the_s256_challenge_matches_rfc_7636_appendix_b() {
        assert_eq!(
            code_challenge_s256("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn the_authorization_url_carries_the_challenge_and_never_the_verifier() {
        let config = app_config_for("https://login.example.test/tenant/oauth2/v2.0");
        let verifier = new_code_verifier();
        let challenge = code_challenge_s256(&verifier);
        let url = config
            .authorization_url("state-123", &challenge)
            .unwrap()
            .to_string();

        assert!(url.contains("code_challenge_method=S256"), "{url}");
        // Base64url is entirely unreserved, so the challenge appears in the
        // query string exactly as computed, with nothing percent-encoded.
        assert!(url.contains(&challenge), "{url}");
        assert!(
            !url.contains(&verifier),
            "the verifier must never leave this process until the token POST"
        );
        assert!(url.contains("offline_access"), "{url}");
        assert!(url.contains("Mail.Read"), "{url}");
        assert!(!url.contains("Mail.Send"), "{url}");
    }

    // -----------------------------------------------------------------------
    // Refresh
    // -----------------------------------------------------------------------

    /// The difference from Google that matters most: Microsoft rotates the
    /// refresh token, and the rotated one must reach **disk**. Asserted by
    /// re-reading the file, not by inspecting the value in memory.
    #[tokio::test]
    async fn a_refresh_persists_the_rotated_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(json(&format!(
                r#"{{"access_token":"fresh-access","refresh_token":"{ROTATED_REFRESH}",
                     "expires_in":3600,"scope":"offline_access Mail.Read"}}"#
            )))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store
            .write(
                "kth",
                &tokens_expiring_at(Utc::now() - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let config = app_config_for(&server.uri());
        let auth = Auth::new(config, store_in(tmp.path())).unwrap();

        let token = auth.access_token("kth").await.unwrap();
        assert_eq!(token, "fresh-access");

        // Off disk, in a fresh store: an in-memory value proves nothing about
        // what survives a daemon restart.
        let persisted = store_in(tmp.path()).read("kth").unwrap();
        assert_eq!(
            persisted.refresh_token, ROTATED_REFRESH,
            "Microsoft retires the old refresh token; keeping it on disk leaves a \
             grant that stops working at the next restart"
        );
        assert_eq!(persisted.access_token, "fresh-access");
        assert!(persisted.expiry > Utc::now() + chrono::Duration::minutes(50));
    }

    /// A blank rotated token would erase the grant. Refusing leaves the
    /// previous one in place and says so.
    #[tokio::test]
    async fn an_empty_rotated_refresh_token_is_refused_rather_than_saved() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/token"))
            .respond_with(json(
                r#"{"access_token":"fresh","refresh_token":"   ","expires_in":3600}"#,
            ))
            .mount(&server)
            .await;

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store
            .write(
                "kth",
                &tokens_expiring_at(Utc::now() - chrono::Duration::minutes(5)),
            )
            .unwrap();

        let auth = Auth::new(app_config_for(&server.uri()), store_in(tmp.path())).unwrap();
        let err = auth.access_token("kth").await.unwrap_err().to_string();
        assert!(err.contains("empty refresh token"), "{err}");

        let persisted = store_in(tmp.path()).read("kth").unwrap();
        assert_eq!(
            persisted.refresh_token, REFRESH_TOKEN,
            "nothing may be written when the replacement is unusable"
        );
    }

    /// A healthy token is handed straight back; nothing touches the network.
    #[tokio::test]
    async fn a_fresh_token_is_returned_without_a_refresh() {
        struct Exploding;
        impl RefreshBackend for Exploding {
            fn refresh<'a>(
                &'a self,
                _refresh_token: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, RefreshError>> + Send + 'a>>
            {
                Box::pin(async { panic!("a healthy token must not be refreshed") })
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store
            .write(
                "kth",
                &tokens_expiring_at(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();

        let auth = Auth::with_backend(store_in(tmp.path()), Arc::new(Exploding));
        assert_eq!(auth.access_token("kth").await.unwrap(), ACCESS_TOKEN);
    }

    /// A revoked grant answers the same way forever. Remembering that is what
    /// keeps a poll from issuing one token POST per request against a dead
    /// account.
    #[tokio::test]
    async fn a_revoked_grant_is_remembered_and_not_re_asked() {
        struct Counting(AtomicUsize);
        impl RefreshBackend for Counting {
            fn refresh<'a>(
                &'a self,
                _refresh_token: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, RefreshError>> + Send + 'a>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Err(RefreshError::InvalidGrant) })
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store
            .write(
                "kth",
                &tokens_expiring_at(Utc::now() - chrono::Duration::hours(1)),
            )
            .unwrap();

        let backend = Arc::new(Counting(AtomicUsize::new(0)));
        let auth = Auth::with_backend(store_in(tmp.path()), backend.clone());

        let first = auth.access_token("kth").await.unwrap_err().to_string();
        let second = auth.access_token("kth").await.unwrap_err().to_string();

        assert_eq!(
            backend.0.load(Ordering::SeqCst),
            1,
            "a grant Microsoft has revoked must be asked about once, not every call"
        );
        assert_eq!(
            first, second,
            "the message must be identical, or the daemon treats each poll as news"
        );
        assert!(first.contains("ea-kth-authorize kth"), "{first}");
    }

    /// A transient failure is **not** a revoked grant, and must not be
    /// remembered as one.
    #[tokio::test]
    async fn a_transient_failure_is_retried_on_the_next_call() {
        struct Counting(AtomicUsize);
        impl RefreshBackend for Counting {
            fn refresh<'a>(
                &'a self,
                _refresh_token: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, RefreshError>> + Send + 'a>>
            {
                self.0.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Err(RefreshError::Other(anyhow::anyhow!("connection reset"))) })
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store
            .write(
                "kth",
                &tokens_expiring_at(Utc::now() - chrono::Duration::hours(1)),
            )
            .unwrap();

        let backend = Arc::new(Counting(AtomicUsize::new(0)));
        let auth = Auth::with_backend(store_in(tmp.path()), backend.clone());

        let err = auth.access_token("kth").await.unwrap_err().to_string();
        let _ = auth.access_token("kth").await;

        assert_eq!(backend.0.load(Ordering::SeqCst), 2);
        assert!(
            !err.contains("ea-kth-authorize"),
            "a dropped connection must not send the owner to a browser: {err}"
        );
    }

    /// `refresh_after_unauthorized` with a token somebody else already
    /// replaced returns the new one without a network call.
    #[tokio::test]
    async fn a_401_on_a_token_already_replaced_costs_no_refresh() {
        struct Exploding;
        impl RefreshBackend for Exploding {
            fn refresh<'a>(
                &'a self,
                _refresh_token: &'a str,
            ) -> Pin<Box<dyn Future<Output = Result<TokenResponse, RefreshError>> + Send + 'a>>
            {
                Box::pin(async { panic!("another task already refreshed this account") })
            }
        }

        let tmp = tempfile::TempDir::new().unwrap();
        let store = store_in(tmp.path());
        store
            .write(
                "kth",
                &tokens_expiring_at(Utc::now() + chrono::Duration::hours(1)),
            )
            .unwrap();

        let auth = Auth::with_backend(store_in(tmp.path()), Arc::new(Exploding));
        let fresh = auth
            .refresh_after_unauthorized("kth", "a-token-from-before")
            .await
            .unwrap();
        assert_eq!(fresh, ACCESS_TOKEN);
    }

    #[tokio::test]
    async fn an_invalid_grant_body_is_recognised_by_its_error_field() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/token"))
            .respond_with(ResponseTemplate::new(400).set_body_raw(
                r#"{"error":"invalid_grant","error_description":"AADSTS700082: The refresh token has expired."}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let backend = HttpTokenBackend::new(&app_config_for(&server.uri())).unwrap();
        let err = backend.refresh("whatever").await.unwrap_err();
        assert!(matches!(err, RefreshError::InvalidGrant), "{err:?}");
    }

    /// An `error_description` that merely mentions the phrase is not a dead
    /// grant: a 503 that says "invalid_grant" in prose must stay transient.
    #[tokio::test]
    async fn a_body_that_only_mentions_invalid_grant_is_not_treated_as_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/token"))
            .respond_with(ResponseTemplate::new(503).set_body_raw(
                r#"{"error":"temporarily_unavailable","error_description":"not an invalid_grant"}"#,
                "application/json",
            ))
            .mount(&server)
            .await;

        let backend = HttpTokenBackend::new(&app_config_for(&server.uri())).unwrap();
        let err = backend.refresh("whatever").await.unwrap_err();
        assert!(matches!(err, RefreshError::Other(_)), "{err:?}");
    }

    /// A 2xx body is never quoted, because a 2xx body is where the tokens are.
    #[tokio::test]
    async fn an_unparseable_success_body_is_never_quoted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/token"))
            .respond_with(json(&format!(
                r#"{{"unexpected":"shape","refresh_token":"{REFRESH_TOKEN}"}}"#
            )))
            .mount(&server)
            .await;

        let backend = HttpTokenBackend::new(&app_config_for(&server.uri())).unwrap();
        let err = backend.refresh("whatever").await.unwrap_err().to_string();
        assert!(
            !err.contains(REFRESH_TOKEN),
            "a success body must never reach an error message: {err}"
        );
        assert!(err.contains("deliberately not quoted"), "{err}");
    }

    /// The code exchange proves possession of the verifier, and the verifier
    /// goes in the POST body — never in a URL, which is what logs.
    #[tokio::test]
    async fn the_code_exchange_sends_the_verifier_in_the_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_matcher("/token"))
            .and(body_string_contains("code_verifier=the-verifier"))
            .and(body_string_contains("grant_type=authorization_code"))
            .respond_with(json(
                r#"{"access_token":"a","refresh_token":"r","expires_in":3600,
                     "scope":"offline_access Mail.Read"}"#,
            ))
            .mount(&server)
            .await;

        let backend = HttpTokenBackend::new(&app_config_for(&server.uri())).unwrap();
        let response = backend
            .exchange_code(
                "the-code",
                "the-verifier",
                DEFAULT_REDIRECT_URI,
                &SCOPES.join(" "),
            )
            .await
            .unwrap();
        assert_eq!(response.refresh_token.as_deref(), Some("r"));
    }

    // -----------------------------------------------------------------------
    // Redaction
    // -----------------------------------------------------------------------

    #[test]
    fn nothing_that_holds_a_secret_prints_it() {
        let tokens = tokens_expiring_at(Utc::now());
        let rendered = format!("{tokens:?}");
        assert!(!rendered.contains(ACCESS_TOKEN), "{rendered}");
        assert!(!rendered.contains(REFRESH_TOKEN), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");

        let response = TokenResponse {
            access_token: ACCESS_TOKEN.to_string(),
            refresh_token: Some(REFRESH_TOKEN.to_string()),
            expires_in: 3600,
            scope: None,
        };
        let rendered = format!("{response:?}");
        assert!(!rendered.contains(ACCESS_TOKEN), "{rendered}");
        assert!(!rendered.contains(REFRESH_TOKEN), "{rendered}");

        let backend = HttpTokenBackend::new(&app_config_for("https://example.test/x")).unwrap();
        let rendered = format!("{backend:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
    }
}
