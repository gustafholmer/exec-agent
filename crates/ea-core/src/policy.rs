use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Approve,
    Deny,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub mode: Mode,
    pub note: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub mode: Mode,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    connectors: HashMap<String, HashMap<String, Rule>>,
}

/// What a `policy.toml` value may look like: `"auto"` or
/// `{ mode = "approve", note = "..." }`.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawRule {
    Mode(String),
    Table {
        mode: String,
        #[serde(default)]
        note: Option<String>,
    },
}

fn parse_mode(raw: &str) -> anyhow::Result<Mode> {
    match raw {
        "auto" => Ok(Mode::Auto),
        "approve" => Ok(Mode::Approve),
        "deny" => Ok(Mode::Deny),
        other => bail!("invalid mode {other:?}; expected auto, approve, or deny"),
    }
}

impl Policy {
    pub fn parse(source: &str) -> anyhow::Result<Self> {
        let raw: HashMap<String, HashMap<String, RawRule>> =
            toml::from_str(source).context("parsing policy TOML")?;

        let mut connectors = HashMap::new();
        for (connector, tools) in raw {
            let mut section = HashMap::new();
            for (tool, rule) in tools {
                let rule = match rule {
                    RawRule::Mode(mode) => Rule {
                        mode: parse_mode(&mode)
                            .with_context(|| format!("in {connector}.{tool}"))?,
                        note: None,
                    },
                    RawRule::Table { mode, note } => Rule {
                        mode: parse_mode(&mode)
                            .with_context(|| format!("in {connector}.{tool}"))?,
                        note,
                    },
                };
                section.insert(tool, rule);
            }
            connectors.insert(connector, section);
        }
        Ok(Self { connectors })
    }

    /// Reads `<dir>/policy.toml` for each `(connector, dir)` pair and merges.
    ///
    /// A connector with no policy file is an error, not an empty policy: an
    /// empty policy would silently default every tool to `approve` and look
    /// like it worked.
    ///
    /// # Each connector polices only itself
    ///
    /// The pairs carry the connector's *own* name — from its `connector.toml`,
    /// which the daemon reads, not from anything inside `policy.toml` — and a
    /// file declaring any other `[section]` is rejected here rather than
    /// merged. Without that check the merge is a privilege-escalation path:
    /// dropping a directory containing
    ///
    /// ```toml
    /// [fortnox]
    /// record_voucher = "auto"
    /// ```
    ///
    /// anywhere under the connectors root would make company bookkeeping
    /// auto-executable, and the connector that shipped it need never be
    /// spawned. The gate is only as trustworthy as the table it decides from,
    /// so the table is assembled with one writer per section.
    ///
    /// Loud, not quiet: startup fails naming the file and the foreign section.
    /// A misconfiguration the owner can see and fix in a minute beats a
    /// silently widened gate they never learn about.
    pub fn load_dirs(connectors: &[(String, PathBuf)]) -> anyhow::Result<Self> {
        let mut merged = Policy::default();
        for (name, dir) in connectors {
            let file = dir.join("policy.toml");
            if !file.exists() {
                bail!("connector at {} has no policy.toml", dir.display());
            }
            let parsed = Policy::parse(&read_to_string(&file)?)
                .with_context(|| format!("in {}", file.display()))?;

            let mut foreign: Vec<&str> = parsed
                .connectors
                .keys()
                .filter(|section| section.as_str() != name.as_str())
                .map(String::as_str)
                .collect();
            if !foreign.is_empty() {
                foreign.sort_unstable();
                bail!(
                    "{} belongs to connector {name:?} but declares policy for {}; \
                     a connector may only police its own tools",
                    file.display(),
                    foreign
                        .iter()
                        .map(|s| format!("{s:?}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                );
            }
            if !parsed.connectors.contains_key(name.as_str()) {
                // Fail-closed already (every tool falls through to `approve`,
                // and `watch_poll` is not `auto`, so the connector is never
                // even polled), but it is always a mistake, so say so.
                tracing::warn!(
                    connector = %name,
                    file = %file.display(),
                    "policy.toml declares no [{name}] section: every tool defaults to approval"
                );
            }

            for (connector, tools) in parsed.connectors {
                merged
                    .connectors
                    .entry(connector)
                    .or_default()
                    .extend(tools);
            }
        }
        Ok(merged)
    }

    pub fn decide(&self, connector: &str, tool: &str) -> Decision {
        let Some(section) = self.connectors.get(connector) else {
            return Decision {
                mode: Mode::Approve,
                reason: format!("unknown connector {connector:?}; defaulting to approval"),
            };
        };
        let Some(rule) = section.get(tool) else {
            return Decision {
                mode: Mode::Approve,
                reason: format!(
                    "tool {tool:?} is not listed in the {connector} policy; defaulting to approval"
                ),
            };
        };
        Decision {
            mode: rule.mode,
            reason: rule
                .note
                .clone()
                .unwrap_or_else(|| format!("{connector}.{tool} is configured as {:?}", rule.mode)),
        }
    }

    pub fn tools_for(&self, connector: &str) -> Vec<&str> {
        self.connectors
            .get(connector)
            .map(|s| s.keys().map(String::as_str).collect())
            .unwrap_or_default()
    }
}

fn read_to_string(path: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Policy {
        Policy::parse(
            r#"
[canvas]
list_courses = "auto"
submit_assignment = "deny"

[fortnox]
record_voucher = "approve"
record_expense = { mode = "approve", note = "always, regardless of amount" }
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_bare_string_modes() {
        let p = fixture();
        assert_eq!(p.decide("canvas", "list_courses").mode, Mode::Auto);
        assert_eq!(p.decide("canvas", "submit_assignment").mode, Mode::Deny);
    }

    #[test]
    fn parses_table_form_with_a_note() {
        let p = fixture();
        let d = p.decide("fortnox", "record_expense");
        assert_eq!(d.mode, Mode::Approve);
        assert!(d.reason.contains("regardless of amount"));
    }

    #[test]
    fn rejects_an_unrecognised_mode() {
        let err = Policy::parse("[canvas]\nfoo = \"sometimes\"\n").unwrap_err();
        assert!(format!("{err:#}").contains("sometimes"));
    }

    #[test]
    fn rejects_a_non_string_non_table_value() {
        assert!(Policy::parse("[canvas]\nfoo = 3\n").is_err());
    }

    #[test]
    fn unknown_tool_on_a_known_connector_defaults_to_approve() {
        let d = fixture().decide("canvas", "invented_tool");
        assert_eq!(d.mode, Mode::Approve);
        assert!(d.reason.contains("not listed"));
    }

    #[test]
    fn unknown_connector_defaults_to_approve() {
        let d = fixture().decide("nonexistent", "whatever");
        assert_eq!(d.mode, Mode::Approve);
        assert!(d.reason.contains("unknown connector"));
    }

    // The single most important property in the system.
    #[test]
    fn nothing_unlisted_is_ever_auto() {
        let p = fixture();
        for tool in ["", "x", "list_courses ", "LIST_COURSES"] {
            assert_ne!(
                p.decide("canvas", tool).mode,
                Mode::Auto,
                "tool {tool:?} must not be auto"
            );
        }
        assert_ne!(p.decide("unknown", "list_courses").mode, Mode::Auto);
    }

    /// A `(name, dir)` pair for a connector whose `policy.toml` is `body`.
    fn connector_dir(name: &str, body: &str) -> (String, PathBuf, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("policy.toml"), body).unwrap();
        (name.to_string(), dir.path().to_path_buf(), dir)
    }

    #[test]
    fn load_dirs_errors_when_a_connector_has_no_policy() {
        let dir = tempfile::TempDir::new().unwrap();
        let err =
            Policy::load_dirs(&[("canvas".to_string(), dir.path().to_path_buf())]).unwrap_err();
        assert!(format!("{err:#}").contains("policy.toml"));
    }

    #[test]
    fn load_dirs_merges_several_connectors() {
        let (an, ad, _a) = connector_dir("canvas", "[canvas]\nx = \"auto\"\n");
        let (bn, bd, _b) = connector_dir("google", "[google]\ny = \"deny\"\n");
        let p = Policy::load_dirs(&[(an, ad), (bn, bd)]).unwrap();
        assert_eq!(p.decide("canvas", "x").mode, Mode::Auto);
        assert_eq!(p.decide("google", "y").mode, Mode::Deny);
    }

    /// The hole this check exists to close: any directory under the connectors
    /// root could widen the gate for a connector it has nothing to do with.
    #[test]
    fn a_connector_may_not_police_another_connectors_tools() {
        let (name, dir, _keep) = connector_dir(
            "canvas",
            "[canvas]\nlist_courses = \"auto\"\n\n[fortnox]\nrecord_voucher = \"auto\"\n",
        );
        let err = Policy::load_dirs(&[(name, dir)]).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("fortnox"), "{message}");
        assert!(message.contains("policy.toml"), "{message}");
        assert!(message.contains("own tools"), "{message}");
    }

    /// And it is rejected rather than partially applied: nothing is merged
    /// from a file that failed the check, including its legitimate half.
    #[test]
    fn a_foreign_section_is_refused_outright_not_merged_minus_the_foreign_half() {
        let (name, dir, _keep) = connector_dir(
            "canvas",
            "[canvas]\nlist_courses = \"auto\"\n\n[fortnox]\nrecord_voucher = \"auto\"\n",
        );
        let (ok_name, ok_dir, _keep_ok) = connector_dir("echo", "[echo]\necho = \"auto\"\n");
        assert!(Policy::load_dirs(&[(ok_name, ok_dir), (name, dir)]).is_err());
    }

    /// The legitimate case must keep working: a file naming only its own
    /// connector loads, and the section name is compared exactly.
    #[test]
    fn a_connector_policing_only_itself_loads() {
        let (name, dir, _keep) = connector_dir("canvas", "[canvas]\nlist_courses = \"auto\"\n");
        let p = Policy::load_dirs(&[(name, dir)]).unwrap();
        assert_eq!(p.decide("canvas", "list_courses").mode, Mode::Auto);
    }

    #[test]
    fn a_section_that_merely_resembles_the_connector_name_is_foreign() {
        for section in ["Canvas", "canvas2", "canvas "] {
            let (name, dir, _keep) =
                connector_dir("canvas", &format!("[\"{section}\"]\nx = \"auto\"\n"));
            let err = Policy::load_dirs(&[(name, dir)]).unwrap_err().to_string();
            assert!(err.contains("own tools"), "{section:?}: {err}");
        }
    }

    /// A policy file with no section of its own is a mistake, but a
    /// fail-closed one: it loads, and every tool defaults to approval.
    #[test]
    fn a_policy_with_no_section_of_its_own_still_denies_auto() {
        let (name, dir, _keep) = connector_dir("canvas", "");
        let p = Policy::load_dirs(&[(name, dir)]).unwrap();
        assert_ne!(p.decide("canvas", "watch_poll").mode, Mode::Auto);
    }
}
