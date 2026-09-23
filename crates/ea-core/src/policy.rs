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

    /// Reads `<dir>/policy.toml` from each directory and merges. A connector
    /// with no policy file is an error, not an empty policy: an empty policy
    /// would silently default every tool to `approve` and look like it worked.
    pub fn load_dirs(dirs: &[PathBuf]) -> anyhow::Result<Self> {
        let mut merged = Policy::default();
        for dir in dirs {
            let file = dir.join("policy.toml");
            if !file.exists() {
                bail!("connector at {} has no policy.toml", dir.display());
            }
            let parsed = Policy::parse(&read_to_string(&file)?)
                .with_context(|| format!("in {}", file.display()))?;
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

    #[test]
    fn load_dirs_errors_when_a_connector_has_no_policy() {
        let dir = tempfile::TempDir::new().unwrap();
        let err = Policy::load_dirs(&[dir.path().to_path_buf()]).unwrap_err();
        assert!(format!("{err:#}").contains("policy.toml"));
    }

    #[test]
    fn load_dirs_merges_several_connectors() {
        let a = tempfile::TempDir::new().unwrap();
        let b = tempfile::TempDir::new().unwrap();
        std::fs::write(a.path().join("policy.toml"), "[canvas]\nx = \"auto\"\n").unwrap();
        std::fs::write(b.path().join("policy.toml"), "[google]\ny = \"deny\"\n").unwrap();
        let p = Policy::load_dirs(&[a.path().to_path_buf(), b.path().to_path_buf()]).unwrap();
        assert_eq!(p.decide("canvas", "x").mode, Mode::Auto);
        assert_eq!(p.decide("google", "y").mode, Mode::Deny);
    }
}
