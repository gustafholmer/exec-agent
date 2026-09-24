//! The MCP surface: seven read tools, three report tools, three previews,
//! four writes and `watch_poll`.
//!
//! # The shape, and why it differs from upstream's
//!
//! Upstream's `tools/` directory is four files returning arrays of tool
//! modules, each write tool carrying a `confirm` flag that turns a preview
//! into a posting. Here the split is by what a tool *does to the world*, which
//! is the split the policy gate cares about:
//!
//! * [`read`] and [`report`] — GET only.
//! * [`preview`] — renders the voucher a write would post, makes no HTTP call
//!   at all, and works on an unconfigured connector.
//! * [`write`] and [`attach`] — POST, with no `confirm` parameter, because
//!   reaching one means `propose_action` already ran and a human already
//!   tapped approve. See the crate docs.
//! * [`watch`] — the daemon's timer calls it, no session involved. GET only,
//!   and it fails loudly rather than answering `[]`.
//!
//! # Every tool here has a policy rule, and every rule has a tool
//!
//! `connectors/fortnox/policy.toml` is checked against this router in both
//! directions by the tests at the foot of this file. The second direction is
//! the one that earns its place: a rule stranded by a rename still parses and
//! still looks deliberate while governing nothing, and the tool it used to
//! cover quietly falls through to the gate's `approve` default.
//!
//! # Where the numbers come from
//!
//! Nothing in this crate does arithmetic. Every figure is computed by
//! [`ea_fortnox`] — [`ea_fortnox::domain::moms::split_gross`] for a VAT split,
//! [`ea_fortnox::domain::voucher::build_payload`] for a posted body,
//! [`ea_fortnox::reporting`] for every summary — which is where the tests that
//! establish the figures are correct also live. These modules translate
//! arguments in and JSON out.
//!
//! # List endpoints go through `get_all`
//!
//! Fortnox caps a list response at 100 rows by default. A VAT return computed
//! from page one is wrong without looking wrong, so every account, voucher and
//! invoice list here uses [`FortnoxClient::get_all`], never a plain `get`.
//! `financialyears` is the one exception, matching upstream: it is looked up
//! by date and answers with the one year that covers it.

pub mod attach;
pub mod preview;
pub mod read;
pub mod report;
pub mod watch;
pub mod write;

use std::sync::Arc;

use ea_fortnox::domain::money::to_api;
use ea_fortnox::errors::FortnoxError;
use ea_fortnox::reporting::AccountRow;
use ea_fortnox::FortnoxClient;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::{tool_handler, ServerHandler};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

/// The name the daemon knows this connector by, and the `[section]` its
/// `policy.toml` rules live under.
pub use ea_fortnox::auth::CONNECTOR;

/// Tools named in `policy.toml` that this server deliberately does **not**
/// implement.
///
/// Canvas holds `submit_assignment` shut this way and Google holds `send_mail`
/// shut; this connector has no such door, and the list is empty on purpose
/// rather than by omission. The reason is that Fortnox's dangerous operations
/// are not tools this connector declines to write — they are tools it *does*
/// implement, because bookkeeping is the job, and they are governed by
/// `approve` instead. A `deny` here would be a tool nobody can reach even with
/// a human tap, and there is no operation in this connector's remit that
/// deserves that.
///
/// If one ever does — deleting a voucher, say, or closing a financial year —
/// adding the name here *and* as a `deny` rule puts the refusal in force
/// before the tool exists, because the gate matches a rule by name.
pub const DELIBERATE_PLACEHOLDERS: &[&str] = &[];

/// The tools that change something in Fortnox. None of these may ever be
/// `auto`; see `nothing_that_posts_is_auto`.
pub const POSTING_TOOLS: &[&str] = &[
    "record_voucher",
    "record_expense",
    "reconcile_payment",
    "attach_receipt",
];

/// Either a working client, or the reason there isn't one.
///
/// A connector with no `app.json` still starts and still completes the MCP
/// handshake; it just fails every call that needs Fortnox with a message
/// naming the file to create. Exiting at start-up instead would reach the
/// daemon as "handshake failed", which tells nobody what to do.
///
/// The [`preview`] tools deliberately do not consult this at all: rendering
/// what *would* be booked is pure arithmetic, and it is more useful for a
/// half-configured connector to still be able to show a person what a voucher
/// would look like than for it to refuse uniformly.
enum Backend {
    Ready(FortnoxClient),
    Unconfigured(String),
}

#[derive(Clone)]
pub struct FortnoxServer {
    backend: Arc<Backend>,
    tool_router: ToolRouter<Self>,
}

impl FortnoxServer {
    pub fn new(client: FortnoxClient) -> Self {
        Self {
            backend: Arc::new(Backend::Ready(client)),
            tool_router: Self::router(),
        }
    }

    /// A server that will answer every Fortnox-touching call with `reason`.
    /// The previews still work.
    pub fn unconfigured(reason: impl Into<String>) -> Self {
        Self {
            backend: Arc::new(Backend::Unconfigured(reason.into())),
            tool_router: Self::router(),
        }
    }

    /// Every tool this connector serves, in one router.
    ///
    /// Public so a test can enumerate the surface and its schemas without
    /// standing up a server — which is how
    /// [`write::tests`] pins the absence of `confirm`.
    pub fn router() -> ToolRouter<Self> {
        Self::read_router()
            + Self::report_router()
            + Self::preview_router()
            + Self::write_router()
            + Self::attach_router()
            + Self::watch_router()
    }

    pub(crate) fn client(&self) -> Result<&FortnoxClient, String> {
        match &*self.backend {
            Backend::Ready(client) => Ok(client),
            Backend::Unconfigured(reason) => Err(format!(
                "fortnox: this connector has no usable credentials, so it cannot reach \
                 Fortnox. {reason}"
            )),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for FortnoxServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Swedish bookkeeping in Fortnox for the owner's company. The read and report \
             tools are free to call. The three preview_* tools render exactly what a write \
             would book and post nothing — call the preview first and show the person its \
             text. The write tools (record_voucher, record_expense, reconcile_payment, \
             attach_receipt) post to Fortnox the moment they are called; they are reachable \
             only through propose_action, and the approval gate — not this server — decides \
             whether one runs. There is deliberately no `confirm` flag: the human tap is the \
             confirmation. Every computed VAT or result figure is best-effort over the BAS \
             chart and must be checked against the official momsdeklaration before filing.",
        )
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// A [`FortnoxError`] as the string the model reads.
///
/// `Display` on `FortnoxError` is already written for this: it never carries a
/// token, a URL with credentials in it, or a response body beyond a bounded
/// snippet. See `ea_fortnox::errors`.
pub(crate) fn render(err: FortnoxError) -> String {
    format!("fortnox: {err}")
}

/// An `anyhow` error — everything from `domain` — as the string the model
/// reads. `{:#}` keeps the context chain, which is where "invalid expense
/// account" lives.
pub(crate) fn render_domain(err: anyhow::Error) -> String {
    format!("fortnox: {err:#}")
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string(value)
        .map_err(|err| format!("fortnox: could not serialise the reply: {err}"))
}

/// Today, UTC, as `YYYY-MM-DD`.
///
/// Upstream's `new Date().toISOString().slice(0, 10)`, which is UTC too. It is
/// only ever used to ask Fortnox *which financial year covers this date*, and
/// a financial year is months long, so the one day a year on which a Swedish
/// local date and a UTC date disagree cannot pick the wrong year except at a
/// year boundary — where the caller should be passing an explicit date anyway.
pub(crate) fn today_utc() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// The id of the financial year covering `date` (default: today).
///
/// [`None`] when Fortnox knows no year for that date — upstream's
/// `years[0]?.Id` being `undefined`, which it then spreads into a query as an
/// absent parameter. Reproduced: an absent `financialyear` makes Fortnox
/// answer for its own default year, which is what upstream has been doing.
pub(crate) async fn financial_year_id(
    client: &FortnoxClient,
    date: Option<&str>,
) -> Result<Option<i64>, String> {
    let owned;
    let date = match date {
        Some(date) => date,
        None => {
            owned = today_utc();
            &owned
        }
    };
    let response = client
        .get("financialyears", &[("date", date)])
        .await
        .map_err(render)?;
    Ok(response
        .get("FinancialYears")
        .and_then(Value::as_array)
        .and_then(|years| years.first())
        .and_then(|year| year.get("Id"))
        .and_then(coerce_i64))
}

/// Fortnox is inconsistent about whether an id is a JSON number or a string.
fn coerce_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// One account from a Fortnox `Accounts` row, as [`ea_fortnox::reporting`]
/// wants it.
///
/// `Number` arrives as a string or a number depending on the endpoint;
/// `Balance` likewise. A row with neither is skipped rather than counted as
/// zero, because a zero would silently join every sum.
pub(crate) fn account_rows(rows: &[Value]) -> Vec<AccountRow> {
    rows.iter()
        .filter_map(|row| {
            let number = coerce_string(row.get("Number")?)?;
            Some(AccountRow {
                number,
                balance: row
                    .get("Balance")
                    .map(coerce_amount)
                    .unwrap_or(Decimal::ZERO),
            })
        })
        .collect()
}

/// `{ account, description, balance }` — upstream's projection of an account
/// row, which is what every report returns instead of Fortnox's full row.
pub(crate) fn account_view(row: &Value) -> Value {
    serde_json::json!({
        "account": row.get("Number").cloned().unwrap_or(Value::Null),
        "description": row.get("Description").cloned().unwrap_or(Value::Null),
        "balance": row.get("Balance").cloned().unwrap_or(Value::Null),
    })
}

/// The account number of a Fortnox row, for the class predicates. `None` when
/// the row has no usable `Number`, which makes it fall out of every filter —
/// the same fate upstream's `String(undefined)` gives it.
pub(crate) fn account_number(row: &Value) -> Option<String> {
    row.get("Number").and_then(coerce_string)
}

fn coerce_string(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A JSON amount as a [`Decimal`].
///
/// Through the shortest round-tripping decimal string rather than
/// `Decimal::try_from(f64)`, the same idiom as `ea_fortnox`'s workbook reader
/// and for the same reason: a balance of `-1234.56` arrives as the double
/// nearest that decimal, and `format!("{f}")` is exactly `"-1234.56"`.
/// Anything unparseable is zero, matching upstream's `Number(x) || 0`.
pub(crate) fn coerce_amount(value: &Value) -> Decimal {
    match value {
        Value::Number(n) => n.to_string().parse().unwrap_or(Decimal::ZERO),
        Value::String(s) => s.trim().parse().unwrap_or(Decimal::ZERO),
        _ => Decimal::ZERO,
    }
}

/// A computed [`Decimal`] as a JSON **number**.
///
/// `rust_decimal`'s own `Serialize` emits a string (`"26800"`), which upstream
/// does not: `JSON.stringify` of a JavaScript number is a number, and a model
/// comparing `net_vat_to_pay` against a threshold should not have to parse a
/// string first. The conversion is
/// [`ea_fortnox::domain::money::to_api`] — round to öre, then to `f64` — the
/// same one every posted amount goes through, so a figure reported here and
/// the same figure posted to Fortnox are the same number. Rounding to öre is
/// a no-op on a sum of öre-granular balances, which is every figure here.
pub(crate) fn amount_value(amount: Decimal) -> Value {
    serde_json::Number::from_f64(to_api(amount))
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// An amount a *caller* supplied, in kronor.
///
/// Same conversion as [`coerce_amount`], but an unusable value is an error
/// rather than zero: silently booking 0.00 kr because a model sent `"1 250"`
/// would be a voucher nobody meant.
pub(crate) fn kronor(field: &str, amount: f64) -> Result<Decimal, String> {
    if !amount.is_finite() {
        return Err(format!(
            "fortnox: {field} must be a finite amount in kronor, not {amount}"
        ));
    }
    format!("{amount}")
        .parse()
        .map_err(|_| format!("fortnox: {field} is not an amount in kronor: {amount}"))
}

#[cfg(test)]
pub(crate) mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::{BTreeMap, BTreeSet};

    fn registered_tools() -> BTreeSet<String> {
        FortnoxServer::router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect()
    }

    fn policy_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../connectors/fortnox")
            .canonicalize()
            .expect("connectors/fortnox must exist")
            .join("policy.toml")
    }

    /// Every rule in the `[fortnox]` section, as `tool -> mode`. Both rule
    /// shapes `ea_core::policy` accepts are flattened here: `"approve"` and
    /// `{ mode = "approve", note = "..." }`.
    fn policy_rules() -> BTreeMap<String, String> {
        let text = std::fs::read_to_string(policy_path()).expect("reading policy.toml");
        let parsed: BTreeMap<String, BTreeMap<String, toml::Value>> =
            toml::from_str(&text).expect("policy.toml must parse");
        let section = parsed
            .get(CONNECTOR)
            .unwrap_or_else(|| panic!("policy.toml must have a [{CONNECTOR}] section"));
        section
            .iter()
            .map(|(tool, value)| {
                let mode = match value {
                    toml::Value::String(mode) => mode.clone(),
                    toml::Value::Table(table) => table
                        .get("mode")
                        .and_then(|mode| mode.as_str())
                        .expect("a table rule must have a mode")
                        .to_string(),
                    other => panic!("unexpected rule shape for {tool}: {other:?}"),
                };
                (tool.clone(), mode)
            })
            .collect()
    }

    /// The catalogue, pinned. Upstream's `index.test.ts` asserts the same
    /// thing over its fourteen tools; this adds the three previews and
    /// `watch_poll`, and drops nothing.
    #[test]
    fn the_server_registers_exactly_the_eighteen_planned_tools() {
        let expected: BTreeSet<String> = [
            "account_ledger",
            "attach_receipt",
            "balance_sheet",
            "financial_overview",
            "period_report",
            "preview_expense",
            "preview_reconciliation",
            "preview_voucher",
            "profit_and_loss",
            "query_fortnox",
            "reconcile_payment",
            "record_expense",
            "record_voucher",
            "result_summary",
            "unpaid_invoices",
            "vat_report",
            "vat_summary",
            "watch_poll",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(
            registered_tools(),
            expected,
            "a nineteenth Fortnox tool must be a deliberate act, with a policy rule to match"
        );
    }

    /// The section name is the connector's own, which is also this
    /// directory's basename. `Policy::load_dirs` rejects anything else as a
    /// privilege-escalation path, so a typo here is a startup failure rather
    /// than a silently ignored file.
    #[test]
    fn the_policy_file_declares_only_this_connectors_own_section() {
        let text = std::fs::read_to_string(policy_path()).expect("reading policy.toml");
        let parsed: BTreeMap<String, toml::Value> =
            toml::from_str(&text).expect("policy.toml must parse");
        let sections: Vec<&str> = parsed.keys().map(String::as_str).collect();
        assert_eq!(
            sections,
            [CONNECTOR],
            "a connector may only police its own tools; Policy::load_dirs fails startup \
             on a foreign section"
        );
        assert_eq!(
            CONNECTOR, "fortnox",
            "the section, the connector name and the directory basename are one name"
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
    /// `approve`, which looks fine from every angle except the one that
    /// matters.
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

    /// Nothing that changes the owner's books may run without a human tap.
    ///
    /// Deliberately no amount threshold: an incorrect voucher is tedious to
    /// unwind and visible to the accountant whatever the number on it says,
    /// and a threshold is a dial that only ever gets turned down. See the
    /// header of `connectors/fortnox/policy.toml`.
    #[test]
    fn nothing_that_posts_is_auto() {
        let rules = policy_rules();
        for tool in POSTING_TOOLS {
            let mode = rules
                .get(*tool)
                .unwrap_or_else(|| panic!("{tool} must have a policy rule"));
            assert_eq!(
                mode, "approve",
                "{tool} posts to Fortnox; it must wait for a human tap"
            );
        }
        // And the list is the truth about the router, not a stale copy of it:
        // every posting tool named above is actually registered.
        let registered = registered_tools();
        for tool in POSTING_TOOLS {
            assert!(registered.contains(*tool), "{tool} is not registered");
        }
    }

    /// Three writes carry the note the gate shows a person; `attach_receipt`
    /// is a plain `"approve"`. All four are `approve`, which is what the test
    /// above pins — this one pins that the note says the thing that makes the
    /// policy unusual.
    #[test]
    fn the_three_booking_writes_say_the_amount_does_not_matter() {
        let text = std::fs::read_to_string(policy_path()).expect("reading policy.toml");
        let parsed: BTreeMap<String, BTreeMap<String, toml::Value>> =
            toml::from_str(&text).expect("policy.toml must parse");
        let section = &parsed[CONNECTOR];
        for tool in ["record_voucher", "record_expense", "reconcile_payment"] {
            let note = section[tool]
                .get("note")
                .and_then(|note| note.as_str())
                .unwrap_or_else(|| panic!("{tool} must carry a note"));
            assert_eq!(note, "always, regardless of amount", "{tool}");
        }
    }

    /// The previews post nothing and make no HTTP call at all. If one of them
    /// needed approval it would be the write with extra steps, and the model
    /// would stop showing people what is about to be booked.
    #[test]
    fn every_preview_tool_is_auto() {
        let rules = policy_rules();
        let previews: Vec<String> = registered_tools()
            .into_iter()
            .filter(|tool| tool.starts_with("preview_"))
            .collect();
        assert_eq!(previews.len(), 3, "{previews:?}");
        for tool in previews {
            assert_eq!(
                rules.get(&tool).map(String::as_str),
                Some("auto"),
                "{tool} renders a voucher and posts nothing; approving it buys nothing"
            );
        }
    }

    /// The daemon polls on a timer with no session and nobody to ask. A
    /// `watch_poll` that is not `auto` is a connector that never polls.
    #[test]
    fn watch_poll_is_auto_or_the_connector_never_polls() {
        assert_eq!(
            policy_rules().get("watch_poll").map(String::as_str),
            Some("auto")
        );
    }

    /// `ea_daemon::jobs` calls `watch_poll` with `{}`, so it must require
    /// nothing at all.
    #[test]
    fn watch_poll_requires_no_arguments_because_the_daemon_calls_it_with_an_empty_object() {
        let schema = FortnoxServer::router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "watch_poll")
            .map(|tool| serde_json::Value::Object((*tool.input_schema).clone()))
            .expect("watch_poll must be registered");
        let required = schema
            .get("required")
            .and_then(|required| required.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        assert_eq!(
            required, 0,
            "the daemon polls with {{}}; a required argument here breaks every poll: {schema:#}"
        );
    }

    /// The manifest and the policy file are one connector, and the daemon
    /// checks that at startup. Checking it here means the failure is a red
    /// test rather than a daemon that will not boot.
    #[test]
    fn the_manifest_names_this_connector_and_its_binary() {
        let dir = policy_path().parent().expect("a directory").to_path_buf();
        let text = std::fs::read_to_string(dir.join("connector.toml")).expect("connector.toml");
        let parsed: toml::Value = text.parse().expect("connector.toml must be TOML");
        assert_eq!(parsed["name"].as_str(), Some(CONNECTOR));
        assert_eq!(
            dir.file_name().and_then(|name| name.to_str()),
            Some(CONNECTOR),
            "a connector's name must equal its directory's basename"
        );
        assert_eq!(parsed["command"].as_str(), Some("ea-fortnox-mcp"));
        assert_eq!(parsed["watch_interval_secs"].as_integer(), Some(86_400));
    }

    /// Every tool must say what it does; the description is the only thing a
    /// model has to go on when choosing between `record_expense` and
    /// `preview_expense`.
    #[test]
    fn every_tool_has_a_description() {
        for tool in FortnoxServer::router().list_all() {
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.len() > 40,
                "{} has no useful description: {description:?}",
                tool.name
            );
        }
    }

    #[test]
    fn an_unconfigured_server_names_the_reason_but_still_previews() {
        let server = FortnoxServer::unconfigured("no Fortnox integration at /tmp/app.json");
        let err = server.client().expect_err("no client");
        assert!(err.contains("app.json"), "{err}");
        assert!(err.contains("no usable credentials"), "{err}");
    }

    #[test]
    fn amounts_convert_through_their_shortest_decimal_string() {
        assert_eq!(kronor("x", 1250.0).unwrap(), Decimal::new(1250, 0));
        assert_eq!(kronor("x", -1234.56).unwrap(), Decimal::new(-123456, 2));
        assert!(kronor("grossAmount", f64::NAN).is_err());
        assert!(kronor("grossAmount", f64::INFINITY).is_err());
    }

    #[test]
    fn account_rows_take_a_number_or_a_string_for_either_field() {
        let rows = vec![
            serde_json::json!({ "Number": "1930", "Balance": 80000.0 }),
            serde_json::json!({ "Number": 2440, "Balance": "-50.25" }),
            serde_json::json!({ "Balance": 1.0 }),
        ];
        let parsed = account_rows(&rows);
        assert_eq!(parsed.len(), 2, "a row with no Number is skipped");
        assert_eq!(parsed[0].number, "1930");
        assert_eq!(parsed[1].number, "2440");
        assert_eq!(parsed[1].balance, Decimal::new(-5025, 2));
    }
}
