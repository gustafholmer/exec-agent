//! End-to-end tests for the `ea-google-authorize` binary.
//!
//! These spawn the real binary with `$EA_CONFIG_DIR` pointed at a temp
//! directory and `tokenUri` pointed at a `wiremock` server, then drive the
//! loopback redirect with an ordinary HTTP GET. Nothing contacts Google: the
//! `authUri` is never fetched by this process (a human's browser would fetch
//! it), and the token endpoint is the mock.
//!
//! The case that earns the cost of spawning a process is the refusal: an
//! account written without a refresh token works for about an hour and then
//! fails inside the daemon, hours later and nowhere near the command that
//! caused it. Nothing short of running the binary proves it does not write.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const ACCESS_TOKEN: &str = "ya29.ACCESS-SECRET-do-not-log-me";
const REFRESH_TOKEN: &str = "1//REFRESH-SECRET-do-not-log-me";
const CLIENT_SECRET: &str = "GOCSPX-CLIENT-SECRET-do-not-log-me";

/// A port nothing is listening on. Racy in principle; the window between the
/// drop here and the child's bind is microseconds, and there is no portable
/// way to hand a bound socket to a child.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn write_app_config(config_dir: &Path, token_uri: &str, redirect_uri: &str) {
    let google = config_dir.join("google");
    std::fs::create_dir_all(&google).unwrap();
    std::fs::set_permissions(&google, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = google.join("app.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "clientId": "123.apps.googleusercontent.com",
            "clientSecret": CLIENT_SECRET,
            "redirectUri": redirect_uri,
            // Never fetched by this process; a human's browser would open it.
            "authUri": "https://accounts.example.invalid/o/oauth2/v2/auth",
            "tokenUri": token_uri,
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

struct Outcome {
    success: bool,
    stdout: String,
    stderr: String,
    tokens_path: PathBuf,
}

/// Run the binary against `token_response`, drive the redirect, and collect
/// everything the test might want to assert on.
async fn run_authorize(config_dir: &Path, token_response: ResponseTemplate) -> Outcome {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(token_response)
        .mount(&server)
        .await;

    let port = free_port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    write_app_config(
        config_dir,
        &format!("{}/token", server.uri()),
        &redirect_uri,
    );

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-google-authorize"))
        .arg("work")
        .env("EA_CONFIG_DIR", config_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning ea-google-authorize");

    // Read stdout until the consent URL appears; the child is blocked on
    // `accept` from that point on.
    let mut stdout = String::new();
    let mut state = None;
    {
        let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            stdout.push_str(&line);
            stdout.push('\n');
            if let Some(found) = line.trim().strip_prefix("https://") {
                let url = reqwest::Url::parse(&format!("https://{found}")).unwrap();
                if let Some((_, value)) = url.query_pairs().find(|(k, _)| k == "state") {
                    state = Some(value.into_owned());
                    break;
                }
            }
        }
        // The rest of stdout is drained after the process exits, below.
        let remainder = tokio::spawn(async move {
            let mut rest = String::new();
            while let Ok(Some(line)) = lines.next_line().await {
                rest.push_str(&line);
                rest.push('\n');
            }
            rest
        });

        let state = state.expect("the binary must print a consent URL carrying `state`");

        // Play the browser. `reqwest` follows nothing interesting here; the
        // binary answers with a small HTML page either way.
        let callback = format!("http://127.0.0.1:{port}/callback?code=AUTH-CODE&state={state}");
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        // The child may not have reached `accept` yet on a loaded machine.
        for attempt in 0..50 {
            match client.get(&callback).send().await {
                Ok(_) => break,
                Err(err) => {
                    assert!(attempt < 49, "the redirect never connected: {err}");
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }

        let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .expect("ea-google-authorize did not exit")
            .unwrap();
        stdout.push_str(&remainder.await.unwrap());

        let mut stderr = String::new();
        use tokio::io::AsyncReadExt;
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .await
            .unwrap();

        Outcome {
            success: status.success(),
            stdout,
            stderr,
            tokens_path: config_dir.join("google").join("work.json"),
        }
    }
}

/// The happy path: tokens land on disk at `0600`, and nothing secret is
/// printed to either stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_consent_writes_the_account_at_mode_0600() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outcome = run_authorize(
        tmp.path(),
        ResponseTemplate::new(200).set_body_raw(
            serde_json::json!({
                "access_token": ACCESS_TOKEN,
                "refresh_token": REFRESH_TOKEN,
                "expires_in": 3599,
                "scope": ea_google::auth::SCOPES.join(" "),
                "token_type": "Bearer",
            })
            .to_string(),
            "application/json",
        ),
    )
    .await;

    assert!(
        outcome.success,
        "stdout:\n{}\nstderr:\n{}",
        outcome.stdout, outcome.stderr
    );

    let mode = std::fs::metadata(&outcome.tokens_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);

    let store = ea_google::auth::TokenStore::new(Some(tmp.path().join("google")));
    let tokens = store.read("work").unwrap();
    assert_eq!(tokens.refresh_token, REFRESH_TOKEN);
    assert_eq!(tokens.access_token, ACCESS_TOKEN);
    assert_eq!(store.list().unwrap(), vec!["work".to_string()]);

    // The whole point of the crate: none of this reaches a terminal, a log, or
    // a shell history file.
    for stream in [&outcome.stdout, &outcome.stderr] {
        for secret in [ACCESS_TOKEN, REFRESH_TOKEN, CLIENT_SECRET, "AUTH-CODE"] {
            assert!(!stream.contains(secret), "{secret} was printed:\n{stream}");
        }
    }
}

/// The refusal. Google issues a refresh token only on the first consent for a
/// given client, so this is what a re-authorisation looks like when
/// `prompt=consent` has not done its job. Writing the access token anyway
/// would work for an hour and then fail somewhere else entirely.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_response_with_no_refresh_token_is_refused_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outcome = run_authorize(
        tmp.path(),
        ResponseTemplate::new(200).set_body_raw(
            serde_json::json!({
                "access_token": ACCESS_TOKEN,
                "expires_in": 3599,
                "scope": ea_google::auth::SCOPES.join(" "),
                "token_type": "Bearer",
            })
            .to_string(),
            "application/json",
        ),
    )
    .await;

    assert!(
        !outcome.success,
        "the command must fail:\n{}",
        outcome.stdout
    );
    assert!(
        !outcome.tokens_path.exists(),
        "nothing may be written: {} exists",
        outcome.tokens_path.display()
    );

    // The only fix is to revoke the existing grant, so the message has to say
    // where.
    assert!(
        outcome
            .stderr
            .contains("https://myaccount.google.com/permissions"),
        "stderr:\n{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("ea-google-authorize work"),
        "stderr:\n{}",
        outcome.stderr
    );
    for secret in [ACCESS_TOKEN, CLIENT_SECRET, "AUTH-CODE"] {
        assert!(
            !outcome.stderr.contains(secret) && !outcome.stdout.contains(secret),
            "{secret} was printed"
        );
    }
}

/// A redirect carrying somebody else's `state` is not the consent this command
/// started, and must not be exchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_redirect_with_the_wrong_state_is_refused() {
    let tmp = tempfile::TempDir::new().unwrap();
    let server = MockServer::start().await;
    // `.expect(0)`: wiremock itself fails the test if the code is exchanged.
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let port = free_port();
    write_app_config(
        tmp.path(),
        &format!("{}/token", server.uri()),
        &format!("http://127.0.0.1:{port}/callback"),
    );

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-google-authorize"))
        .arg("work")
        .env("EA_CONFIG_DIR", tmp.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.contains("state=") {
            break;
        }
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let callback = format!("http://127.0.0.1:{port}/callback?code=AUTH-CODE&state=not-the-state");
    for attempt in 0..50 {
        if client.get(&callback).send().await.is_ok() {
            break;
        }
        assert!(attempt < 49, "the redirect never connected");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("ea-google-authorize did not exit")
        .unwrap();

    assert!(!status.success());
    assert!(!tmp.path().join("google").join("work.json").exists());
    server.verify().await;
}

/// The path-traversal guard reaches the command line too.
#[tokio::test]
async fn a_traversing_account_label_is_refused_before_anything_is_bound() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-google-authorize"))
        .arg("../../.ssh/id_rsa")
        .env("EA_CONFIG_DIR", tmp.path())
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("^[A-Za-z0-9][A-Za-z0-9_-]*$"),
        "stderr:\n{stderr}"
    );
}
