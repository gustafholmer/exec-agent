//! End-to-end against the real `ea-google` binary over a real stdio MCP
//! session.
//!
//! The unit tests pin the router, the clients and the poll. These pin what the
//! daemon's connector registry actually meets: a process that completes the
//! handshake, advertises six tools, and — crucially — does all of that with
//! **no credentials configured**, reporting the missing `app.json` as a tool
//! error rather than dying at start-up. A connector that exits before the
//! handshake tells the operator only "handshake failed".
//!
//! `EA_CONFIG_DIR` points at an empty temp directory throughout, so nothing
//! here can reach Google even if the machine running it has real credentials.

use std::borrow::Cow;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};

async fn spawn(config_dir: &std::path::Path) -> RunningService<RoleClient, ()> {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-google"));
    command.env("EA_CONFIG_DIR", config_dir);
    let transport = TokioChildProcess::new(command).expect("spawning ea-google");
    tokio::time::timeout(Duration::from_secs(20), ().serve(transport))
        .await
        .expect("ea-google must complete the MCP handshake")
        .expect("ea-google must complete the MCP handshake")
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
async fn the_live_server_advertises_the_six_planned_tools() {
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
        vec![
            "create_draft",
            "find_conflicts",
            "get_mail",
            "list_events",
            "list_mail",
            "watch_poll",
        ],
        "a seventh Google tool must be deliberate, and must never be one that sends mail"
    );

    client.cancel().await.unwrap();
}

/// The failure the user will actually hit first: no OAuth client yet. It must
/// arrive as a readable tool error naming the file, and the server must
/// survive it — the daemon keeps the client warm and will call again on the
/// next tick.
#[tokio::test]
async fn watch_poll_without_credentials_is_an_error_naming_the_file_and_the_server_survives() {
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
    assert_eq!(client.list_all_tools().await.unwrap().len(), 6);

    client.cancel().await.unwrap();
}

/// The required `account` is not merely declared in the schema — it is
/// enforced on the wire. A call that omits it must fail, not fall back to
/// some default mailbox.
///
/// This server is deliberately **unconfigured**, so every tool body would
/// fail anyway with "no usable credentials". `is_error == Some(true)` alone
/// therefore proves nothing at all: it is satisfied identically whether the
/// schema rejected the call or the credentials did, and it stays green after
/// `account` is made optional. The assertion that carries the weight is the
/// text — `missing field \`account\`` is rmcp's argument-deserialisation
/// refusal, produced *before* the tool body runs. Mutation-checked: making
/// `account` an `Option<String>` on any of these four tools fails this test,
/// because the call then reaches the body and comes back with the
/// credentials error instead.
#[tokio::test]
async fn a_tool_call_with_no_account_is_refused_by_the_schema_not_by_the_credentials() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    for tool in ["list_events", "list_mail", "get_mail", "create_draft"] {
        let mut params = CallToolRequestParams::new(Cow::Owned(tool.to_string()));
        params.arguments = Some(serde_json::Map::new());
        let result = client
            .call_tool(params)
            .await
            .expect("a missing argument must not kill the connector");
        let text = text_of(&result);
        assert_eq!(
            result.is_error,
            Some(true),
            "{tool} accepted a call with no account: {text}"
        );
        assert!(
            text.contains("missing field `account`"),
            "{tool} must refuse the call for the missing `account`, before it ever \
             looks at credentials. Got: {text}"
        );
        assert!(
            !text.contains("no usable credentials"),
            "{tool} let a call with no account reach the tool body, where it failed \
             for an unrelated reason. `account` is no longer required. Got: {text}"
        );
    }

    assert_eq!(client.list_all_tools().await.unwrap().len(), 6);
    client.cancel().await.unwrap();
}

/// Bad arguments must not take the process down either.
#[tokio::test]
async fn malformed_arguments_do_not_kill_the_connector() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("list_events"));
    params.arguments = Some(
        serde_json::json!({ "account": 7, "days": "a week" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let result = client
        .call_tool(params)
        .await
        .expect("bad arguments must not kill the connector");
    assert_eq!(result.is_error, Some(true));

    assert_eq!(client.list_all_tools().await.unwrap().len(), 6);
    client.cancel().await.unwrap();
}
