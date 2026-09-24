//! End-to-end tests for the `ea-kth-authorize` binary.
//!
//! These spawn the real binary with `$EA_CONFIG_DIR` pointed at a temp
//! directory and the authority pointed at a `wiremock` server, then drive the
//! loopback redirect with an ordinary HTTP GET. **Nothing contacts
//! Microsoft**: the `authorize` endpoint is never fetched by this process (a
//! human's browser would fetch it), and the token endpoint is the mock.
//!
//! Two cases earn the cost of spawning a process.
//!
//! * **PKCE actually happens.** The verifier is generated inside the child,
//!   never printed, and must arrive at the token endpoint. Nothing short of
//!   running the binary can show that, because the verifier never crosses a
//!   function boundary a unit test can reach.
//! * **The refusal.** An account written without a refresh token works for
//!   about an hour and then fails inside the daemon, hours later and nowhere
//!   near the command that caused it.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const ACCESS_TOKEN: &str = "eyJ0eXAi-ACCESS-SECRET-do-not-log-me";
const REFRESH_TOKEN: &str = "0.AXoA-REFRESH-SECRET-do-not-log-me";

/// A port nothing is listening on. Racy in principle; the window between the
/// drop here and the child's bind is microseconds, and there is no portable
/// way to hand a bound socket to a child.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn write_app_config(config_dir: &Path, authority: &str, redirect_uri: &str) {
    let kth = config_dir.join("kth");
    std::fs::create_dir_all(&kth).unwrap();
    std::fs::set_permissions(&kth, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        kth.join("app.json"),
        serde_json::json!({
            "clientId": "11111111-2222-3333-4444-555555555555",
            "redirectUri": redirect_uri,
            // The `/authorize` half is never fetched by this process; a
            // human's browser would open it.
            "authority": authority,
        })
        .to_string(),
    )
    .unwrap();
}

/// Records the body of every token POST, so a test can assert on what the
/// child actually sent without the child having to print it.
struct RecordingResponder {
    template: ResponseTemplate,
    bodies: Arc<Mutex<Vec<String>>>,
}

impl Respond for RecordingResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if let Ok(mut bodies) = self.bodies.lock() {
            bodies.push(String::from_utf8_lossy(&request.body).into_owned());
        }
        self.template.clone()
    }
}

struct Outcome {
    success: bool,
    stdout: String,
    stderr: String,
    tokens_path: PathBuf,
    /// The bodies of the token POSTs the child made.
    token_requests: Vec<String>,
}

/// Run the binary against `token_response`, drive the redirect, and collect
/// everything the test might want to assert on.
async fn run_authorize(config_dir: &Path, token_response: ResponseTemplate) -> Outcome {
    let server = MockServer::start().await;
    let bodies = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .respond_with(RecordingResponder {
            template: token_response,
            bodies: Arc::clone(&bodies),
        })
        .mount(&server)
        .await;

    let port = free_port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");
    write_app_config(config_dir, &server.uri(), &redirect_uri);

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-kth-authorize"))
        .arg("kth")
        .env("EA_CONFIG_DIR", config_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawning ea-kth-authorize");

    // Read stdout until the consent URL appears; the child is blocked on
    // `accept` from that point on.
    let mut stdout = String::new();
    let mut state = None;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        stdout.push_str(&line);
        stdout.push('\n');
        let trimmed = line.trim();
        if let Some(found) = trimmed.strip_prefix("http") {
            if let Ok(url) = reqwest::Url::parse(&format!("http{found}")) {
                if let Some((_, value)) = url.query_pairs().find(|(k, _)| k == "state") {
                    state = Some(value.into_owned());
                    break;
                }
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

    // Play the browser.
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
        .expect("ea-kth-authorize did not exit")
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

    let token_requests = bodies.lock().map(|b| b.clone()).unwrap_or_default();

    Outcome {
        success: status.success(),
        stdout,
        stderr,
        tokens_path: config_dir.join("kth").join("kth.json"),
        token_requests,
    }
}

/// The happy path: tokens land on disk at `0600`, the exchange carried a PKCE
/// verifier matching the challenge that was advertised, and nothing secret is
/// printed to either stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_successful_consent_writes_the_account_at_mode_0600_and_proves_pkce() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outcome = run_authorize(
        tmp.path(),
        ResponseTemplate::new(200).set_body_raw(
            serde_json::json!({
                "access_token": ACCESS_TOKEN,
                "refresh_token": REFRESH_TOKEN,
                "expires_in": 3599,
                "scope": "Mail.Read",
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

    let store = ea_kth::auth::TokenStore::new(Some(tmp.path().join("kth")));
    let tokens = store.read("kth").unwrap();
    assert_eq!(tokens.refresh_token, REFRESH_TOKEN);
    assert_eq!(tokens.access_token, ACCESS_TOKEN);
    assert_eq!(store.list().unwrap(), vec!["kth".to_string()]);

    // PKCE end to end. This is a public client with no secret, so the
    // verifier is the only thing binding the redirect to this process — and
    // it must be the verifier for the challenge that was advertised, not any
    // old string.
    assert_eq!(outcome.token_requests.len(), 1);
    let body = &outcome.token_requests[0];
    let verifier = body
        .split('&')
        .find_map(|pair| pair.strip_prefix("code_verifier="))
        .expect("the code exchange must carry a code_verifier: {body}");
    assert!(body.contains("grant_type=authorization_code"), "{body}");

    // Search for the *consent* URL specifically: stdout also prints the scope
    // list, and `https://graph.microsoft.com/Mail.Read` parses as a URL too.
    let challenge = outcome
        .stdout
        .split_whitespace()
        .filter_map(|word| reqwest::Url::parse(word).ok())
        .find_map(|url| {
            url.query_pairs()
                .find(|(k, _)| k == "code_challenge")
                .map(|(_, v)| v.into_owned())
        })
        .expect("the consent URL must carry a code_challenge");
    assert_eq!(
        ea_kth::auth::code_challenge_s256(verifier),
        challenge,
        "the verifier sent to the token endpoint must be the one the advertised \
         challenge was derived from"
    );

    // The whole point: none of this reaches a terminal, a log, or a shell
    // history file.
    for stream in [&outcome.stdout, &outcome.stderr] {
        for secret in [ACCESS_TOKEN, REFRESH_TOKEN, "AUTH-CODE", verifier] {
            assert!(!stream.contains(secret), "{secret} was printed:\n{stream}");
        }
    }
}

/// The refusal. An access token with no refresh token works for an hour and
/// then fails inside the daemon, nowhere near the command that caused it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_response_with_no_refresh_token_is_refused_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let outcome = run_authorize(
        tmp.path(),
        ResponseTemplate::new(200).set_body_raw(
            serde_json::json!({
                "access_token": ACCESS_TOKEN,
                "expires_in": 3599,
                "scope": "Mail.Read",
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
    assert!(
        outcome.stderr.contains("offline_access"),
        "the message must name the likely cause:\n{}",
        outcome.stderr
    );
    assert!(
        outcome.stderr.contains("ea-kth-authorize kth"),
        "stderr:\n{}",
        outcome.stderr
    );
    for secret in [ACCESS_TOKEN, "AUTH-CODE"] {
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
        &server.uri(),
        &format!("http://127.0.0.1:{port}/callback"),
    );

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-kth-authorize"))
        .arg("kth")
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
        .expect("ea-kth-authorize did not exit")
        .unwrap();

    assert!(!status.success());
    assert!(!tmp.path().join("kth").join("kth.json").exists());
    server.verify().await;
}

/// A consent the tenant refused arrives as `?error=…` on the redirect, and is
/// the outcome this whole command exists to discover. The message has to say
/// what it means and what to do instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_tenant_that_refuses_consent_is_reported_as_such_and_writes_nothing() {
    let tmp = tempfile::TempDir::new().unwrap();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;

    let port = free_port();
    write_app_config(
        tmp.path(),
        &server.uri(),
        &format!("http://127.0.0.1:{port}/callback"),
    );

    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-kth-authorize"))
        .arg("kth")
        .env("EA_CONFIG_DIR", tmp.path())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut state = None;
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(found) = line.trim().strip_prefix("http") {
            if let Ok(url) = reqwest::Url::parse(&format!("http{found}")) {
                if let Some((_, value)) = url.query_pairs().find(|(k, _)| k == "state") {
                    state = Some(value.into_owned());
                    break;
                }
            }
        }
    }
    let state = state.expect("a consent URL");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let callback = format!(
        "http://127.0.0.1:{port}/callback?error=access_denied&error_description=\
         AADSTS65001%3A%20The%20user%20or%20administrator%20has%20not%20consented&state={state}"
    );
    for attempt in 0..50 {
        if client.get(&callback).send().await.is_ok() {
            break;
        }
        assert!(attempt < 49, "the redirect never connected");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
        .await
        .expect("ea-kth-authorize did not exit")
        .unwrap();
    let mut stderr = String::new();
    use tokio::io::AsyncReadExt;
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .await
        .unwrap();

    assert!(!status.success());
    assert!(!tmp.path().join("kth").join("kth.json").exists());
    assert!(stderr.contains("AADSTS65001"), "stderr:\n{stderr}");
    assert!(
        stderr.contains("Gmail"),
        "the refusal must name the fallback that needs no code:\n{stderr}"
    );
    server.verify().await;
}

/// The path-traversal guard reaches the command line too.
#[tokio::test]
async fn a_traversing_account_label_is_refused_before_anything_is_bound() {
    let tmp = tempfile::TempDir::new().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_ea-kth-authorize"))
        .arg("../../.ssh/id_rsa")
        .env("EA_CONFIG_DIR", tmp.path())
        .output()
        .await
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("^[a-z0-9][a-z0-9_-]*$"),
        "stderr:\n{stderr}"
    );
}
