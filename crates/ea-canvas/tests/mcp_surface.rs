//! End-to-end against the real `ea-canvas` binary over a real stdio MCP
//! session.
//!
//! The unit tests pin the router and the client. These pin what the daemon's
//! connector registry actually meets: a process that completes the handshake,
//! advertises four read-only tools, and — crucially — does all of that with **no
//! credentials configured**, reporting the missing file as a tool error rather
//! than dying at start-up. A connector that exits before the handshake tells
//! the operator only "handshake failed".

use std::borrow::Cow;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};

/// Spawn with `$EA_CONFIG_DIR` pointed at an empty directory, so
/// `credentials.json` genuinely does not exist and the test can never reach
/// the real Canvas.
async fn spawn(config_dir: &std::path::Path) -> RunningService<RoleClient, ()> {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-canvas"));
    command.env("EA_CONFIG_DIR", config_dir);
    let transport = TokioChildProcess::new(command).expect("spawning ea-canvas");
    tokio::time::timeout(Duration::from_secs(20), ().serve(transport))
        .await
        .expect("ea-canvas must complete the MCP handshake")
        .expect("ea-canvas must complete the MCP handshake")
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
async fn the_live_server_advertises_the_four_read_only_tools() {
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
            "list_assignments",
            "list_courses",
            "list_upcoming",
            "watch_poll"
        ],
        "the Canvas connector is read-only; a fifth tool must be deliberate"
    );

    client.cancel().await.unwrap();
}

/// The failure the user will actually hit first: no token yet. It must arrive
/// as a readable tool error naming the file, and the server must survive it —
/// the daemon keeps the client warm and will call again on the next tick.
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
    assert!(text.contains("credentials.json"), "{text}");
    assert!(text.contains("chmod 600"), "{text}");
    assert!(text.contains("New access token"), "{text}");
    assert_ne!(text.trim(), "[]", "silence must not look like success");

    // Still serving: a failed poll costs the daemon nothing.
    let again = client.call_tool(params).await.expect("still serving");
    assert_eq!(again.is_error, Some(true));
    assert_eq!(client.list_all_tools().await.unwrap().len(), 4);

    client.cancel().await.unwrap();
}

/// Bad arguments must not take the process down either.
#[tokio::test]
async fn malformed_arguments_do_not_kill_the_connector() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("list_assignments"));
    params.arguments = Some(
        serde_json::json!({ "course_id": "not a number" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let result = client
        .call_tool(params)
        .await
        .expect("bad arguments must not kill the connector");
    assert_eq!(result.is_error, Some(true));

    assert_eq!(client.list_all_tools().await.unwrap().len(), 4);
    client.cancel().await.unwrap();
}
