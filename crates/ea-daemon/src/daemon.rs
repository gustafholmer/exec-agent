//! The daemon's IPC surface: the methods the control socket answers, and the
//! state they are answered from.
//!
//! [`Daemon`] owns the [`Executor`] and nothing else yet. Every method it
//! registers is a thin adapter — parse the params, call into the executor, hand
//! back JSON — so that the rules about what may happen live in one place
//! (`ea_core::policy` behind [`Executor::submit`]) rather than being restated
//! per endpoint.
//!
//! Today there is exactly one method, `propose`, because there is exactly one
//! thing in the system that can change the outside world. Note what is
//! deliberately absent, and must stay absent: any method that reaches a
//! connector directly. A tool call is an action, and an action reaches a
//! connector only through the gate. `connectors.call` was removed from this
//! socket once already for exactly that reason; do not put it back.

use std::sync::Arc;

use anyhow::{bail, Context};
use chrono::Duration;
use ea_core::store::actions::ProposeInput;
use serde::Deserialize;
use serde_json::Value;

use crate::connectors::{self, Registry};
use crate::executor::{Executor, ToolCaller};
use crate::ipc;

/// How long a proposal waits for a human before it expires. The brief's
/// default, and the only one there is until a caller asks for another.
pub const DEFAULT_TTL_SECS: i64 = 24 * 60 * 60;

/// Params of the `propose` method.
///
/// `connector` and `tool` are required: without them there is no action to
/// speak of, and a request missing either is a bug in the caller that should be
/// reported as one. Everything else has a default, because a proposal arriving
/// with a thin `preview` is still a proposal a human can look at, and refusing
/// it would lose information rather than protect anything.
#[derive(Debug, Clone, Deserialize)]
pub struct ProposeParams {
    pub connector: String,
    pub tool: String,
    #[serde(default = "empty_object")]
    pub args: Value,
    #[serde(default)]
    pub preview: String,
    #[serde(default)]
    pub rationale: String,
    /// Overrides [`DEFAULT_TTL_SECS`]. Must be positive.
    #[serde(default)]
    pub ttl_secs: Option<i64>,
}

fn empty_object() -> Value {
    Value::Object(serde_json::Map::new())
}

impl ProposeParams {
    fn into_input(self) -> anyhow::Result<ProposeInput> {
        let ttl_secs = self.ttl_secs.unwrap_or(DEFAULT_TTL_SECS);
        if ttl_secs <= 0 {
            bail!("propose: ttl_secs must be positive, got {ttl_secs}");
        }
        Ok(ProposeInput {
            connector: self.connector,
            tool: self.tool,
            args: self.args,
            preview: self.preview,
            rationale: self.rationale,
            ttl: Duration::seconds(ttl_secs),
        })
    }
}

/// The daemon's state, shared by every connection the IPC server accepts.
pub struct Daemon<C: ToolCaller> {
    executor: Arc<Executor<C>>,
}

impl<C: ToolCaller + Send + Sync + 'static> Daemon<C> {
    pub fn new(executor: Arc<Executor<C>>) -> Arc<Self> {
        Arc::new(Self { executor })
    }

    /// Register every method this daemon answers on `server`.
    ///
    /// One place, so that "what can be asked of the daemon over its socket" is
    /// a list you can read in ten seconds rather than something scattered
    /// through `main`.
    pub fn register(self: &Arc<Self>, server: &mut ipc::Server) {
        let this = Arc::clone(self);
        server.register("propose", move |params| {
            let this = Arc::clone(&this);
            Box::pin(async move { this.propose(params).await })
        });
    }

    /// `propose` — put an action through the policy gate.
    ///
    /// Returns the resulting `Action`, serialised. The caller reads its
    /// `status` to learn which of the three things happened: `executed` (policy
    /// said auto), `proposed` (a human must approve it), `rejected` (policy
    /// said no).
    ///
    /// An action naming a connector nobody has ever heard of is *not* an error
    /// here. `Policy::decide` defaults an unknown connector to `approve`, so it
    /// is recorded and queued for a human to look at — which is the correct
    /// response to a model hallucinating a tool: a person sees the nonsense,
    /// nothing is executed, and the session does not crash. Erroring instead
    /// would teach the model to retry; executing would be catastrophic.
    pub async fn propose(&self, params: Value) -> anyhow::Result<Value> {
        let params: ProposeParams = serde_json::from_value(params)
            .context("propose: invalid params; `connector` and `tool` are required strings")?;
        let (action, _executed) = self.executor.submit(params.into_input()?).await?;
        serde_json::to_value(action).context("propose: serialising the action")
    }
}

impl Daemon<Registry> {
    /// Build the production daemon from the user's configuration: the real
    /// database, the connectors discovered under the config directory, and the
    /// policy those connectors ship.
    ///
    /// A directory without a `policy.toml` is not a connector and is skipped by
    /// [`connectors::discover`], so the policy loaded here always covers
    /// exactly the connectors the registry knows about.
    pub fn from_config() -> anyhow::Result<Arc<Self>> {
        use std::sync::Mutex;

        use ea_core::policy::Policy;
        use ea_core::store::actions::ActionStore;
        use ea_core::store::runs::RunStore;

        let config_dir = ea_core::paths::config_dir();
        let manifests = connectors::discover(&config_dir)?;
        let dirs: Vec<_> = manifests.iter().map(|m| m.dir.clone()).collect();
        let policy = Policy::load_dirs(&dirs)?;
        tracing::info!(
            connectors = ?manifests.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            "loaded connectors and their policies"
        );

        let db_path = ea_core::paths::database_path();
        let conn = Arc::new(Mutex::new(ea_core::db::open(&db_path).with_context(
            || format!("opening the state database at {}", db_path.display()),
        )?));

        Ok(Self::new(Arc::new(Executor::new(
            ActionStore::new(Arc::clone(&conn)),
            RunStore::new(conn),
            policy,
            Registry::new(manifests),
        ))))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ea_core::policy::Policy;
    use ea_core::store::actions::ActionStore;
    use ea_core::store::runs::RunStore;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    type CallLog = Arc<Mutex<Vec<(String, String)>>>;

    /// Records every connector call it is asked to make, so a test can assert
    /// the far stronger property than "the status looks right": that the
    /// connector was never reached at all.
    ///
    /// The log is shared rather than owned because `Executor` deliberately does
    /// not hand its caller back out.
    #[derive(Default, Clone)]
    struct SpyCaller {
        calls: CallLog,
    }

    impl ToolCaller for SpyCaller {
        async fn call(&self, connector: &str, tool: &str, _args: Value) -> anyhow::Result<String> {
            self.calls
                .lock()
                .unwrap()
                .push((connector.to_string(), tool.to_string()));
            Ok("done".to_string())
        }
    }

    fn policy() -> Policy {
        Policy::parse(
            r#"
[canvas]
list_courses = "auto"

[fortnox]
record_voucher = "approve"
"#,
        )
        .unwrap()
    }

    struct Fixture {
        _dir: TempDir,
        daemon: Arc<Daemon<SpyCaller>>,
        log: CallLog,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let conn = Arc::new(Mutex::new(
                ea_core::db::open(&dir.path().join("state.db")).unwrap(),
            ));
            let caller = SpyCaller::default();
            let log = Arc::clone(&caller.calls);
            let executor = Executor::new(
                ActionStore::new(Arc::clone(&conn)),
                RunStore::new(conn),
                policy(),
                caller,
            );
            Self {
                _dir: dir,
                daemon: Daemon::new(Arc::new(executor)),
                log,
            }
        }

        fn calls(&self) -> Vec<(String, String)> {
            self.log.lock().unwrap().clone()
        }
    }

    fn params(connector: &str, tool: &str) -> Value {
        json!({
            "connector": connector,
            "tool": tool,
            "args": { "term": "HT26" },
            "preview": format!("{connector}.{tool}"),
            "rationale": "the agent asked",
        })
    }

    #[tokio::test]
    async fn an_auto_tool_comes_back_executed() {
        let f = Fixture::new();
        let action = f
            .daemon
            .propose(params("canvas", "list_courses"))
            .await
            .unwrap();
        assert_eq!(action["status"], "executed");
        assert_eq!(action["result"], "done");
        assert_eq!(
            f.calls(),
            vec![("canvas".to_string(), "list_courses".to_string())]
        );
    }

    #[tokio::test]
    async fn an_approve_tool_comes_back_proposed() {
        let f = Fixture::new();
        let action = f
            .daemon
            .propose(params("fortnox", "record_voucher"))
            .await
            .unwrap();
        assert_eq!(action["status"], "proposed");
        assert!(f.calls().is_empty(), "the connector must not be reached");
    }

    /// Review focus. A model that invents a connector must not crash the loop
    /// and must not be executed: the gate's `approve` default queues the
    /// nonsense for a human to see.
    #[tokio::test]
    async fn an_unknown_connector_is_queued_for_a_human_not_executed_and_not_an_error() {
        let f = Fixture::new();
        let action = f
            .daemon
            .propose(params("definitely-not-a-connector", "transfer_everything"))
            .await
            .expect("an unknown connector must not be an error");
        assert_eq!(action["status"], "proposed");
        assert_eq!(action["connector"], "definitely-not-a-connector");
        assert!(
            f.calls().is_empty(),
            "no connector call may be attempted for a connector that does not exist"
        );

        // And the daemon is unharmed: a real proposal still works afterwards.
        let next = f
            .daemon
            .propose(params("canvas", "list_courses"))
            .await
            .unwrap();
        assert_eq!(next["status"], "executed");
    }

    #[tokio::test]
    async fn an_unknown_tool_on_a_known_connector_is_also_queued() {
        let f = Fixture::new();
        let action = f
            .daemon
            .propose(params("canvas", "invented_tool"))
            .await
            .unwrap();
        assert_eq!(action["status"], "proposed");
        assert!(f.calls().is_empty());
    }

    #[tokio::test]
    async fn the_default_ttl_is_twenty_four_hours() {
        let f = Fixture::new();
        let action = f
            .daemon
            .propose(params("fortnox", "record_voucher"))
            .await
            .unwrap();
        let created: chrono::DateTime<chrono::Utc> = action["created_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::FixedOffset>>()
            .unwrap()
            .into();
        let expires: chrono::DateTime<chrono::Utc> = action["expires_at"]
            .as_str()
            .unwrap()
            .parse::<chrono::DateTime<chrono::FixedOffset>>()
            .unwrap()
            .into();
        assert_eq!((expires - created).num_seconds(), DEFAULT_TTL_SECS);
    }

    #[tokio::test]
    async fn a_non_positive_ttl_is_refused() {
        let f = Fixture::new();
        let mut p = params("fortnox", "record_voucher");
        p["ttl_secs"] = json!(0);
        let err = f.daemon.propose(p).await.unwrap_err();
        assert!(format!("{err:#}").contains("ttl_secs"), "{err:#}");
    }

    #[tokio::test]
    async fn params_missing_the_tool_are_an_error() {
        let f = Fixture::new();
        let err = f
            .daemon
            .propose(json!({ "connector": "fortnox", "preview": "x", "rationale": "y" }))
            .await
            .unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("invalid params"), "{text}");
        assert!(text.contains("tool"), "{text}");
        assert!(f.calls().is_empty());
    }

    /// The same malformed request over a real socket, because "returns an IPC
    /// error" and "the daemon stays up" are properties of the server, not of
    /// the handler.
    #[tokio::test]
    async fn malformed_params_are_an_ipc_error_and_the_daemon_keeps_serving() {
        let f = Fixture::new();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let mut server = ipc::Server::new(&path);
        f.daemon.register(&mut server);
        let handle = server.spawn().await.unwrap();

        let client = ea_core::ipc::Client::new(&path);

        let err = client
            .call(
                "propose",
                json!({ "connector": "fortnox", "preview": "x", "rationale": "y" }),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid params"), "{err:#}");

        // Same connection pool, next request: the daemon must still answer.
        let action = client
            .call("propose", params("canvas", "list_courses"))
            .await
            .expect("the daemon must survive a malformed request");
        assert_eq!(action["status"], "executed");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_proposal_round_trips_over_the_socket() {
        let f = Fixture::new();
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("d.sock");
        let mut server = ipc::Server::new(&path);
        f.daemon.register(&mut server);
        let handle = server.spawn().await.unwrap();

        let action = ea_core::ipc::Client::new(&path)
            .call("propose", params("fortnox", "record_voucher"))
            .await
            .unwrap();
        assert_eq!(action["status"], "proposed");
        assert!(action["id"].as_i64().unwrap() > 0);

        handle.shutdown().await;
    }
}
