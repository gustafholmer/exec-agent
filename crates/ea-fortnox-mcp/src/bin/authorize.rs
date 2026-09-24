//! `ea-fortnox-authorize` — the one-shot Fortnox consent flow.
//!
//! Run once, by a person, at a browser. It prints a consent URL, waits on a
//! loopback listener for Fortnox to redirect back with an authorization code,
//! exchanges that code for the first token pair, and writes it to
//! `~/.config/exec-agent/fortnox/tokens.json` at mode `0600`.
//!
//! A port of `bin/authorize.ts` from the TypeScript integration this replaces,
//! built to the same shape as `ea-google-authorize`, with four differences
//! from the TypeScript worth knowing before changing anything here.
//!
//! **1. The redirect URI must match the Developer Portal entry byte for byte.**
//! Fortnox compares the `redirect_uri` on the consent request and on the code
//! exchange against the one registered for the integration as strings —
//! `http://localhost:8910/callback` and `http://127.0.0.1:8910/callback` are
//! two different URIs, and so are the same string with and without a trailing
//! slash. The failure arrives as a flat `400` at the token endpoint with
//! nothing in it about URIs, which is why the exchange error in this file ends
//! with the URI it used and the sentence naming this as the usual cause.
//!
//! **2. Both loopback families are bound.** `localhost` resolves to `::1` and
//! to `127.0.0.1`, in an order that is the resolver's business and not ours; a
//! listener on one of them and a browser on the other is a connection refused
//! after the human has already consented. So every address the redirect host
//! resolves to gets a listener, and the first one to catch the code wins.
//!
//! **3. No refresh token means no write.** Fortnox always sends one, so this
//! is a guard against a surprise rather than a routine path — but writing an
//! access token with an empty refresh token would work for an hour and then
//! fail inside the daemon, hours later and nowhere near this command.
//!
//! **4. The state parameter is checked.** Upstream checks it too; what is added
//! here is that a mismatch is a hard failure that writes nothing, and that
//! requests which are not the redirect at all (a browser's `/favicon.ico`, a
//! stray probe) get a 404 and are ignored rather than ending the wait.
//!
//! # What is never printed
//!
//! The authorization code, the access token, the refresh token, and the client
//! secret. The code arrives in the query string of the loopback request, so
//! the raw request line is parsed and dropped rather than logged — a code is a
//! single-use bearer of the entire grant. The client *id* is public by design
//! and appears in the consent URL, which is the one thing here that must be
//! copied into a browser.
#![forbid(unsafe_code)]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use chrono::Utc;
use ea_fortnox::auth::{
    build_authorize_url, AuthorizeParams, FileTokenStore, OAuthClient, StoredTokens, TokenStore,
    AUTHORIZE_COMMAND,
};
use ea_fortnox_mcp::config::{AppConfig, SCOPES};
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// How long to wait for the browser round trip before giving up and freeing
/// the port. Approving for a company in Fortnox means signing in and picking
/// the right one, so this is deliberately generous.
const CONSENT_TIMEOUT: Duration = Duration::from_secs(300);

/// Cap on the loopback request we will read. A GET request line plus headers
/// from any browser is far inside this; the cap is here so a stray client
/// cannot make this process allocate without bound.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// The company the owner must pick on the consent screen. Fortnox asks *which
/// company* the integration may act for, and a person with access to more than
/// one can approve for the wrong one without any later error saying so — the
/// tokens work, they just read somebody else's books.
const COMPANY: &str = "Devs Alike AB";

const USAGE: &str = "usage: ea-fortnox-authorize\n\
    \n\
    Takes no arguments. Reads the client id and secret from\n\
    ~/.config/exec-agent/fortnox/app.json (or $EA_CONFIG_DIR/fortnox/app.json)\n\
    and writes tokens.json beside it.";

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // `{:#}` walks the `anyhow` chain on one line. Never `{:?}`: that
            // would print a backtrace, and nothing in the chain is worth a
            // backtrace to the person reading this in a terminal.
            eprintln!("{AUTHORIZE_COMMAND}: {err:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> anyhow::Result<()> {
    if std::env::args().nth(1).is_some() {
        bail!("{USAGE}");
    }

    let config = AppConfig::load()?;
    let store = FileTokenStore::at_default_path();

    let redirect = Url::parse(&config.redirect_uri)
        .with_context(|| format!("parsing redirectUri {:?}", config.redirect_uri))?;
    let host = redirect
        .host_str()
        .context("the redirectUri has no host; it must be a loopback URL")?
        .to_string();
    let port = redirect.port_or_known_default().unwrap_or(80);
    if !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]" | "::1") {
        bail!(
            "redirectUri {:?} is not a loopback address. This command listens on the \
             redirect host itself, so it must be 127.0.0.1 or localhost.",
            config.redirect_uri
        );
    }
    // Fortnox compares the whole URI, path included, so the listener answers
    // on that path and 404s everything else.
    let redirect_path = redirect.path().to_string();

    // Bind before printing the URL: discovering the port is busy after the
    // person has already consented in a browser wastes a consent round trip.
    let listeners = bind_loopback(&host, port).await?;

    let state = uuid::Uuid::new_v4().to_string();
    let url = build_authorize_url(&AuthorizeParams {
        client_id: &config.client_id,
        redirect_uri: &config.redirect_uri,
        scopes: SCOPES,
        state: &state,
    })?;

    println!("Authorising Fortnox for {COMPANY}.");
    println!();
    println!("Scopes requested:");
    for scope in SCOPES {
        println!("  {scope}");
    }
    println!();
    println!("Open this URL, sign in, and approve — for {COMPANY}, not for any");
    println!("other company the account can reach:");
    println!();
    println!("  {url}");
    println!();
    println!("Waiting for the redirect to {} ...", config.redirect_uri);

    let code = tokio::time::timeout(
        CONSENT_TIMEOUT,
        wait_for_code(listeners, state.clone(), redirect_path),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out after {}s waiting for Fortnox to redirect back. Nothing was \
             written; run the command again.",
            CONSENT_TIMEOUT.as_secs()
        )
    })??;

    let oauth = OAuthClient::new(&config.client_id, &config.client_secret)?;
    let now = Utc::now();
    let response = oauth
        .exchange_code(&code, &config.redirect_uri)
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "{err}\n\
                 \n\
                 The usual cause is a redirect URI that does not match the one on the \
                 integration in the Fortnox Developer Portal. Fortnox compares them as \
                 strings: this command sent {uri:?}, and the portal entry must be that \
                 exactly — same scheme, same host spelling (\"localhost\" and \
                 \"127.0.0.1\" are different), same port, same path, no trailing slash \
                 unless the portal has one. Nothing was written.",
                uri = config.redirect_uri
            )
        })?;

    // The refusal. Fortnox always sends a refresh token, and `exchange_code`'s
    // response type requires one — but an empty string would deserialise, and
    // an empty grant is a connector that works until the access token expires
    // and then fails somewhere else entirely.
    if response.refresh_token.trim().is_empty() {
        bail!(
            "Fortnox returned an access token but an empty refresh token, so nothing was \
             written — an access token alone stops working in about an hour and cannot be \
             renewed. Run {AUTHORIZE_COMMAND} again; if it repeats, check the \
             integration's scopes in the Developer Portal."
        );
    }

    let granted = if response.scope.trim().is_empty() {
        SCOPES.join(" ")
    } else {
        response.scope.clone()
    };
    let tokens = StoredTokens {
        access_token: response.access_token,
        refresh_token: response.refresh_token,
        expires_at: now + chrono::Duration::seconds(response.expires_in),
        scope: granted.clone(),
    };
    store.save(&tokens)?;

    println!();
    println!("Wrote {} (mode 0600).", store.path().display());
    println!("Granted scopes: {granted}");

    // Fortnox can grant a subset — an integration registered for fewer scopes
    // than this asks for consents fine and then 403s on the endpoint the
    // missing scope covers, which reads as a bug in the connector.
    let missing: Vec<&str> = SCOPES
        .iter()
        .copied()
        .filter(|scope| !granted.split_whitespace().any(|g| g == *scope))
        .collect();
    if !missing.is_empty() {
        println!();
        println!("Warning: these requested scopes were NOT granted:");
        for scope in &missing {
            println!("  {scope}");
        }
        println!("Enable them on the integration at https://developer.fortnox.se/ and");
        println!("re-run this command. Until then the tools that need them will fail 403.");
    }

    println!();
    println!("The refresh token rotates on every refresh and lapses after 45 days of");
    println!("disuse. If Fortnox goes quiet, run {AUTHORIZE_COMMAND} again.");
    Ok(())
}

/// One listener per address the redirect host resolves to.
///
/// See the module docs: binding only the first resolved address is a coin
/// flip between `::1` and `127.0.0.1`, and losing it is a refused connection
/// *after* the human has consented.
async fn bind_loopback(host: &str, port: u16) -> anyhow::Result<Vec<TcpListener>> {
    let candidates: &[&str] = match host {
        "localhost" => &["127.0.0.1", "::1"],
        other => &[other],
    };

    let mut listeners = Vec::new();
    let mut last_error = None;
    for candidate in candidates {
        match TcpListener::bind((candidate.trim_matches(['[', ']']), port)).await {
            Ok(listener) => listeners.push(listener),
            Err(err) => last_error = Some((*candidate, err)),
        }
    }

    if listeners.is_empty() {
        let (candidate, err) = last_error.expect("a non-empty candidate list either binds or errs");
        return Err(err).with_context(|| {
            format!(
                "binding {candidate}:{port} for the OAuth redirect. Another process may be \
                 using it, or a previous run may still be waiting."
            )
        });
    }
    Ok(listeners)
}

/// Race every listener; the first to produce an outcome decides.
///
/// A channel rather than `select!` because the number of listeners is not
/// known at compile time.
async fn wait_for_code(
    listeners: Vec<TcpListener>,
    state: String,
    redirect_path: String,
) -> anyhow::Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::channel(listeners.len().max(1));
    let state = Arc::new(state);
    let redirect_path = Arc::new(redirect_path);
    let mut tasks = Vec::new();

    for listener in listeners {
        let tx = tx.clone();
        let state = Arc::clone(&state);
        let redirect_path = Arc::clone(&redirect_path);
        tasks.push(tokio::spawn(async move {
            let outcome = accept_until_code(&listener, &state, &redirect_path).await;
            let _ = tx.send(outcome).await;
        }));
    }
    drop(tx);

    let outcome = rx.recv().await.unwrap_or_else(|| {
        Err(anyhow::anyhow!(
            "every redirect listener stopped unexpectedly"
        ))
    });

    // The losing listeners hold the port; drop them before returning so a
    // re-run can bind again immediately.
    for task in tasks {
        task.abort();
    }
    outcome
}

/// Accept connections on one listener until one is Fortnox's redirect.
///
/// A loop rather than a single accept, because a browser opening the loopback
/// URL will also ask for `/favicon.ico`, and because anything at all on the
/// machine may connect to a listening port. Requests that are not the redirect
/// get a 404 and are ignored.
async fn accept_until_code(
    listener: &TcpListener,
    state: &str,
    redirect_path: &str,
) -> anyhow::Result<String> {
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

        // A relative request target; any base will do, since only the path and
        // query matter and the base is discarded.
        let Ok(parsed) = Url::parse("http://127.0.0.1").and_then(|base| base.join(&target)) else {
            respond(&mut socket, 400, "Bad request.").await;
            continue;
        };

        if parsed.path() != redirect_path {
            respond(&mut socket, 404, "Not found.").await;
            continue;
        }

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
                "Fortnox reported {error:?} instead of an authorization code. Nothing was \
                 written."
            );
        }

        let Some(code) = params.get("code") else {
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

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::net::TcpStream;

    /// Send one raw request line to `addr` and return the response text.
    async fn get(addr: std::net::SocketAddr, target: &str) -> String {
        let mut socket = TcpStream::connect(addr).await.expect("connecting");
        socket
            .write_all(format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
            .await
            .expect("writing the request");
        let mut response = String::new();
        let _ = socket.read_to_string(&mut response).await;
        response
    }

    #[tokio::test]
    async fn the_redirect_yields_the_code_and_the_browser_gets_a_page() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiting =
            tokio::spawn(async move { accept_until_code(&listener, "st8", "/callback").await });

        let response = get(addr, "/callback?code=THE-CODE&state=st8").await;

        assert_eq!(waiting.await.unwrap().unwrap(), "THE-CODE");
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        assert!(response.contains("Authorised."), "{response}");
        // The browser page must not echo the authorization code back.
        assert!(!response.contains("THE-CODE"), "{response}");
    }

    /// A browser asking for `/favicon.ico` while the consent tab loads must not
    /// end the wait — that would be a one-shot listener that misses its shot.
    #[tokio::test]
    async fn other_paths_are_404ed_and_the_wait_continues() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiting =
            tokio::spawn(async move { accept_until_code(&listener, "st8", "/callback").await });

        let favicon = get(addr, "/favicon.ico").await;
        assert!(favicon.starts_with("HTTP/1.1 404 "), "{favicon}");

        get(addr, "/callback?code=c2&state=st8").await;
        assert_eq!(waiting.await.unwrap().unwrap(), "c2");
    }

    /// The CSRF check. Anything on the machine can reach a listening loopback
    /// port during the consent window.
    #[tokio::test]
    async fn a_mismatched_state_fails_without_a_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiting =
            tokio::spawn(async move { accept_until_code(&listener, "st8", "/callback").await });

        let response = get(addr, "/callback?code=SOMEONE-ELSES&state=wrong").await;
        assert!(response.starts_with("HTTP/1.1 400 "), "{response}");

        let err = format!("{:#}", waiting.await.unwrap().unwrap_err());
        assert!(err.contains("state"), "{err}");
        assert!(err.contains("Nothing was written"), "{err}");
        assert!(!err.contains("SOMEONE-ELSES"), "{err}");
    }

    /// A refusal at the consent screen names the OAuth error code and nothing
    /// else.
    #[tokio::test]
    async fn a_refused_consent_is_an_error_naming_the_oauth_code() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let waiting =
            tokio::spawn(async move { accept_until_code(&listener, "st8", "/callback").await });

        get(addr, "/callback?error=access_denied&state=st8").await;

        let err = format!("{:#}", waiting.await.unwrap().unwrap_err());
        assert!(err.contains("access_denied"), "{err}");
    }

    /// See the module docs: a listener on one loopback family and a browser on
    /// the other is a refused connection after the human has already consented.
    #[tokio::test]
    async fn localhost_is_bound_on_every_family_it_resolves_to() {
        // Port 0 gives each family its own port, which is not what production
        // does — this asserts only that both are attempted and at least one
        // binds, which is the property `bind_loopback` is there for.
        let listeners = bind_loopback("localhost", 0).await.unwrap();
        assert!(
            !listeners.is_empty(),
            "localhost must bind at least one loopback address"
        );
        for listener in &listeners {
            assert!(listener.local_addr().unwrap().ip().is_loopback());
        }
    }

    /// A port already in use must fail *before* the consent URL is printed,
    /// with a message about the port rather than a silent hang.
    #[tokio::test]
    async fn a_busy_port_is_an_error_naming_the_port() {
        let held = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = held.local_addr().unwrap().port();

        let err = format!("{:#}", bind_loopback("127.0.0.1", port).await.unwrap_err());
        assert!(err.contains(&port.to_string()), "{err}");
    }
}
