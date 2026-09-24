//! End-to-end tests against the real `ea-propose` binary over a real stdio MCP
//! session.
//!
//! The unit tests in `lib.rs` pin the router and the handler. These pin the
//! thing a `claude -p` subprocess actually sees: a process on the other end of
//! a pipe that completes a handshake, advertises one tool, and answers a call
//! without dying. Nothing short of spawning it proves that.

use std::borrow::Cow;
use std::time::Duration;

use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::TokioChildProcess;
use rmcp::{RoleClient, ServiceExt};

/// Spawn the binary with its state directory pointed somewhere empty, so the
/// daemon socket it looks for genuinely does not exist.
async fn spawn(state_dir: &std::path::Path) -> RunningService<RoleClient, ()> {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-propose"));
    command.env("EA_STATE_DIR", state_dir);
    let transport = TokioChildProcess::new(command).expect("spawning ea-propose");
    tokio::time::timeout(Duration::from_secs(20), ().serve(transport))
        .await
        .expect("ea-propose must complete the MCP handshake")
        .expect("ea-propose must complete the MCP handshake")
}

/// The guard rail, at the only level that finally counts: what a session is
/// offered on the wire. This crate is the entire write path of the system, so a
/// second tool turning up here has to be somebody's deliberate decision.
#[tokio::test]
async fn the_live_server_advertises_exactly_one_tool() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    assert_eq!(
        tools.len(),
        1,
        "ea-propose must expose exactly one tool, got {:?}",
        tools.iter().map(|t| t.name.as_ref()).collect::<Vec<_>>()
    );
    assert_eq!(tools[0].name.as_ref(), "propose_action");
    assert!(
        tools[0]
            .description
            .as_deref()
            .is_some_and(|d| d.contains("policy gate")),
        "the tool description must tell the model what happens to its proposal"
    );

    client.cancel().await.unwrap();
}

/// The failure a session will actually hit first: no daemon. It must come back
/// as a tool error result the model can read, and the server must still be
/// alive afterwards.
#[tokio::test]
async fn a_call_with_no_daemon_is_an_error_result_and_the_server_survives() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("propose_action"));
    params.arguments = Some(
        serde_json::json!({
            "connector": "fortnox",
            "tool": "record_voucher",
            "args": { "amount": 1250 },
            "preview": "post a 1250 SEK voucher for the train tickets",
            "rationale": "the receipt arrived by mail this morning",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let result = client
        .call_tool(params.clone())
        .await
        .expect("a missing daemon must not be a protocol error");
    assert_eq!(
        result.is_error,
        Some(true),
        "a missing daemon must be reported as a tool error"
    );
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("not reachable"), "{text}");
    assert!(text.contains("Nothing was proposed"), "{text}");
    assert!(text.contains("daemon.sock"), "{text}");

    // Still up: the whole point of never panicking is that the session keeps
    // its tool after a failure.
    let tools = client.list_all_tools().await.expect("still serving");
    assert_eq!(tools.len(), 1);
    let again = client.call_tool(params).await.expect("still serving");
    assert_eq!(again.is_error, Some(true));

    client.cancel().await.unwrap();
}

/// Arguments that do not match the schema must likewise leave the server
/// standing. `rmcp` 3.4 reports a parameter-deserialisation failure as a tool
/// result with `is_error: true` rather than a JSON-RPC error, so the model gets
/// a readable message and the session keeps its tool either way.
#[tokio::test]
async fn malformed_arguments_do_not_take_the_server_down() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("propose_action"));
    params.arguments = Some(
        serde_json::json!({ "connector": "fortnox" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let result = client
        .call_tool(params)
        .await
        .expect("bad arguments must not kill the session");
    assert_eq!(result.is_error, Some(true));
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("tool"),
        "the message must name the missing field: {text}"
    );

    let tools = client.list_all_tools().await.expect("still serving");
    assert_eq!(tools.len(), 1);

    client.cancel().await.unwrap();
}
