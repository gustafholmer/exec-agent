//! End-to-end against the real `ea-fortnox-mcp` binary over a real stdio MCP
//! session.
//!
//! The unit tests pin the router, the schemas and the tool bodies. These pin
//! what the daemon's connector registry actually meets: a process that
//! completes the handshake, advertises eighteen tools, and — crucially — does
//! all of that with **no credentials configured**, reporting the missing
//! `app.json` as a tool error rather than dying at start-up. A connector that
//! exits before the handshake tells the operator only "handshake failed".
//!
//! `EA_CONFIG_DIR` points at an empty temp directory throughout, so nothing
//! here can reach Fortnox even on a machine that has real credentials.
//!
//! The assertion that carries the most weight is
//! [`no_write_tool_advertises_a_confirm_parameter`]: it reads the schemas off
//! the wire, where a model would see them, rather than out of the router.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};

const EXPECTED_TOOLS: &[&str] = &[
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
];

const WRITE_TOOLS: &[&str] = &[
    "record_voucher",
    "record_expense",
    "reconcile_payment",
    "attach_receipt",
];

async fn spawn(config_dir: &std::path::Path) -> RunningService<RoleClient, ()> {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-fortnox-mcp"));
    command.env("EA_CONFIG_DIR", config_dir);
    let transport = TokioChildProcess::new(command).expect("spawning ea-fortnox-mcp");
    tokio::time::timeout(Duration::from_secs(20), ().serve(transport))
        .await
        .expect("ea-fortnox-mcp must complete the MCP handshake")
        .expect("ea-fortnox-mcp must complete the MCP handshake")
}

fn text_of(result: &rmcp::model::CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn the_live_server_advertises_the_eighteen_planned_tools() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut names: Vec<String> = client
        .list_all_tools()
        .await
        .expect("tools/list")
        .into_iter()
        .map(|tool| tool.name.to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        EXPECTED_TOOLS
            .iter()
            .map(|n| n.to_string())
            .collect::<Vec<_>>(),
        "a nineteenth Fortnox tool must be deliberate, and must come with a policy rule"
    );

    client.cancel().await.unwrap();
}

/// The pin, on the wire. Upstream guards each write with `confirm: true`;
/// here the confirmation is `propose_action` plus a human tap, and a second
/// one inside the tool would be the one that stops being read. A model must
/// not even be *offered* the flag.
#[tokio::test]
async fn no_write_tool_advertises_a_confirm_parameter() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    for tool in client.list_all_tools().await.expect("tools/list") {
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let mut names = BTreeSet::new();
        collect_property_names(&schema, &mut names);
        assert!(
            !names.contains("confirm"),
            "{} advertises a `confirm` parameter: {schema}",
            tool.name
        );
        if WRITE_TOOLS.contains(&tool.name.as_ref()) {
            let description = tool.description.as_deref().unwrap_or_default();
            assert!(
                description.contains("Posts immediately"),
                "{} must say that it posts: {description}",
                tool.name
            );
            assert!(
                description.contains("propose_action"),
                "{} must name the gate: {description}",
                tool.name
            );
        }
    }

    client.cancel().await.unwrap();
}

fn collect_property_names(node: &serde_json::Value, found: &mut BTreeSet<String>) {
    match node {
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                if key == "properties" {
                    if let serde_json::Value::Object(properties) = value {
                        found.extend(properties.keys().cloned());
                    }
                }
                collect_property_names(value, found);
            }
        }
        serde_json::Value::Array(items) => items
            .iter()
            .for_each(|item| collect_property_names(item, found)),
        _ => {}
    }
}

/// The failure the user will actually hit first: no integration registered
/// yet. It must arrive as a readable tool error naming the file, and the
/// server must survive it — the daemon keeps the client warm and will call
/// again on the next tick.
#[tokio::test]
async fn a_read_without_credentials_is_an_error_naming_the_file_and_the_server_survives() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("financial_overview"));
    params.arguments = Some(serde_json::Map::new());
    let result = client
        .call_tool(params.clone())
        .await
        .expect("missing credentials must not be a protocol error");

    assert_eq!(
        result.is_error,
        Some(true),
        "an unconfigured connector must report an error, not an empty answer"
    );
    let text = text_of(&result);
    assert!(text.contains("app.json"), "{text}");
    assert!(text.contains("no usable credentials"), "{text}");
    assert!(text.contains("ea-fortnox-authorize"), "{text}");
    assert_ne!(text.trim(), "[]", "silence must not look like success");

    // Still serving.
    let again = client.call_tool(params).await.expect("still serving");
    assert_eq!(again.is_error, Some(true));
    assert_eq!(
        client.list_all_tools().await.unwrap().len(),
        EXPECTED_TOOLS.len()
    );

    client.cancel().await.unwrap();
}

/// The lapsed-grant failure, on the wire, where the daemon meets it.
///
/// This is the one that must never be `[]`. An empty array is what a quiet
/// week looks like: the breaker would not trip, `ea status` would stay green,
/// and a connector whose refresh token died in October would go silent until
/// somebody happened to ask it a question. The tax deadlines inside
/// `watch_poll` are pure and would have cost nothing to emit here — they are
/// deliberately not emitted, because a poll that half works reads as a poll
/// that works.
#[tokio::test]
async fn watch_poll_without_credentials_is_an_error_and_never_an_empty_array() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let params = CallToolRequestParams::new(Cow::Borrowed("watch_poll"));
    let result = client
        .call_tool(params.clone())
        .await
        .expect("missing credentials must not be a protocol error");

    assert_eq!(
        result.is_error,
        Some(true),
        "an unconfigured connector must report an error, not an empty change list"
    );
    let text = text_of(&result);
    assert!(text.contains("app.json"), "{text}");
    assert!(text.contains("no usable credentials"), "{text}");
    assert_ne!(text.trim(), "[]", "silence must not look like success");

    // Still serving: a failed poll costs the daemon nothing.
    let again = client.call_tool(params).await.expect("still serving");
    assert_eq!(again.is_error, Some(true));
    assert_eq!(
        client.list_all_tools().await.unwrap().len(),
        EXPECTED_TOOLS.len()
    );

    client.cancel().await.unwrap();
}

/// A preview needs no credentials, and this is the property that makes the
/// session's flow work on a connector that has not been authorised yet:
/// the model can still show the person what would be booked.
#[tokio::test]
async fn preview_expense_works_with_no_credentials_at_all() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("preview_expense"));
    params.arguments = Some(
        serde_json::json!({
            "gross_amount": 1250,
            "vat_rate": 25,
            "expense_account": "5410",
            "payment_account": "1930",
            "transaction_date": "2026-05-31",
            "description": "Dator",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let result = client.call_tool(params).await.expect("preview");
    let text = text_of(&result);

    assert_ne!(
        result.is_error,
        Some(true),
        "a preview posts nothing and needs no credentials: {text}"
    );
    assert!(text.starts_with("PREVIEW — nothing posted."), "{text}");
    assert!(!text.to_lowercase().contains("confirm"), "{text}");
    assert!(text.contains("5410"), "{text}");
    assert!(text.contains("2640"), "{text}");
    assert!(text.contains("1930"), "{text}");
    assert!(text.contains("debit 1000"), "{text}");
    assert!(text.contains("debit 250"), "{text}");

    client.cancel().await.unwrap();
}

/// A write on an unconfigured connector must fail at the credentials, having
/// posted nothing — and, since this process has no network path to Fortnox at
/// all, must not hang either.
#[tokio::test]
async fn a_write_without_credentials_is_refused_rather_than_attempted() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("record_expense"));
    params.arguments = Some(
        serde_json::json!({
            "gross_amount": 1250,
            "vat_rate": 25,
            "expense_account": "5410",
            "payment_account": "1930",
            "transaction_date": "2026-05-31",
            "description": "Dator",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let result = client.call_tool(params).await.expect("still serving");
    assert_eq!(result.is_error, Some(true));
    let text = text_of(&result);
    assert!(text.contains("no usable credentials"), "{text}");

    client.cancel().await.unwrap();
}

/// Bad arguments must not take the process down.
#[tokio::test]
async fn malformed_arguments_do_not_kill_the_connector() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("unpaid_invoices"));
    params.arguments = Some(
        serde_json::json!({ "kind": "neither" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let result = client
        .call_tool(params)
        .await
        .expect("bad arguments must not kill the connector");
    assert_eq!(result.is_error, Some(true));

    assert_eq!(
        client.list_all_tools().await.unwrap().len(),
        EXPECTED_TOOLS.len()
    );
    client.cancel().await.unwrap();
}

/// No error path may print a credential.
///
/// This spawns a server that **is** configured — a real `app.json` with a
/// known client secret — but has no stored grant. The first tool call
/// therefore fails in the token manager, which is the deepest failure
/// reachable without a network, and the assertion is that the secret sitting
/// in the process's own memory does not appear in what comes back.
///
/// It cannot reach Fortnox: there are no tokens to present, so the call fails
/// before any HTTP request is built.
#[tokio::test]
async fn a_failing_call_never_quotes_the_client_secret() {
    use std::os::unix::fs::PermissionsExt;

    const SECRET: &str = "SUPER-SECRET-CLIENT-VALUE";

    let dir = tempfile::TempDir::new().unwrap();
    let connector = dir.path().join("fortnox");
    std::fs::create_dir_all(&connector).unwrap();
    let app = connector.join("app.json");
    std::fs::write(
        &app,
        format!(r#"{{"clientId":"public-client-id","clientSecret":"{SECRET}"}}"#),
    )
    .unwrap();
    std::fs::set_permissions(&app, std::fs::Permissions::from_mode(0o600)).unwrap();

    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("vat_report"));
    params.arguments = Some(serde_json::Map::new());
    let result = client.call_tool(params).await.expect("still serving");
    let text = text_of(&result);

    assert_eq!(result.is_error, Some(true), "{text}");
    assert!(
        text.contains("No stored Fortnox tokens"),
        "the failure must be the missing grant, not something else: {text}"
    );
    assert!(text.contains("ea-fortnox-authorize"), "{text}");
    for forbidden in [SECRET, "Bearer ", "refresh_token", "access_token"] {
        assert!(
            !text.contains(forbidden),
            "a tool error must not mention {forbidden}: {text}"
        );
    }

    client.cancel().await.unwrap();
}
