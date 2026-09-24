//! End-to-end tests against the real `ea-propose` binary over a real stdio MCP
//! session.
//!
//! The unit tests in `lib.rs` pin the router and the handler. These pin the
//! thing a `claude -p` subprocess actually sees: a process on the other end of
//! a pipe that completes a handshake, advertises exactly its two tools, and
//! answers a call without dying. Nothing short of spawning it proves that.

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
/// tool turning up here has to be somebody's deliberate decision — the list is
/// pinned by name, not by length, so a rename or a swap fails too.
///
/// These are also exactly the two entries in
/// `ea_daemon::session::ToolScope::ProposeAndRemember`, the widest scope any
/// session gets — narrower scopes take tools away, never add them. A tool
/// advertised here and missing from the scope that should reach it is silently
/// denied by the CLI, which looks like a model that never uses it rather than
/// like a bug.
#[tokio::test]
async fn the_live_server_advertises_exactly_the_two_tools() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let tools = client.list_all_tools().await.expect("tools/list");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        vec!["propose_action", "remember"],
        "ea-propose exposes exactly these two tools"
    );
    assert!(
        tools[0]
            .description
            .as_deref()
            .is_some_and(|d| d.contains("policy gate")),
        "the tool description must tell the model what happens to its proposal"
    );
    assert!(
        tools[1]
            .description
            .as_deref()
            .is_some_and(|d| d.contains("nothing in the outside world")),
        "the memory tool must tell the model it is not an action"
    );

    client.cancel().await.unwrap();
}

/// The memory tool over a real pipe, with no daemon behind it: an error result
/// the model can read, and a server still standing afterwards.
#[tokio::test]
async fn remember_with_no_daemon_is_an_error_result_and_the_server_survives() {
    let dir = tempfile::TempDir::new().unwrap();
    let client = spawn(dir.path()).await;

    let mut params = CallToolRequestParams::new(Cow::Borrowed("remember"));
    params.arguments = Some(
        serde_json::json!({ "topic": "tenta", "body": "the databases tenta is on the 14th" })
            .as_object()
            .unwrap()
            .clone(),
    );

    let result = client
        .call_tool(params)
        .await
        .expect("a missing daemon must not be a protocol error");
    assert_eq!(result.is_error, Some(true));
    let text = result
        .content
        .iter()
        .filter_map(|block| block.as_text().map(|t| t.text.as_str()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("Nothing was stored"), "{text}");

    let tools = client.list_all_tools().await.expect("still serving");
    assert_eq!(tools.len(), 2);

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
    assert_eq!(tools.len(), 2);
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
    assert_eq!(tools.len(), 2);

    client.cancel().await.unwrap();
}
