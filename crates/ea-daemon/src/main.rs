//! The `ea-daemon` binary: assembly, and nothing else.
//!
//! Everything of substance lives in the library — the IPC methods in
//! [`daemon`], the periodic work in [`jobs`], the gate in [`executor`]. What is
//! here is the wiring that cannot be unit-tested because it is precisely the
//! act of reading the real configuration, opening the real database and
//! spawning the real children.
//!
//! The order below matters in two places:
//!
//! * the policy is loaded from the discovered connectors' own directories, so
//!   a connector without a `policy.toml` is not a connector at all (`discover`
//!   skips it) and cannot end up with an empty, permissive policy;
//! * the IPC socket is bound *before* the scheduler starts, so `ea status`
//!   answers during the first poll rather than after it.
//!
//! # Shutdown
//!
//! `launchd` sends SIGTERM. The daemon pauses the scheduler (so nothing new is
//! spawned), stops the tick loop and the Telegram poller, then waits up to
//! [`SHUTDOWN_DRAIN`] for work already in flight — a `watch_poll` halfway
//! through writing events, an approved action mid-call. Only then does it
//! close the socket and take the connector children down.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use ea_core::policy::Policy;
use ea_core::store::actions::ActionStore;
use ea_core::store::conversations::ConversationStore;
use ea_core::store::events::EventStore;
use ea_core::store::kv::KvStore;
use ea_core::store::retention::RetentionStore;
use ea_core::store::runs::RunStore;
use ea_daemon::config::DaemonConfig;
use ea_daemon::connectors::{self, Registry};
use ea_daemon::daemon::{Daemon, Deps, SHUTDOWN_DRAIN};
use ea_daemon::executor::Executor;
use ea_daemon::ipc;
use ea_daemon::jobs::{self, Pusher, TriageDeps};
use ea_daemon::lock::InstanceLock;
use ea_daemon::notify::log::NotificationLog;
use ea_daemon::notify::policy::NotificationPolicy;
use ea_daemon::notify::telegram::{Notifier, TelegramConfig, TelegramTransport, TOKEN_FILE};
use ea_daemon::notify::updates::{OffsetStore, UpdateLoop};
use ea_daemon::retention::{retention_job, RetentionDeps};
use ea_daemon::scheduler::Scheduler;
use ea_daemon::session::SessionRunner;
use ea_daemon::triage::SessionBoundary;

/// Where a `claude -p` session runs: deliberately empty, and outside any
/// repository, so a session inherits no CLAUDE.md and no auto-memory.
const SESSION_DIR: &str = "sessions";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_dir = ea_core::paths::config_dir();
    let state_dir = ea_core::paths::state_dir();

    // Before anything else touches the state directory. A second daemon
    // sharing this directory would share the database, the session budget and
    // the connectors' credentials, and would take the control socket away from
    // the first one without either noticing — see `lock`. Held for the whole
    // of `main`; the kernel releases it however this process ends.
    let _instance = InstanceLock::acquire(&ea_core::paths::lock_path())?;

    let config = DaemonConfig::load_from(&config_dir)?;
    tracing::info!(?config, "configuration loaded");

    // --- connectors and the policy they ship ------------------------------
    let manifests = connectors::discover(&config.connectors_dir)?;
    if manifests.is_empty() {
        tracing::warn!(
            dir = %config.connectors_dir.display(),
            "no connectors discovered: a directory needs both connector.toml and policy.toml"
        );
    }
    // Paired with the name from each `connector.toml`, so `load_dirs` can
    // refuse a policy file that declares a section belonging to somebody else.
    let dirs: Vec<(String, std::path::PathBuf)> = manifests
        .iter()
        .map(|m| (m.name.clone(), m.dir.clone()))
        .collect();
    let policy = Policy::load_dirs(&dirs)?;
    let connector_names: Vec<String> = manifests.iter().map(|m| m.name.clone()).collect();
    tracing::info!(connectors = ?connector_names, "loaded connectors and their policies");

    // --- state -------------------------------------------------------------
    let db_path = ea_core::paths::database_path();
    let conn = Arc::new(Mutex::new(ea_core::db::open(&db_path).with_context(
        || format!("opening the state database at {}", db_path.display()),
    )?));

    let registry = Arc::new(Registry::new(manifests.clone()));
    let executor = Arc::new(Executor::new(
        ActionStore::new(Arc::clone(&conn)),
        RunStore::new(Arc::clone(&conn)),
        policy.clone(),
        Arc::clone(&registry),
    ));

    // --- sessions ----------------------------------------------------------
    //
    // A missing `claude` is a degraded daemon, not a dead one: polling,
    // approvals and the whole gate still work, and only the thinking stops.
    // Saying so loudly beats refusing to start.
    let sessions: Option<Arc<dyn SessionBoundary>> = match SessionRunner::discover(
        RunStore::new(Arc::clone(&conn)),
        state_dir.join(SESSION_DIR),
        manifests.clone(),
    ) {
        Ok(runner) => Some(Arc::new(runner)),
        Err(err) => {
            tracing::warn!(
                error = %format!("{err:#}"),
                "no session runner: triage will do tier 0 only and `ea chat` will not reply"
            );
            None
        }
    };

    // --- telegram ----------------------------------------------------------
    let telegram = load_telegram(&config, &config_dir)?;
    let mut pusher: Option<Arc<dyn Pusher>> = None;
    let mut update_loop = None;
    if let Some(credentials) = telegram {
        let notifier = Arc::new(Notifier::new(
            TelegramTransport::from_config(&credentials)?,
            ActionStore::new(Arc::clone(&conn)),
            Arc::clone(&executor),
            credentials.owner_id,
        ));
        // A second transport for the polling half. It is the same credentials
        // and a different HTTP client: a 25-second long poll must not sit in
        // the same connection pool slot as an outgoing notification.
        let poller = TelegramTransport::from_config(&credentials)?;
        update_loop = Some(UpdateLoop::new(
            poller,
            Arc::clone(&notifier),
            credentials.chat_id,
            OffsetStore::new(KvStore::new(Arc::clone(&conn))),
        ));
        pusher = Some(notifier as Arc<dyn Pusher>);
        tracing::info!(owner = %credentials.owner_id, "telegram configured");
    } else {
        tracing::warn!(
            "telegram is not configured: nothing will be pushed and no button can be pressed"
        );
    }

    // --- what the last run left behind --------------------------------------
    //
    // Before the scheduler starts and before the socket is bound: an action
    // the previous process was mid-way through executing when it died is
    // resolved as failed-with-unknown-outcome and never retried. See
    // `recovery`.
    ea_daemon::recovery::sweep_stranded(
        &ActionStore::new(Arc::clone(&conn)),
        &NotificationLog::new(KvStore::new(Arc::clone(&conn))),
        pusher.as_ref(),
    )
    .await?;

    // --- scheduler ---------------------------------------------------------
    let scheduler = Arc::new(Scheduler::with_cooldown(
        config.breaker_threshold,
        config.breaker_cooldown,
        config.breaker_max_cooldown,
    ));
    // A tripped breaker is the daemon reporting on itself, and this is the
    // only channel that reaches the owner without them asking. See
    // `scheduler::HEALTH_REPUSH_INTERVAL` for the once-per-trip rule and for
    // why these ignore quiet hours.
    scheduler.set_health_pusher(pusher.clone());
    for manifest in &manifests {
        scheduler.add(jobs::watch_job(
            manifest.name.clone(),
            manifest.watch_interval,
            Arc::clone(&registry),
            EventStore::new(Arc::clone(&conn)),
            policy.clone(),
        ));
    }
    scheduler.add(jobs::triage_job(
        config.triage_interval,
        Arc::new(TriageDeps {
            events: EventStore::new(Arc::clone(&conn)),
            actions: ActionStore::new(Arc::clone(&conn)),
            runs: RunStore::new(Arc::clone(&conn)),
            sessions: sessions.clone(),
            pusher: pusher.clone(),
            log: NotificationLog::new(KvStore::new(Arc::clone(&conn))),
            notify: NotificationPolicy::new(config.notify.clone()),
            rules: config.tier0.clone(),
            daily_session_budget: config.daily_session_budget,
        }),
    ));
    // The only job that deletes anything. Everything else in this daemon is
    // append-only, which over the years this thing is meant to run unattended
    // is a leak rather than an audit trail.
    scheduler.add(retention_job(
        config.retention.interval,
        Arc::new(RetentionDeps {
            store: RetentionStore::new(Arc::clone(&conn)),
            policy: config.retention.policy,
            // Where the plist points StandardOutPath and StandardErrorPath.
            log_dir: state_dir.clone(),
            log_max_bytes: config.retention.log_max_bytes,
        }),
    ));
    tracing::info!(jobs = ?scheduler.names(), "scheduled");

    // --- the socket --------------------------------------------------------
    let daemon = Daemon::build(Deps {
        executor,
        actions: ActionStore::new(Arc::clone(&conn)),
        conversations: ConversationStore::new(Arc::clone(&conn)),
        events: EventStore::new(Arc::clone(&conn)),
        runs: RunStore::new(Arc::clone(&conn)),
        scheduler: Arc::clone(&scheduler),
        sessions,
        pusher,
        notify_log: NotificationLog::new(KvStore::new(Arc::clone(&conn))),
        connectors: connector_names,
        daily_session_budget: config.daily_session_budget,
        chat_model: config.chat_model.clone(),
    });

    let socket_path = ea_core::paths::socket_path();
    let mut server = ipc::Server::new(&socket_path);
    daemon.register(&mut server);
    let server_handle = server.spawn().await?;
    tracing::info!("ea-daemon listening on {}", socket_path.display());

    // --- run ---------------------------------------------------------------
    let ticker = scheduler.start();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let poller = update_loop.map(|lp| tokio::spawn(lp.run(shutdown_rx)));

    wait_for_signal().await?;

    // --- shut down ---------------------------------------------------------
    tracing::info!("shutting down");
    // Pause first: the drain's bound only means anything if nothing new can be
    // spawned while it waits.
    scheduler.pause();
    ticker.abort();
    let _ = shutdown_tx.send(true);

    if !scheduler.drain(SHUTDOWN_DRAIN).await {
        tracing::warn!("some scheduler jobs were still running at shutdown");
    }
    if let Some(poller) = poller {
        match tokio::time::timeout(Duration::from_secs(5), poller).await {
            Ok(_) => tracing::debug!("telegram poller stopped"),
            Err(_) => tracing::warn!("telegram poller did not stop in time"),
        }
    }

    server_handle.shutdown().await;
    registry.shutdown().await;
    tracing::info!("stopped");
    Ok(())
}

/// The credentials, or `None` when Telegram simply is not set up.
///
/// "Not set up" is the absence of the token file *and* of a `[telegram]` block
/// — anything else (a token that is world-readable, a chat id that is not a
/// number) is a misconfiguration and fails startup, because silently running
/// without notifications is exactly the failure this system cannot afford.
fn load_telegram(
    config: &DaemonConfig,
    config_dir: &std::path::Path,
) -> anyhow::Result<Option<TelegramConfig>> {
    if let Some(settings) = &config.telegram {
        return settings.resolve(config_dir).map(Some);
    }
    if !config_dir.join(TOKEN_FILE).exists() {
        return Ok(None);
    }
    TelegramConfig::load_from(config_dir).map(Some)
}

/// Wait for SIGINT or SIGTERM.
///
/// SIGTERM is what `launchd` sends; SIGINT is Ctrl-C in a terminal. Both are a
/// request to stop, and both get the same orderly shutdown rather than a
/// default-disposition process kill.
async fn wait_for_signal() -> anyhow::Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut term = signal(SignalKind::terminate()).context("installing the SIGTERM handler")?;
    let mut int = signal(SignalKind::interrupt()).context("installing the SIGINT handler")?;

    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = int.recv() => tracing::info!("SIGINT"),
    }
    Ok(())
}
