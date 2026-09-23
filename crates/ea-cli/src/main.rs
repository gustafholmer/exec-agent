mod client;

use clap::{Parser, Subcommand};
use client::Client;
use serde_json::json;

#[derive(Parser)]
#[command(name = "ea", about = "exec-agent CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Ask the daemon whether it is up.
    Status,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let client = Client::new(ea_core::paths::socket_path());

    match cli.command {
        Command::Status => {
            let data = client.call("status", json!(null)).await?;
            println!("{}", serde_json::to_string_pretty(&data)?);
        }
    }

    Ok(())
}
