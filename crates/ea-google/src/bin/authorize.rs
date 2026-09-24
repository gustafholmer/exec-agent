//! `ea-google-authorize <account>` — the one-shot consent flow.
//!
//! Run once per Google account (`work`, `private`). It prints a consent URL,
//! waits on a loopback listener for Google to redirect back with an
//! authorization code, exchanges that code for tokens, and writes them to
//! `~/.config/exec-agent/google/<account>.json` at mode `0600`.
//!
//! Two behaviours are deliberate and worth knowing before changing them.
//!
//! **`access_type=offline&prompt=consent`, always.** The first is what makes
//! Google willing to issue a refresh token; the second is what makes it issue
//! one *again* for an account that has already consented. Without `prompt`,
//! re-running this command for an account that was authorised before returns
//! an access token and no refresh token, and everything looks fine for an hour.
//!
//! **No refresh token means no write.** If Google returns a response without
//! one, this command fails and changes nothing. Writing an access token with
//! an empty refresh token would succeed here, work for sixty minutes, and then
//! fail inside the daemon with an error about a missing grant — hours later and
//! nowhere near the command that caused it. The only fix is to revoke the app's
//! existing grant at <https://myaccount.google.com/permissions> and run this
//! again, so that is what the message says.
//!
//! # What is never printed
//!
//! The authorization code, the access token, the refresh token, and the client
//! secret. The code arrives in the query string of the loopback request, which
//! is why the raw request line is parsed and discarded rather than logged — a
//! code is a single-use bearer of the entire grant.
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::Utc;
use ea_google::auth::{
    validate_account, AppConfig, HttpRefreshBackend, TokenStore, Tokens, SCOPES,
};
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long to wait for the browser round trip before giving up and freeing
/// the port.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Cap on the loopback request we will read. A GET request line plus headers
/// from any browser is far inside this; the cap is here so a stray client
/// cannot make this process allocate without bound.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

const USAGE: &str = "usage: ea-google-authorize <account>\n\
    \n\
    <account> is a label of your choosing for one Google identity — typically\n\
    \"work\" or \"private\". It must match ^[A-Za-z0-9][A-Za-z0-9_-]*$, because it\n\
    becomes a filename under ~/.config/exec-agent/google/.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` walks the `anyhow` chain on one line. Never `{:?}`: that
            // would print a backtrace, and nothing in the chain is worth a
            // backtrace to the person reading this in a terminal.
            eprintln!("ea-google-authorize: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let account = match (args.next(), args.next()) {
        (Some(account), None) if account != "-h" && account != "--help" => account,
        _ => bail!("{USAGE}"),
    };
    validate_account(&account)?;

    let config = AppConfig::load()?;
    let store = TokenStore::new(None);

    let redirect = Url::parse(&config.redirect_uri)
        .with_context(|| format!("parsing redirectUri {:?}", config.redirect_uri))?;
    let host = redirect
        .host_str()
        .context("the redirectUri has no host; it must be a loopback URL")?;
    let port = redirect.port_or_known_default().unwrap_or(80);
    if !matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        bail!(
            "redirectUri {:?} is not a loopback address. This command listens on the \
             redirect host itself, so it must be 127.0.0.1 or localhost.",
            config.redirect_uri
        );
    }

    // Bind before printing the URL: discovering the port is busy after the
    // user has already consented in a browser wastes a consent round trip, and
    // (with `prompt=consent`) an extra grant.
    let listener = tokio::net::TcpListener::bind((host.trim_matches(['[', ']']), port))
        .await
        .with_context(|| {
            format!(
                "binding {host}:{port} for the OAuth redirect. Another process may be \
                 using it, or a previous run may still be waiting."
            )
        })?;

    let state = uuid::Uuid::new_v4().to_string();
    let url = config.authorization_url(&state)?;

    println!("Authorising the Google account {account:?}.");
    println!();
    println!("Scopes requested:");
    for scope in SCOPES {
        println!("  {scope}");
    }
    println!();
    println!("Open this URL, sign in as the {account:?} account, and approve:");
    println!();
    println!("  {url}");
    println!();
    println!("Waiting for the redirect to {}/ ...", config.redirect_uri);

    let code = tokio::time::timeout(CONSENT_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out after {}s waiting for Google to redirect back. Nothing was \
                 written; run the command again.",
                CONSENT_TIMEOUT.as_secs()
            )
        })??;

    let backend = HttpRefreshBackend::new(&config)?;
    let now = Utc::now();
    let response = backend.exchange_code(&code, &config.redirect_uri).await?;

    // The refusal. See the module docs: an account whose refresh token is
    // missing works for an hour and then fails somewhere else entirely.
    let Some(refresh_token) = response.refresh_token else {
        bail!(
            "Google returned an access token but no refresh token for {account:?}, so \
             nothing was written — an access token alone stops working in about an \
             hour and cannot be renewed.\n\
             \n\
             This happens when this OAuth client already holds a grant for the \
             account: Google issues a refresh token only on the first consent. The fix \
             is to revoke the existing grant and consent again:\n\
             \n  \
             1. Open https://myaccount.google.com/permissions\n  \
             2. Sign in as the {account:?} account and remove this application's access\n  \
             3. Run: ea-google-authorize {account}"
        );
    };

    let granted = response.scope.unwrap_or_else(|| SCOPES.join(" "));
    let tokens = Tokens {
        access_token: response.access_token,
        refresh_token,
        expiry: now + chrono::Duration::seconds(response.expires_in),
        scope: granted.clone(),
    };
    store.write(&account, &tokens)?;

    let path = store.path_for(&account)?;
    println!();
    println!("Wrote {} (mode 0600).", path.display());
    println!("Granted scopes: {granted}");

    // A user can untick a scope on the consent screen; the connector would
    // then fail later with a 403 that says nothing about consent.
    let missing: Vec<&str> = SCOPES
        .iter()
        .copied()
        .filter(|scope| !granted.split_whitespace().any(|g| g == *scope))
        .collect();
    if !missing.is_empty() {
        println!();
        println!("Warning: these requested scopes were NOT granted:");
        for scope in missing {
            println!("  {scope}");
        }
        println!("Re-run this command and approve all of them if a connector needs them.");
    }

    println!();
    println!("Accounts now authorised: {}", store.list()?.join(", "));
    Ok(())
}

/// Accept connections until one is Google's redirect, and return the code.
///
/// A loop rather than a single accept, because a browser opening the loopback
/// URL will also ask for `/favicon.ico`, and because anything at all on the
/// machine may connect to a listening port. Requests that are not the redirect
/// get a 404 and are ignored.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> anyhow::Result<String> {
    loop {
        let (mut socket, _peer) = listener
            .accept()
            .await
            .context("accepting the OAuth redirect")?;

        let mut buffer = Vec::new();
        let mut chunk = [0u8; 2048];
        // Read until the end of the headers. The code is in the request line,
        // so the first line would do, but consuming the headers lets the
        // browser's write finish cleanly before the response is sent.
        loop {
            let n = socket.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() >= MAX_REQUEST_BYTES {
                break;
            }
        }

        // NOTE: `buffer` holds the authorization code. It is parsed and
        // dropped. It is never printed, logged, or put into an error.
        let target = String::from_utf8_lossy(&buffer)
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .map(str::to_string);

        let Some(target) = target else {
            respond(&mut socket, 400, "Bad request.").await;
            continue;
        };

        // A relative request target; any base will do, since only the query
        // matters and the base is discarded.
        let Ok(parsed) = Url::parse("http://127.0.0.1").and_then(|base| base.join(&target)) else {
            respond(&mut socket, 400, "Bad request.").await;
            continue;
        };

        let params: std::collections::HashMap<String, String> =
            parsed.query_pairs().into_owned().collect();

        if let Some(error) = params.get("error") {
            respond(
                &mut socket,
                200,
                "Authorisation was refused. You can close this tab and check the terminal.",
            )
            .await;
            // `error` is an OAuth error code (`access_denied`, …), not a
            // credential.
            bail!(
                "Google reported {error:?} instead of an authorization code. Nothing was written."
            );
        }

        let Some(code) = params.get("code") else {
            // Not the redirect — a favicon request, a probe, a stray curl.
            respond(&mut socket, 404, "Not found.").await;
            continue;
        };

        // The state check: without it, anything that can reach this port
        // during the window could hand the process a code from a different
        // grant.
        match params.get("state") {
            Some(returned) if returned == state => {}
            _ => {
                respond(&mut socket, 400, "State mismatch. Check the terminal.").await;
                bail!(
                    "the redirect carried the wrong `state` value, so it did not come from \
                     the consent request this command started. Nothing was written."
                );
            }
        }

        respond(
            &mut socket,
            200,
            "Authorised. You can close this tab and return to the terminal.",
        )
        .await;
        return Ok(code.clone());
    }
}

/// A minimal HTTP response. Failures are ignored: the browser tab's content is
/// a courtesy, and the code is already in hand by the time it matters.
async fn respond(socket: &mut tokio::net::TcpStream, status: u16, message: &str) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>exec-agent</title>\
         <body style=\"font:16px system-ui;padding:3rem\">{message}</body>"
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.shutdown().await;
}
