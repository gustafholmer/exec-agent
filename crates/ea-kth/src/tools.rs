//! The MCP surface: two reads plus `watch_poll`, over the transport seam.
//!
//! # Every tool names its account
//!
//! There is no such thing as "the" KTH account. Every tool except
//! [`KthServer::watch_poll`] takes a required `account` argument with no
//! default, pinned by
//! `every_account_scoped_tool_requires_an_account_with_no_default`. In
//! practice the owner will authorise one mailbox and the argument will always
//! be `kth` — which is exactly the situation in which a default looks
//! harmless and quietly becomes wrong the day a second one exists. A tool
//! that defaults its account reads the wrong mailbox, and nothing downstream
//! — triage, the notification, the owner reading it — can tell.
//!
//! `watch_poll` is the one exception and cannot be otherwise: the daemon calls
//! it with `{}` on a timer (see `ea_daemon::jobs::run_watch_poll`) and its job
//! is to cover *every* authorised account at once. It takes no arguments at
//! all, which the same test asserts rather than leaves implied.
//!
//! # Nothing here writes
//!
//! There is no draft tool and no send tool, and the OAuth scopes
//! ([`crate::auth::SCOPES`]) contain neither `Mail.ReadWrite` nor `Mail.Send`,
//! so Microsoft itself would refuse one. `connectors/kth/policy.toml` denies
//! both by name anyway, because the gate matches rules by name and the door
//! should be shut before anybody opens it. Adding a draft tool later is a
//! deliberate act that has to touch all three places.
//!
//! # Failing loudly
//!
//! Every error propagates as a tool error. See the `watch` module docs for why
//! an empty array on failure would leave a lapsed token looking healthy
//! forever.

use std::sync::Arc;

use chrono::Utc;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::mail::MailTransport;
use crate::watch::{self, WatchEntry};

/// Tools named in `policy.toml` that this server deliberately does **not**
/// implement.
///
/// Both are declared `deny` so the rule is already in force the moment such a
/// tool appears, rather than after somebody remembers to review the policy.
/// `create_draft` is here rather than implemented because implementing it
/// would mean asking KTH for `Mail.ReadWrite`, and this connector asks for the
/// minimum it needs. `send_mail` is here because nothing in this project may
/// ever send mail as the owner.
pub const DELIBERATE_PLACEHOLDERS: &[&str] = &["create_draft", "send_mail"];

/// The one tool that takes no `account`, because the daemon calls it with `{}`
/// and it covers every authorised account. See the module docs.
pub const ACCOUNT_EXEMPT_TOOLS: &[&str] = &["watch_poll"];

/// How many messages `list_mail` returns when a caller does not say.
pub const DEFAULT_MAIL_MAX: u32 = 25;

/// The most a caller may ask for in one go. A model that asked for ten
/// thousand would get a response too large to be useful to anyone.
pub const MAX_MAIL_MAX: u32 = 100;

/// Either a working transport, or the reason there isn't one.
///
/// A connector with no `app.json` still starts and still completes the MCP
/// handshake; it just fails every call with a message naming the file to
/// create. Exiting at start-up instead would reach the daemon as "handshake
/// failed", which tells nobody what to do.
enum Backend {
    Ready(Arc<dyn MailTransport>),
    Unconfigured(String),
}

#[derive(Clone)]
pub struct KthServer {
    backend: Arc<Backend>,
    #[expect(
        dead_code,
        reason = "read by the code the #[tool_handler] macro generates"
    )]
    tool_router: ToolRouter<Self>,
}

impl KthServer {
    pub fn new(transport: Arc<dyn MailTransport>) -> Self {
        Self {
            backend: Arc::new(Backend::Ready(transport)),
            tool_router: Self::tool_router(),
        }
    }

    /// A server that will answer every call with `reason`.
    pub fn unconfigured(reason: impl Into<String>) -> Self {
        Self {
            backend: Arc::new(Backend::Unconfigured(reason.into())),
            tool_router: Self::tool_router(),
        }
    }

    fn transport(&self) -> Result<&Arc<dyn MailTransport>, String> {
        match &*self.backend {
            Backend::Ready(transport) => Ok(transport),
            Backend::Unconfigured(reason) => Err(format!(
                "kth: this connector has no usable credentials, so it cannot read \
                 anything from the KTH mailbox. {reason}"
            )),
        }
    }

    /// The accounts that have a stored grant, in a stable order. Read fresh on
    /// every poll rather than latched at start-up, so authorising an account
    /// does not need a daemon restart.
    pub fn authorised_accounts(&self) -> Result<Vec<String>, String> {
        self.transport()?.accounts().map_err(render)
    }

    /// What [`KthServer::watch_poll`] returns, before serialisation. Public so
    /// the tests can look at the rows rather than at a string.
    pub async fn poll(&self) -> Result<Vec<WatchEntry>, String> {
        let transport = self.transport()?;
        let accounts = self.authorised_accounts()?;
        watch::poll(transport.as_ref(), &accounts, Utc::now())
            .await
            .map_err(render)
    }
}

/// `anyhow` error -> the string the model reads. `{:#}` keeps the context
/// chain, which is where the account and the failing URL live.
fn render(err: anyhow::Error) -> String {
    format!("{err:#}")
}

fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value).map_err(|err| format!("kth: could not serialise the reply: {err}"))
}

// ---------------------------------------------------------------------------
// Tool arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListMailArgs {
    /// Which KTH account to read — the label it was authorised under, usually
    /// `kth`. Required: there is no default mailbox.
    pub account: String,
    /// How many messages to return. Defaults to 25, capped at 100.
    pub max: Option<u32>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct GetMailArgs {
    /// Which KTH account to read — the label it was authorised under, usually
    /// `kth`. Required, and must be the account the id came from.
    pub account: String,
    /// The Microsoft Graph message id, as returned by `list_mail`.
    pub id: String,
}

#[tool_router]
impl KthServer {
    #[tool(
        description = "List unread messages in one KTH mailbox's inbox, newest first, as a JSON \
                       array of { id, conversation_id, account, from, subject, preview, body, \
                       received_at, is_read, web_link }. Junk and Deleted Items are not \
                       included. Read-only. `account` is required: there is no default \
                       mailbox."
    )]
    pub async fn list_mail(
        &self,
        Parameters(ListMailArgs { account, max }): Parameters<ListMailArgs>,
    ) -> Result<String, String> {
        let transport = self.transport()?;
        let max = max.unwrap_or(DEFAULT_MAIL_MAX).clamp(1, MAX_MAIL_MAX) as usize;
        let mail = transport.list_unread(&account, max).await.map_err(render)?;
        to_json(&mail)
    }

    #[tool(
        description = "Read one message in full from a KTH mailbox, by the id list_mail \
                       returned, as { id, conversation_id, account, from, subject, preview, \
                       body, received_at, is_read, web_link }. Read-only. `account` is \
                       required, and must be the account the id came from."
    )]
    pub async fn get_mail(
        &self,
        Parameters(GetMailArgs { account, id }): Parameters<GetMailArgs>,
    ) -> Result<String, String> {
        let transport = self.transport()?;
        let mail = transport.get(&account, &id).await.map_err(render)?;
        to_json(&mail)
    }

    #[tool(
        description = "Poll the KTH mailbox for unread mail across every authorised account. \
                       Returns a JSON array of { external_id, kind, payload }: one entry per \
                       unread message (kind `mail`). An account that cannot be read \
                       contributes one `connector_error` entry instead of failing the poll, so \
                       one lapsed grant does not silence the others; if every account fails, \
                       the poll errors. Takes no arguments — it covers every account by design."
    )]
    pub async fn watch_poll(&self) -> Result<String, String> {
        let entries = self.poll().await?;
        to_json(&entries)
    }
}

#[tool_handler]
impl ServerHandler for KthServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "The owner's KTH mailbox (Exchange Online, read through Microsoft Graph). Every \
             tool takes a required `account` argument — the label the mailbox was authorised \
             under, usually `kth`; there is no default account. Everything here is \
             read-only: this connector cannot draft, send, move, delete or mark anything, \
             and the OAuth grant it holds does not permit it to.",
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    use chrono::DateTime;

    use crate::mail::{BoxFuture, Mail};

    /// A transport with no Microsoft in it — the same shape `watch`'s tests
    /// use, and the same point: nothing above the seam knows what is below it.
    struct FakeTransport {
        accounts: Vec<String>,
    }

    impl MailTransport for FakeTransport {
        fn accounts(&self) -> anyhow::Result<Vec<String>> {
            Ok(self.accounts.clone())
        }

        fn list_unread<'a>(
            &'a self,
            account: &'a str,
            _max: usize,
        ) -> BoxFuture<'a, anyhow::Result<Vec<Mail>>> {
            Box::pin(async move {
                Ok(vec![Mail {
                    id: "m1".to_string(),
                    conversation_id: "c1".to_string(),
                    account: account.to_string(),
                    from: "kurs@kth.se".to_string(),
                    subject: "Tentamen".to_string(),
                    preview: "p".to_string(),
                    body: "Tentamen flyttad.".to_string(),
                    received_at: DateTime::<Utc>::UNIX_EPOCH,
                    is_read: false,
                    web_link: String::new(),
                }])
            })
        }

        fn get<'a>(&'a self, account: &'a str, id: &'a str) -> BoxFuture<'a, anyhow::Result<Mail>> {
            Box::pin(async move {
                Ok(Mail {
                    id: id.to_string(),
                    conversation_id: "c1".to_string(),
                    account: account.to_string(),
                    from: "kurs@kth.se".to_string(),
                    subject: "Tentamen".to_string(),
                    preview: "p".to_string(),
                    body: "hel text".to_string(),
                    received_at: DateTime::<Utc>::UNIX_EPOCH,
                    is_read: false,
                    web_link: String::new(),
                })
            })
        }

        fn authorize_hint(&self, account: &str) -> String {
            format!("ea-kth-authorize {account}")
        }
    }

    fn server_with(accounts: &[&str]) -> KthServer {
        KthServer::new(Arc::new(FakeTransport {
            accounts: accounts.iter().map(|a| a.to_string()).collect(),
        }))
    }

    // -----------------------------------------------------------------------
    // The tools
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn list_mail_reads_the_named_account() {
        let text = server_with(&["kth"])
            .list_mail(Parameters(ListMailArgs {
                account: "kth".into(),
                max: None,
            }))
            .await
            .unwrap();
        let mail: Vec<Mail> = serde_json::from_str(&text).unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].account, "kth");
        assert_eq!(mail[0].body, "Tentamen flyttad.");
    }

    #[tokio::test]
    async fn get_mail_returns_the_message_it_was_asked_for() {
        let text = server_with(&["kth"])
            .get_mail(Parameters(GetMailArgs {
                account: "kth".into(),
                id: "AAMk=".into(),
            }))
            .await
            .unwrap();
        let mail: Mail = serde_json::from_str(&text).unwrap();
        assert_eq!(mail.id, "AAMk=");
    }

    #[tokio::test]
    async fn watch_poll_covers_every_authorised_account() {
        let entries = server_with(&["kth", "kth-staff"]).poll().await.unwrap();
        let ids: BTreeSet<&str> = entries.iter().map(|e| e.external_id.as_str()).collect();
        assert_eq!(
            ids,
            ["kthmail:kth-staff:m1", "kthmail:kth:m1"]
                .into_iter()
                .collect::<BTreeSet<&str>>()
        );
    }

    /// The daemon calls `watch_poll` with `{}`; if the accounts it should
    /// cover are unknown, that is a failure, not a quiet success.
    #[tokio::test]
    async fn watch_poll_with_no_authorised_accounts_errors_rather_than_returning_an_empty_list() {
        let err = server_with(&[])
            .watch_poll()
            .await
            .expect_err("no accounts must be loud");
        assert!(err.contains("ea-kth-authorize"), "{err}");
        assert_ne!(err, "[]");
    }

    #[tokio::test]
    async fn an_unconfigured_server_says_what_to_create_on_every_tool() {
        let server = KthServer::unconfigured(
            "no KTH app registration at /home/x/.config/exec-agent/kth/app.json",
        );
        for err in [
            server.watch_poll().await.expect_err("watch_poll"),
            server
                .list_mail(Parameters(ListMailArgs {
                    account: "kth".into(),
                    max: None,
                }))
                .await
                .expect_err("list_mail"),
            server
                .get_mail(Parameters(GetMailArgs {
                    account: "kth".into(),
                    id: "m1".into(),
                }))
                .await
                .expect_err("get_mail"),
        ] {
            assert!(err.contains("app.json"), "{err}");
            assert!(err.contains("no usable credentials"), "{err}");
        }
    }

    // -----------------------------------------------------------------------
    // The tool surface, its schemas, and its policy
    // -----------------------------------------------------------------------

    fn registered_tools() -> BTreeSet<String> {
        KthServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn input_schemas() -> BTreeMap<String, serde_json::Value> {
        KthServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    serde_json::Value::Object((*tool.input_schema).clone()),
                )
            })
            .collect()
    }

    fn connector_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/kth")
            .canonicalize()
            .expect("connectors/kth must exist")
    }

    fn policy_path() -> PathBuf {
        connector_dir().join("policy.toml")
    }

    fn policy_rules() -> BTreeMap<String, String> {
        let text = std::fs::read_to_string(policy_path()).unwrap();
        let parsed: BTreeMap<String, BTreeMap<String, toml::Value>> =
            toml::from_str(&text).expect("policy.toml must parse");
        let section = parsed.get(crate::auth::CONNECTOR).unwrap_or_else(|| {
            panic!(
                "policy.toml must have a [{}] section",
                crate::auth::CONNECTOR
            )
        });
        section
            .iter()
            .map(|(tool, value)| {
                let mode = match value {
                    toml::Value::String(mode) => mode.clone(),
                    toml::Value::Table(table) => table
                        .get("mode")
                        .and_then(|m| m.as_str())
                        .expect("a table rule must have a mode")
                        .to_string(),
                    other => panic!("unexpected rule shape for {tool}: {other:?}"),
                };
                (tool.clone(), mode)
            })
            .collect()
    }

    #[test]
    fn the_server_registers_exactly_the_three_planned_tools() {
        let expected: BTreeSet<String> = ["list_mail", "get_mail", "watch_poll"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            registered_tools(),
            expected,
            "a new KTH tool must be a deliberate act, with a policy rule to match"
        );
    }

    /// Direction one: nothing the server offers is unpoliced. A tool with no
    /// rule falls through to the gate's `approve` default, which is not the
    /// same as having been thought about.
    #[test]
    fn every_registered_tool_has_a_policy_rule() {
        let rules = policy_rules();
        for tool in registered_tools() {
            assert!(
                rules.contains_key(&tool),
                "tool {tool:?} has no rule in {}; add one",
                policy_path().display()
            );
        }
    }

    /// Direction two, and the one that catches the subtle failure: a rule left
    /// behind by a rename still parses, still looks deliberate, and applies to
    /// nothing at all — while the tool it used to govern now falls through to
    /// `approve`.
    #[test]
    fn every_policy_rule_names_a_registered_tool_or_a_deliberate_placeholder() {
        let tools = registered_tools();
        for (rule, mode) in policy_rules() {
            if tools.contains(&rule) {
                continue;
            }
            assert!(
                DELIBERATE_PLACEHOLDERS.contains(&rule.as_str()),
                "policy rule {rule:?} names no registered tool. If it is a rename \
                 leftover, delete it — it governs nothing while the renamed tool falls \
                 through to the approve default. If it is a door deliberately held shut, \
                 add it to DELIBERATE_PLACEHOLDERS."
            );
            assert_eq!(
                mode, "deny",
                "placeholder rule {rule:?} exists to forbid a tool that does not exist \
                 yet; anything but deny would pre-authorise it"
            );
        }
    }

    /// Every read is `auto`: this connector cannot change anything, so there
    /// is nothing for a human to approve.
    #[test]
    fn every_tool_this_connector_implements_is_a_read_and_is_auto() {
        let rules = policy_rules();
        for tool in registered_tools() {
            assert_eq!(
                rules.get(&tool).map(String::as_str),
                Some("auto"),
                "{tool} is read-only; anything but auto would put a human tap in front \
                 of a request that changes nothing"
            );
        }
    }

    /// The whole multi-account design in one assertion. A tool that defaults
    /// its account quietly reads the wrong mailbox, and nothing downstream
    /// would notice.
    #[test]
    fn every_account_scoped_tool_requires_an_account_with_no_default() {
        for (name, schema) in input_schemas() {
            if ACCOUNT_EXEMPT_TOOLS.contains(&name.as_str()) {
                continue;
            }

            let required: Vec<&str> = schema["required"]
                .as_array()
                .unwrap_or_else(|| panic!("tool {name:?} has no `required` list: {schema:#}"))
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
            assert!(
                required.contains(&"account"),
                "tool {name:?} does not require an `account`: {schema:#}"
            );

            let account = &schema["properties"]["account"];
            assert_eq!(
                account["type"], "string",
                "tool {name:?}'s account must be a plain string: {schema:#}"
            );
            assert!(
                account.get("default").is_none(),
                "tool {name:?} gives `account` a default. There is no default KTH \
                 account: a default silently reads the wrong mailbox. {schema:#}"
            );
        }
    }

    /// The exemption list itself, pinned to its one member.
    ///
    /// Without this, the loop above skips whatever `ACCOUNT_EXEMPT_TOOLS`
    /// happens to contain, so adding `"get_mail"` to it would quietly buy that
    /// tool an exemption from the whole multi-account rule and nothing in this
    /// crate would fail. Widening the list must mean deliberately editing this
    /// test — and the only reason that has ever been good enough is the one
    /// `watch_poll` has: the daemon calls it with `{}` on a timer.
    #[test]
    fn only_watch_poll_is_exempt_from_the_required_account() {
        assert_eq!(
            ACCOUNT_EXEMPT_TOOLS,
            ["watch_poll"],
            "a second exempt tool means a tool that can read the wrong mailbox with \
             nothing downstream able to tell. See the module docs."
        );
    }

    /// The exemption, pinned rather than assumed: `ea_daemon::jobs` calls
    /// `watch_poll` with `{}`, so it must require nothing at all.
    #[test]
    fn watch_poll_requires_no_arguments_because_the_daemon_calls_it_with_an_empty_object() {
        let schema = &input_schemas()["watch_poll"];
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|r| r.len())
            .unwrap_or(0);
        assert_eq!(
            required, 0,
            "the daemon polls with {{}}; a required argument here breaks every poll: {schema:#}"
        );
    }

    /// Three locks on the same door, and this test checks two of them. The
    /// third is [`crate::auth::SCOPES`], which Microsoft itself enforces.
    #[test]
    fn writing_and_sending_are_denied_before_any_such_tool_exists() {
        let rules = policy_rules();
        for forbidden in DELIBERATE_PLACEHOLDERS {
            assert_eq!(
                rules.get(*forbidden).map(String::as_str),
                Some("deny"),
                "kth.{forbidden} must be denied in policy.toml"
            );
            assert!(
                !registered_tools().contains(*forbidden),
                "this connector is read-only; it must not implement {forbidden}"
            );
        }
        for scope in crate::auth::SCOPES {
            assert!(!scope.contains("Mail.Send") && !scope.contains("ReadWrite"));
        }
    }

    /// Phase 1's loader refuses a connector whose declared name is not its
    /// directory's basename, and refuses a policy file declaring a foreign
    /// section. The three names have to be one name.
    #[test]
    fn the_connector_directory_its_name_its_command_and_its_policy_section_all_agree() {
        let dir = connector_dir();
        let manifest: toml::Value =
            toml::from_str(&std::fs::read_to_string(dir.join("connector.toml")).unwrap()).unwrap();

        let basename = dir.file_name().and_then(|n| n.to_str()).unwrap();
        assert_eq!(basename, "kth");
        assert_eq!(manifest["name"].as_str(), Some(basename));
        assert_eq!(manifest["name"].as_str(), Some(crate::auth::CONNECTOR));
        assert_eq!(manifest["command"].as_str(), Some("ea-kth"));

        // The policy section, read as a raw table so this checks the name
        // rather than trusting `policy_rules`' own lookup.
        let policy: BTreeMap<String, toml::Value> =
            toml::from_str(&std::fs::read_to_string(policy_path()).unwrap()).unwrap();
        assert_eq!(
            policy.keys().collect::<Vec<_>>(),
            vec!["kth"],
            "a policy file that could name another connector would be a \
             privilege-escalation path; Policy::load_dirs refuses one"
        );

        assert!(
            manifest["watch_interval_secs"].as_integer().unwrap_or(0) > 0,
            "the daemon needs a poll cadence"
        );
    }
}
