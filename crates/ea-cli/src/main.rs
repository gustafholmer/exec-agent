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
    /// Say something to the assistant. With no message, starts an
    /// interactive loop; Ctrl-D (or `exit`) leaves it.
    Chat {
        /// The message. Quoting is optional: everything after `chat` is joined.
        #[arg(num_args = 1..)]
        message: Vec<String>,
    },
    /// What the assistant has been told to remember.
    Facts,
    /// Delete one fact, by the id `ea facts` shows.
    Forget { id: i64 },
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
            let client = Client::with_timeout(ea_core::paths::socket_path(), CHAT_TIMEOUT);
            if message.is_empty() {
                converse(&client).await?;
            } else {
                let message = message.join(" ");
                print_json(&client.call("chat", json!({ "message": message })).await?)?;
            }
        }
        Command::Facts => {
            let data = client.call("facts", Value::Null).await?;
            print!("{}", render_facts(&data));
        }
        Command::Forget { id } => print_json(&client.call("forget", json!({ "id": id })).await?)?,
    }

    Ok(())
}

/// `ea chat` with no argument: one thread, one line at a time.
///
/// Prints the reply rather than the JSON, because this is a conversation and
/// nobody reads a conversation as `{"reply": ...}`. A failed turn prints the
/// error and the loop continues: the daemon stored no assistant message for
/// it, so trying again is the right move and exiting would throw away the
/// thread.
///
/// The prompt is written only when stdin is a terminal, so that
/// `echo "..." | ea chat` stays usable from a script.
async fn converse(client: &Client) -> anyhow::Result<()> {
    use std::io::{BufRead, IsTerminal, Write};

    let interactive = std::io::stdin().is_terminal();
    if interactive {
        println!("Talking to the assistant. Ctrl-D to leave.");
    }

    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        if interactive {
            print!("> ");
            std::io::stdout().flush().ok();
        }
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            return Ok(());
        }
        let message = line.trim();
        if message.is_empty() {
            continue;
        }
        if matches!(message, "exit" | "quit") {
            return Ok(());
        }

        match client.call("chat", json!({ "message": message })).await {
            Ok(answer) => println!("{}", chat_answer(&answer)),
            Err(err) => eprintln!("{err:#}"),
        }
    }
}

/// The line to print for one turn: the reply, or the daemon's note about why
/// there is not one. Pure, so both cases are testable without a daemon.
fn chat_answer(value: &Value) -> String {
    if let Some(reply) = value["reply"].as_str() {
        return reply.to_string();
    }
    match value["note"].as_str() {
        Some(note) => format!("(no reply: {note})"),
        None => format!("(no reply: {value})"),
    }
}

/// `#3  tenta` with the body indented under it.
///
/// Rendered rather than dumped for the same reason `queue` is: this is the
/// command someone runs to find the wrong thing the assistant believes, and a
/// wall of JSON answers that badly.
fn render_facts(data: &Value) -> String {
    let Some(rows) = data.as_array() else {
        return format!("{data}\n");
    };
    if rows.is_empty() {
        return "Nothing remembered yet.\n".to_string();
    }

    let mut out = String::new();
    for row in rows {
        let id = row["id"].as_i64().unwrap_or_default();
        let topic = row["topic"].as_str().unwrap_or("?");
        out.push_str(&format!("#{id}  {topic}\n"));
        for line in row["body"].as_str().unwrap_or("").lines() {
            out.push_str(&format!("    {line}\n"));
        }
        // The date that answers "is this still true?": when it was last
        // corrected, falling back to when it was first learned.
        if let Some(when) = row["updated_at"].as_str().or(row["created_at"].as_str()) {
            out.push_str(&format!("    ({when})\n"));
        }
        out.push('\n');
    }
    out.push_str(&format!(
        "{} remembered. `ea forget <id>` to delete one.\n",
        rows.len()
    ));
    out
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

    fn fact(id: i64, topic: &str, body: &str) -> Value {
        json!({
            "id": id,
            "topic": topic,
            "body": body,
            "created_at": "2026-09-01T09:00:00+00:00",
            "updated_at": null,
        })
    }

    #[test]
    fn facts_are_listed_with_the_id_you_forget_them_by() {
        let rendered = render_facts(&json!([fact(3, "tenta", "on the 14th")]));
        assert!(rendered.contains("#3  tenta\n"), "{rendered}");
        assert!(rendered.contains("\n    on the 14th\n"), "{rendered}");
        assert!(rendered.contains("2026-09-01"), "{rendered}");
        assert!(rendered.contains("ea forget <id>"), "{rendered}");
    }

    #[test]
    fn a_corrected_fact_shows_when_it_was_corrected() {
        let mut row = fact(3, "tenta", "moved to the 21st");
        row["updated_at"] = json!("2026-09-20T18:00:00+00:00");
        let rendered = render_facts(&json!([row]));
        assert!(rendered.contains("2026-09-20"), "{rendered}");
        assert!(!rendered.contains("2026-09-01"), "{rendered}");
    }

    #[test]
    fn an_empty_memory_says_so_rather_than_printing_nothing() {
        assert_eq!(render_facts(&json!([])), "Nothing remembered yet.\n");
    }

    #[test]
    fn a_malformed_fact_row_still_prints() {
        let rendered = render_facts(&json!([{ "id": "not a number" }]));
        assert!(rendered.contains("#0  ?"), "{rendered}");
    }

    #[test]
    fn a_turn_prints_the_reply_and_falls_back_to_the_note() {
        assert_eq!(
            chat_answer(&json!({ "reply": "the tenta is on the 14th", "note": null })),
            "the tenta is on the 14th"
        );
        let no_runner = chat_answer(&json!({
            "reply": null,
            "note": "recorded, but this daemon has no session runner",
        }));
        assert!(no_runner.contains("no session runner"), "{no_runner}");
        // Neither field: still a line, never a panic.
        assert!(chat_answer(&json!({})).contains("no reply"));
    }
}
