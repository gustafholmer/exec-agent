//! `ea` — the control surface for the daemon.
//!
//! Every subcommand is one IPC call and one way of printing the answer. There
//! is deliberately no logic here beyond formatting: the CLI is a client of the
//! socket like any other, and anything it could decide for itself would be a
//! second implementation of a rule that lives in the daemon.
//!
//! `queue` is rendered for a human because it is the thing read most often and
//! under the most time pressure — "what is waiting for me?" — and a wall of
//! JSON answers that badly. Everything else prints the daemon's JSON, which is
//! honest about what the daemon actually said and stays usable from a script.

mod client;

use std::time::Duration;

use anyhow::Context;
use clap::{Parser, Subcommand};
use client::Client;
use serde_json::{json, Value};

/// How long `ea chat` waits, where every other subcommand uses
/// [`ea_core::ipc::DEFAULT_CALL_TIMEOUT`].
///
/// `chat` is the one call that blocks on a `claude -p` session, and the
/// daemon's own runner gives that session 300 seconds
/// (`ea_daemon::session::DEFAULT_TIMEOUT`) before it signals the child. Half a
/// minute past that, so the daemon's bound is always the one that fires and
/// the CLI's is only there for a daemon that has stopped answering at all.
const CHAT_TIMEOUT: Duration = Duration::from_secs(330);

#[derive(Parser)]
#[command(name = "ea", about = "exec-agent CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Is the daemon up, is it paused, and is any job broken?
    Status,
    /// The proposals waiting for a decision.
    Queue,
    /// Approve a proposal and run it.
    Approve {
        /// The action id, as shown by `ea queue`.
        id: i64,
    },
    /// Reject a proposal. Nothing is called.
    Reject {
        id: i64,
        /// Why, recorded on the action.
        #[arg(long)]
        reason: Option<String>,
    },
    /// The most recent runs: sessions and executed actions.
    Log {
        #[arg(short = 'n', default_value_t = 20)]
        n: i64,
    },
    /// Stop the scheduler starting new work. In-flight work finishes.
    Pause,
    /// Undo `pause`, or — given a job name — clear that job's tripped
    /// circuit breaker so it starts polling again.
    ///
    /// A job whose breaker has tripped retries itself after a cooldown, which
    /// `ea status` shows as `retry_in_secs`. Name the job here when you have
    /// just fixed whatever was broken and do not want to wait it out.
    Resume {
        /// The job, as `ea status` names it (a connector, or `triage`).
        job: Option<String>,
    },
    /// Say something to the assistant.
    Chat {
        /// The message. Quoting is optional: everything after `chat` is joined.
        #[arg(required = true, num_args = 1..)]
        message: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = Client::new(ea_core::paths::socket_path());

    match cli.command {
        Command::Status => print_json(&client.call("status", Value::Null).await?)?,
        Command::Queue => {
            let data = client.call("queue", Value::Null).await?;
            print!("{}", render_queue(&data));
        }
        Command::Approve { id } => print_json(&client.call("approve", json!({ "id": id })).await?)?,
        Command::Reject { id, reason } => print_json(
            &client
                .call("reject", json!({ "id": id, "reason": reason }))
                .await?,
        )?,
        Command::Log { n } => print_json(&client.call("log", json!({ "n": n })).await?)?,
        Command::Pause => print_json(&client.call("pause", Value::Null).await?)?,
        Command::Resume { job } => {
            let answer = match job {
                Some(job) => client.call("resume_job", json!({ "job": job })).await?,
                None => client.call("resume", Value::Null).await?,
            };
            print_json(&answer)?
        }
        Command::Chat { message } => {
            let message = message.join(" ");
            let client = Client::with_timeout(ea_core::paths::socket_path(), CHAT_TIMEOUT);
            print_json(&client.call("chat", json!({ "message": message })).await?)?
        }
    }

    Ok(())
}

fn print_json(value: &Value) -> anyhow::Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(value).context("formatting the daemon's answer")?
    );
    Ok(())
}

/// `#12  canvas.list_courses` and the preview indented under it.
///
/// Pure, and separate from the call, so the layout is testable without a
/// daemon. A malformed row prints what it can rather than panicking: this is
/// the command someone runs when things are already going wrong.
fn render_queue(data: &Value) -> String {
    let Some(rows) = data.as_array() else {
        return format!("{data}\n");
    };
    if rows.is_empty() {
        return "Nothing is waiting for you.\n".to_string();
    }

    let mut out = String::new();
    for row in rows {
        let id = row["id"].as_i64().unwrap_or_default();
        let connector = row["connector"].as_str().unwrap_or("?");
        let tool = row["tool"].as_str().unwrap_or("?");
        out.push_str(&format!("#{id}  {connector}.{tool}\n"));

        for line in row["preview"].as_str().unwrap_or("").lines() {
            out.push_str(&format!("    {line}\n"));
        }
        if let Some(expires) = row["expires_at"].as_str() {
            out.push_str(&format!("    (expires {expires})\n"));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "{} waiting. `ea approve <id>` or `ea reject <id> --reason \"...\"`.\n",
        rows.len()
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn action(id: i64, connector: &str, tool: &str, preview: &str) -> Value {
        json!({
            "id": id,
            "connector": connector,
            "tool": tool,
            "preview": preview,
            "expires_at": "2026-09-25T09:00:00+00:00",
            "status": "proposed",
        })
    }

    #[test]
    fn the_queue_names_each_action_and_indents_its_preview() {
        let rendered = render_queue(&json!([action(
            12,
            "canvas",
            "list_courses",
            "List the active courses"
        )]));
        assert!(
            rendered.contains("#12  canvas.list_courses\n"),
            "{rendered}"
        );
        assert!(
            rendered.contains("\n    List the active courses\n"),
            "{rendered}"
        );
        assert!(rendered.contains("1 waiting"), "{rendered}");
    }

    #[test]
    fn a_multi_line_preview_is_indented_throughout() {
        let rendered = render_queue(&json!([action(1, "fortnox", "record_voucher", "a\nb")]));
        assert!(rendered.contains("    a\n    b\n"), "{rendered}");
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_printing_nothing() {
        assert_eq!(render_queue(&json!([])), "Nothing is waiting for you.\n");
    }

    /// `ea queue` is what someone runs when things are already going wrong; it
    /// must not be the thing that panics.
    #[test]
    fn a_malformed_row_still_prints() {
        let rendered = render_queue(&json!([{ "id": "not a number" }]));
        assert!(rendered.contains("#0  ?.?"), "{rendered}");
    }

    #[test]
    fn a_non_array_answer_is_printed_as_is() {
        assert_eq!(render_queue(&json!("nope")), "\"nope\"\n");
    }
}
