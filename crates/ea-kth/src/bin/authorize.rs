//! `ea-kth-authorize <account>` — the one-shot consent flow against KTH's
//! Entra tenant.
//!
//! Run once. It prints a consent URL, waits on a loopback listener for
//! Microsoft to redirect back with an authorization code, exchanges that code
//! (proving possession of the PKCE verifier) for tokens, and writes them to
//! `~/.config/exec-agent/kth/<account>.json` at mode `0600`.
//!
//! # This command is the experiment
//!
//! Nothing in this crate has ever spoken to Microsoft. The one question the
//! discovery spike could not answer from outside — whether KTH's tenant lets a
//! user consent to a third-party application asking for delegated `Mail.Read`
//! — is answered by running this and reading what the consent page says. Three
//! outcomes, and the messages below name all three:
//!
//! * **It works.** A consent page appears, the owner approves, and a token
//!   file is written.
//! * **`AADSTS65001` / "Need admin approval".** The tenant requires an
//!   administrator to consent on behalf of users. Nothing a student can do
//!   alone; see the README's "If KTH refuses consent".
//! * **`AADSTS90094` / `AADSTS50011` / `AADSTS700016`.** Consent is blocked
//!   outright, the redirect URI does not match the registration, or the
//!   application is not known to the tenant. The first is the tenant's policy;
//!   the other two are the registration's own fault and are fixable.
//!
//! # Deliberate behaviours
//!
//! **PKCE, always.** This is a public client with no secret, so the
//! `code_verifier` is the only thing binding the redirect to this process. It
//! is generated fresh per run, never printed, and sent only in the token POST
//! body.
//!
//! **No refresh token means no write.** If Microsoft returns a response
//! without one, this command fails and changes nothing. Writing an access
//! token with an empty refresh token would succeed here, work for an hour, and
//! then fail inside the daemon hours later, nowhere near the command that
//! caused it. The usual cause is `offline_access` not being granted.
//!
//! # What is never printed
//!
//! The authorization code, the PKCE verifier, the access token, and the
//! refresh token. The code arrives in the query string of the loopback
//! request, which is why the raw request line is parsed and discarded rather
//! than logged — a code is a single-use bearer of the entire grant.
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::Utc;
use ea_kth::auth::{
    code_challenge_s256, new_code_verifier, validate_account, AppConfig, HttpTokenBackend,
    TokenStore, Tokens, SCOPES,
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

const USAGE: &str = "usage: ea-kth-authorize <account>\n\
    \n\
    <account> is a label of your choosing for one KTH mailbox — \"kth\" unless you\n\
    have more than one. It must match ^[a-z0-9][a-z0-9_-]*$ — lower-case only,\n\
    because it becomes a filename under ~/.config/exec-agent/kth/, on a disk where\n\
    \"kth\" and \"KTH\" would be the same file.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` walks the `anyhow` chain on one line. Never `{:?}`: that
            // would print a backtrace, and nothing in the chain is worth one
            // to the person reading this in a terminal.
            eprintln!("ea-kth-authorize: {err:#}");
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
    // owner has already consented in a browser wastes a consent round trip.
    let listener = tokio::net::TcpListener::bind((host.trim_matches(['[', ']']), port))
        .await
        .with_context(|| {
            format!(
                "binding {host}:{port} for the OAuth redirect. Another process may be \
                 using it, or a previous run may still be waiting."
            )
        })?;

    let state = uuid::Uuid::new_v4().to_string();
    // NOTE: the verifier is a credential for the length of this run. It is
    // never printed and never leaves this process except in the token POST.
    let verifier = new_code_verifier();
    let url = config.authorization_url(&state, &code_challenge_s256(&verifier))?;

    println!("Authorising the KTH mailbox {account:?}.");
    println!();
    println!("Tenant:  {}", config.tenant);
    println!("Client:  {}", config.client_id);
    println!("Scopes requested (read-only; nothing here can send or change mail):");
    for scope in SCOPES {
        println!("  {scope}");
    }
    println!();
    println!("Open this URL, sign in with your KTH account, and approve:");
    println!();
    println!("  {url}");
    println!();
    println!("Waiting for the redirect to {} ...", config.redirect_uri);
    println!();
    println!("If the page says \"Need admin approval\" or shows AADSTS65001, KTH's tenant");
    println!("does not allow users to consent to this application. That is the outcome this");
    println!("command exists to find out; see connectors/kth/README.md.");

    let code = tokio::time::timeout(CONSENT_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "timed out after {}s waiting for Microsoft to redirect back. Nothing was \
                 written; run the command again.",
                CONSENT_TIMEOUT.as_secs()
            )
        })??;

    let backend = HttpTokenBackend::new(&config)?;
    let now = Utc::now();
    let response = backend
        .exchange_code(&code, &verifier, &config.redirect_uri, &SCOPES.join(" "))
        .await?;

    // The refusal. See the module docs: an account with no refresh token works
    // for an hour and then fails somewhere else entirely.
    let refresh_token = match response.refresh_token.as_deref().map(str::trim) {
        Some(token) if !token.is_empty() => token.to_string(),
        _ => bail!(
            "Microsoft returned an access token but no refresh token for {account:?}, so \
             nothing was written — an access token alone stops working in about an hour \
             and cannot be renewed.\n\
             \n\
             The usual cause is that `offline_access` was not granted. Check the app \
             registration's API permissions include `offline_access` and \
             `Mail.Read` (delegated), then run: ea-kth-authorize {account}"
        ),
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

    // Entra reports granted scopes by short name (`Mail.Read`), not by the
    // resource URI that was requested, so the check is on the last path
    // segment rather than on the whole string.
    let missing: Vec<&str> = SCOPES
        .iter()
        .copied()
        .filter(|scope| {
            let short = scope.rsplit('/').next().unwrap_or(scope);
            !granted
                .split_whitespace()
                .any(|g| g == *scope || g.eq_ignore_ascii_case(short))
        })
        .collect();
    if !missing.is_empty() {
        println!();
        println!("Note: these requested scopes were not listed in the reply:");
        for scope in missing {
            println!("  {scope}");
        }
        println!(
            "Entra does not always echo `offline_access` back. A refresh token was \
             issued, which is what it buys, so this is usually harmless — but if \
             reads start failing with HTTP 403, re-run this command and approve \
             everything."
        );
    }

    println!();
    println!("Accounts now authorised: {}", store.list()?.join(", "));
    Ok(())
}

/// Accept connections until one is Microsoft's redirect, and return the code.
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
            // These are OAuth error codes and AADSTS descriptions, not
            // credentials, and they are the whole point of running this
            // command — the answer to "does KTH allow this" is in them.
            let description = params
                .get("error_description")
                .map(|d| format!(" Microsoft said: {d}"))
                .unwrap_or_default();
            bail!(
                "Microsoft reported {error:?} instead of an authorization code. Nothing \
                 was written.{description}\n\
                 \n\
                 If that mentions AADSTS65001 or \"admin approval\", KTH's tenant does \
                 not permit a user to consent to this application, and no change to this \
                 code can work around it. Read the \"If KTH refuses consent\" section of \
                 connectors/kth/README.md — forwarding KTH mail to an already-connected \
                 Gmail account is a two-minute fallback that needs no code at all."
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
