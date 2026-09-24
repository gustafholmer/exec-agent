//! Notion credentials: one integration token per workspace, on disk.
//!
//! # Why this is a token store and not an OAuth client
//!
//! Google's grant is a three-leg OAuth flow because Google will not issue a
//! long-lived credential any other way. Notion will: an *internal
//! integration* created at <https://www.notion.so/my-integrations> hands the
//! workspace owner a secret (`ntn_…`) that does not expire and needs no
//! refresh. Notion also offers a public-integration OAuth flow, but that
//! exists so a *third party* can ask strangers for access, which is the
//! opposite of this project's shape — one owner, their own workspaces, their
//! own machine. So there is no [`crate::auth`] analogue of `ea-google`'s
//! `Auth`: no refresh mutex, no expiry clock, no forced-refresh-on-401. A
//! Notion 401 means the token is wrong or the integration was removed, and the
//! only cure is a human visiting the integrations page — which is exactly what
//! [`setup_instructions`] prints.
//!
//! What *is* copied from `ea-google`, deliberately and in full:
//!
//! * **One file per workspace**, `~/.config/exec-agent/notion/<workspace>.json`,
//!   so a corrupt or half-written file costs one workspace rather than all of
//!   them.
//! * **Mode `0600`, asserted on write and enforced on read.** The file holds a
//!   credential that reads and writes every page the integration is shared
//!   with.
//! * **Atomic writes.** Temp file in the same directory, `fsync`, `rename`.
//!   A crash leaves the old file or the new one, never a truncated one.
//! * **The workspace label is validated before it becomes a path.** Labels
//!   arrive from configuration *and* from tool arguments a language model
//!   produces, so `../../id_rsa` is a realistic input.
//! * **Nothing here can print a token.** [`Credentials`] has a hand-written
//!   `Debug`; no error quotes a file's contents, only its path.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};

/// The connector's name, equal to its directory's basename
/// (`connectors/notion`), which the daemon enforces at startup. Declared here
/// so the manifest Task 2 writes cannot drift from the directory this crate
/// reads its credentials out of.
pub const CONNECTOR: &str = "notion";

/// Where a person goes to mint the token this store holds.
pub const INTEGRATIONS_URL: &str = "https://www.notion.so/my-integrations";

/// Reject anything that is not `^[a-z0-9][a-z0-9_-]*$`, **before** a path is
/// built from it.
///
/// Identical rule, and identical reasoning, to `ea_google::auth`'s account
/// validation: `Path::join` with an absolute path replaces the base entirely
/// and `..` walks out of the config directory, so an unvalidated label turns
/// [`TokenStore::read`] into a file-read primitive and [`TokenStore::write`]
/// into an overwrite primitive.
///
/// Lower-case only, because the label becomes a filename on APFS, which is
/// case-insensitive by default: `work` and `Work` would be two workspaces as
/// far as the rest of this crate is concerned, sharing one file, so
/// authorising one would silently destroy the other's token.
pub fn validate_workspace(workspace: &str) -> anyhow::Result<()> {
    let lower = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = workspace.chars();
    let ok = match chars.next() {
        Some(first) if lower(first) => chars.all(|c| lower(c) || c == '_' || c == '-'),
        _ => false,
    };
    if !ok {
        let hint = if workspace.chars().any(|c| c.is_ascii_uppercase()) {
            format!(
                " Labels are lower-case only, because they become filenames on a \
                 case-insensitive disk where {:?} and {workspace:?} would be one file \
                 holding two workspaces' tokens; use {:?}.",
                workspace.to_ascii_lowercase(),
                workspace.to_ascii_lowercase()
            )
        } else {
            String::new()
        };
        bail!(
            "{workspace:?} is not a usable Notion workspace label. A label must match \
             ^[a-z0-9][a-z0-9_-]*$ (for example \"personal\" or \"work\"); it becomes a \
             filename, so \"/\" and \"..\" are refused.{hint}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// One workspace's Notion integration token, as stored on disk.
///
/// No `Debug` derive: [`Credentials::token`] is a bearer credential with the
/// full read/write reach of whatever the integration has been shared with.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Credentials {
    /// The integration secret. Notion mints these as `ntn_…`; tokens issued
    /// before the 2024 rename begin `secret_…`. The prefix is not checked —
    /// a prefix test that is wrong the day Notion changes it fails closed on
    /// a perfectly good token, and the API's own 401 is the authoritative
    /// answer anyway.
    pub token: String,
    /// The workspace's human name, as Notion shows it. Purely for the
    /// reader's benefit in error messages and `list` output; the *label*
    /// (the filename stem) is what the code keys on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("token", &"<redacted>")
            .field("workspace_name", &self.workspace_name)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// The token store
// ---------------------------------------------------------------------------

/// One JSON file per workspace under `~/.config/exec-agent/notion/`, mode
/// `0600`.
#[derive(Debug, Clone)]
pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    /// `dir` of `None` means `~/.config/exec-agent/notion/` (or
    /// `$EA_CONFIG_DIR/notion/`), created `0700` if absent.
    pub fn new(dir: Option<PathBuf>) -> Self {
        let dir = dir.unwrap_or_else(|| ea_core::paths::connector_config_dir(CONNECTOR));
        Self { dir }
    }

    /// The directory the store reads and writes.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The path a workspace's credentials live at. Errors rather than
    /// returning a path for a label that failed validation, so there is no
    /// way to obtain a path from this type without passing the check.
    pub fn path_for(&self, workspace: &str) -> anyhow::Result<PathBuf> {
        validate_workspace(workspace)?;
        Ok(self.dir.join(format!("{workspace}.json")))
    }

    /// Read one workspace's credentials.
    ///
    /// A missing file is the ordinary state of a fresh install, so the error
    /// names both the workspace and the steps that fix it rather than just
    /// reporting `ENOENT`.
    pub fn read(&self, workspace: &str) -> anyhow::Result<Credentials> {
        use std::os::unix::fs::PermissionsExt;

        let path = self.path_for(workspace)?;

        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                bail!(
                    "no Notion credentials for workspace {workspace:?} at {}.\n{}",
                    path.display(),
                    setup_instructions(workspace, &path)
                );
            }
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", path.display()));
            }
        };

        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            bail!(
                "{} is mode {mode:04o}; it holds a Notion integration token and must be \
                 0600 (run: chmod 600 {})",
                path.display(),
                path.display()
            );
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        // The serde error is quoted; the file's contents are not. A malformed
        // credential file is precisely where a token may be sitting in an
        // unexpected place.
        let creds: Credentials = serde_json::from_str(&text).map_err(|err| {
            anyhow::anyhow!(
                "{} is not a readable Notion credential file ({err}). Delete it and \
                 write a fresh one.\n{}",
                path.display(),
                setup_instructions(workspace, &path)
            )
        })?;

        if creds.token.trim().is_empty() {
            bail!(
                "{} holds an empty Notion token for workspace {workspace:?}.\n{}",
                path.display(),
                setup_instructions(workspace, &path)
            );
        }

        Ok(creds)
    }

    /// Write one workspace's credentials, atomically, mode `0600`.
    ///
    /// Temp file in the same directory (so `rename` stays on one filesystem
    /// and is therefore atomic), `fsync`, then `rename`. A crash at any point
    /// leaves either the old file or the new one, never a truncated one — and
    /// a truncated credential file means a trip back to the integrations page.
    pub fn write(&self, workspace: &str, creds: &Credentials) -> anyhow::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let path = self.path_for(workspace)?;

        std::fs::create_dir_all(&self.dir)
            .with_context(|| format!("creating {}", self.dir.display()))?;
        let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));

        // Unique per process and per call: two daemons, or two tasks, must not
        // share a temp file and interleave their bytes.
        let tmp = self.dir.join(format!(
            ".{workspace}.json.tmp.{}.{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));

        // 0600 from the moment the file exists, not set afterwards — which
        // would leave a window in which the token is world-readable.
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;

        let body = serde_json::to_vec_pretty(creds).context("serialising Notion credentials")?;

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

        // Re-assert the mode: `create_new(..).mode(..)` is masked by the
        // process umask, and `rename` preserves whatever the temp file has.
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

    /// The workspace labels that have credential files, sorted.
    ///
    /// A missing directory is an empty list, not an error: nothing has been
    /// set up yet. Dotted temp files are skipped, as is anything whose stem
    /// would not pass [`validate_workspace`] — such a file was not written by
    /// [`TokenStore::write`].
    pub fn list(&self) -> anyhow::Result<Vec<String>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("reading {}", self.dir.display()));
            }
        };

        let mut workspaces = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", self.dir.display()))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".json") else {
                continue;
            };
            if validate_workspace(stem).is_err() {
                continue;
            }
            workspaces.push(stem.to_string());
        }
        workspaces.sort();
        Ok(workspaces)
    }
}

/// The steps a person follows to give this connector a workspace.
///
/// Every message about a missing, unreadable, or rejected credential ends with
/// this, because in each of those cases the reader's next action is the same
/// and it is not something a program can do for them. Step 3 is the one people
/// skip: a Notion integration sees *nothing* until a page is explicitly shared
/// with it, so a perfectly valid token returns an empty workspace and looks
/// like a broken connector.
pub fn setup_instructions(workspace: &str, path: &Path) -> String {
    let dir = path.parent().unwrap_or(Path::new(".")).display();
    format!(
        "Set it up:\n  \
         1. Create an internal integration at {INTEGRATIONS_URL} and copy its secret.\n  \
         2. mkdir -p {dir} && printf '{{\"token\":\"ntn_...\"}}' > {}\n     \
            chmod 600 {}\n  \
         3. In Notion, open each page or database the connector should see, and use\n     \
            \"Connections\" -> your integration to share it. An integration that has not\n     \
            been shared with anything sees an empty workspace, which is indistinguishable\n     \
            from a quiet one.\n  \
         (workspace label: {workspace:?})",
        path.display(),
        path.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;

    fn store() -> (tempfile::TempDir, TokenStore) {
        let dir = tempfile::tempdir().expect("a temp dir");
        let store = TokenStore::new(Some(dir.path().join("notion")));
        (dir, store)
    }

    fn creds(token: &str) -> Credentials {
        Credentials {
            token: token.to_string(),
            workspace_name: None,
        }
    }

    // -----------------------------------------------------------------------
    // Label validation
    // -----------------------------------------------------------------------

    #[test]
    fn ordinary_labels_are_accepted() {
        for label in ["work", "personal", "acme-inc", "ws_2", "a", "x1"] {
            validate_workspace(label).unwrap_or_else(|err| panic!("{label:?} should pass: {err}"));
        }
    }

    #[test]
    fn a_label_that_could_escape_the_config_directory_is_refused() {
        for label in [
            "..",
            "../../.ssh/id_rsa",
            "/etc/passwd",
            "work/../..",
            "",
            ".hidden",
            "-leading",
            "with space",
            "emoji\u{1f4cc}",
        ] {
            assert!(
                validate_workspace(label).is_err(),
                "{label:?} must be refused"
            );
        }
    }

    #[test]
    fn an_upper_case_label_is_refused_and_the_message_offers_the_lower_case_one() {
        let err = validate_workspace("Work").expect_err("upper case is refused");
        let text = format!("{err}");
        assert!(text.contains("\"work\""), "should suggest \"work\": {text}");
    }

    #[test]
    fn a_rejected_label_never_produces_a_path() {
        let (_tmp, store) = store();
        assert!(store.path_for("../escape").is_err());
        assert!(store.read("../escape").is_err());
        assert!(store.write("../escape", &creds("ntn_x")).is_err());
    }

    // -----------------------------------------------------------------------
    // Round trip
    // -----------------------------------------------------------------------

    #[test]
    fn a_written_token_reads_back() {
        let (_tmp, store) = store();
        let written = Credentials {
            token: "ntn_secret_value".to_string(),
            workspace_name: Some("Gustaf's Notion".to_string()),
        };
        store.write("work", &written).expect("write succeeds");

        let read = store.read("work").expect("read succeeds");
        assert_eq!(read.token, "ntn_secret_value");
        assert_eq!(read.workspace_name.as_deref(), Some("Gustaf's Notion"));
    }

    #[test]
    fn the_credential_file_is_0600_and_its_directory_0700() {
        let (_tmp, store) = store();
        store.write("work", &creds("ntn_x")).expect("write");

        let path = store.path_for("work").expect("a path");
        let mode = std::fs::metadata(&path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "credential file mode");

        let dir_mode = std::fs::metadata(store.dir())
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "credential directory mode");
    }

    #[test]
    fn a_write_leaves_no_temp_file_behind() {
        let (_tmp, store) = store();
        store.write("work", &creds("ntn_x")).expect("write");
        store.write("work", &creds("ntn_y")).expect("overwrite");

        let leftovers: Vec<String> = std::fs::read_dir(store.dir())
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
        assert_eq!(store.read("work").expect("read").token, "ntn_y");
    }

    #[test]
    fn a_group_readable_file_is_refused_rather_than_read() {
        let (_tmp, store) = store();
        store.write("work", &creds("ntn_x")).expect("write");
        let path = store.path_for("work").expect("a path");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");

        let err = store.read("work").expect_err("0640 is refused");
        let text = format!("{err}");
        assert!(text.contains("0640"), "should name the mode: {text}");
        assert!(text.contains("chmod 600"), "should say how to fix: {text}");
    }

    // -----------------------------------------------------------------------
    // Missing and malformed
    // -----------------------------------------------------------------------

    #[test]
    fn a_missing_file_names_the_workspace_and_the_setup_steps() {
        let (_tmp, store) = store();
        let err = store.read("work").expect_err("nothing is authorised yet");
        let text = format!("{err}");
        assert!(text.contains("\"work\""), "names the workspace: {text}");
        assert!(text.contains(INTEGRATIONS_URL), "names the page: {text}");
        assert!(
            text.contains("Connections"),
            "names the sharing step, which is the one people skip: {text}"
        );
    }

    #[test]
    fn a_corrupt_file_reports_the_path_but_never_its_contents() {
        let (_tmp, store) = store();
        std::fs::create_dir_all(store.dir()).expect("mkdir");
        let path = store.path_for("work").expect("a path");
        std::fs::write(&path, "{\"token\": \"ntn_leaked_secret\"").expect("write a truncated file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");

        let err = store.read("work").expect_err("truncated JSON is refused");
        let text = format!("{err}");
        assert!(text.contains(&path.display().to_string()), "names the path");
        assert!(
            !text.contains("ntn_leaked_secret"),
            "must not quote the file's contents: {text}"
        );
    }

    #[test]
    fn an_empty_token_is_refused_at_read_time() {
        let (_tmp, store) = store();
        store.write("work", &creds("   ")).expect("write");
        let err = store.read("work").expect_err("an empty token is useless");
        assert!(format!("{err}").contains("empty"));
    }

    // -----------------------------------------------------------------------
    // list
    // -----------------------------------------------------------------------

    #[test]
    fn list_is_empty_rather_than_an_error_before_anything_is_set_up() {
        let (_tmp, store) = store();
        assert_eq!(store.list().expect("list"), Vec::<String>::new());
    }

    #[test]
    fn list_returns_every_workspace_sorted_and_skips_what_it_did_not_write() {
        let (_tmp, store) = store();
        store.write("work", &creds("a")).expect("write");
        store.write("personal", &creds("b")).expect("write");

        std::fs::write(store.dir().join("notes.txt"), "x").expect("a stray file");
        std::fs::write(store.dir().join("Bad.json"), "{}").expect("a stray upper-case file");
        std::fs::write(store.dir().join(".work.json.tmp.1.2"), "{}").expect("a stray temp file");

        assert_eq!(
            store.list().expect("list"),
            vec!["personal".to_string(), "work".to_string()]
        );
    }

    // -----------------------------------------------------------------------
    // Redaction
    // -----------------------------------------------------------------------

    #[test]
    fn debug_never_prints_the_token() {
        let creds = Credentials {
            token: "ntn_this_must_not_appear".to_string(),
            workspace_name: Some("Acme".to_string()),
        };
        let text = format!("{creds:?}");
        assert!(!text.contains("ntn_this_must_not_appear"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        // The non-secret field is still useful and still printed.
        assert!(text.contains("Acme"), "{text}");
    }
}
