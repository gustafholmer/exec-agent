//! The daemon's periodic work: polling connectors and running triage, on a
//! per-job clock, contained from each other and from the process itself.
//!
//! Task 13 registers two kinds of [`Job`] here: one per connector's
//! `watch_poll`, on that connector's declared interval, and one for the
//! triage pass. Both share the same failure mode -- a connector with a
//! lapsed OAuth token, or a triage pass that hits a bad event -- and this
//! module exists so that failure mode stays local to the one job, rather
//! than reaching the rest of the daemon.
//!
//! Three properties matter enough to call out:
//!
//! - **A job never overlaps itself.** `watch_poll` on a slow connector can
//!   still be running when the next tick fires. Running it again
//!   concurrently would double-record events and could double-spend a
//!   session, so a job that is still in flight is simply skipped until it
//!   finishes -- see [`Scheduler::maybe_spawn`].
//! - **A panic is a failure, not a crash.** `run` returns a future; if
//!   polling that future panics, the panic is caught (see [`RunningGuard`]
//!   and the `catch_unwind` in `maybe_spawn`) and recorded exactly like an
//!   `Err` would be. A connector poll must never be able to take the whole
//!   daemon down.
//! - **A tripped breaker is visible, not silent.** [`Scheduler::last_error`]
//!   keeps the message from the failure that tripped (or most recently
//!   failed) the breaker, so `ea status` has something to show the owner
//!   beyond "not running" -- this is not in the brief's interface sketch, but
//!   a breaker that trips silently is worse than no breaker at all.
//! - **A tripped breaker is temporary.** This daemon runs unattended for
//!   weeks; a breaker that only a restart could clear turns a transient
//!   outage -- an expired session, a Canvas maintenance window, a flaky
//!   network -- into a permanent one, and the owner finds out weeks later by
//!   noticing that nothing has happened. So the breaker is *half-open* after
//!   a cooling-off period ([`DEFAULT_BREAKER_COOLDOWN`], doubling to
//!   [`MAX_BREAKER_COOLDOWN`]): one attempt is allowed, a success closes the
//!   breaker, a failure re-opens it and waits longer. A human can short-cut
//!   the wait with [`Scheduler::reset`], which `ea resume <job>` reaches over
//!   the socket.
//!
//! `tick` takes the clock explicitly so callers -- tests, and [`Scheduler::start`]
//! -- decide how time advances; nothing in here reads the wall clock itself
//! except `start`'s own loop.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt;
use tokio::task::JoinHandle;

/// How long a tripped breaker waits before allowing one half-open attempt.
///
/// Five minutes. The shortest interval anything here actually runs on is the
/// triage pass at five minutes and a connector `watch_poll` at five to thirty,
/// so a cooldown shorter than this would only retry inside a window the job
/// would not have run in anyway. Longer would be worse: the breaker exists to
/// stop a *storm* of failing calls, and five failing calls spread over five
/// minutes is not a storm. Note that the cooldown is a floor, not a schedule:
/// a job whose own interval is longer keeps its own interval (see
/// [`Scheduler::wait_for`]), so a 30-minute connector is never polled more
/// often while broken than while healthy.
pub const DEFAULT_BREAKER_COOLDOWN: Duration = Duration::from_secs(5 * 60);

/// The ceiling the cooldown doubles up to.
///
/// One hour. A service that has been down long enough to fail eight
/// consecutive half-open probes is not coming back in the next minute, and an
/// hour keeps the daily cost of a dead connector at 24 calls while still
/// meaning that *whenever* it comes back the daemon notices within the hour
/// without anybody touching it. Anything longer and the owner would beat the
/// daemon to it, which defeats the point of automatic recovery.
pub const MAX_BREAKER_COOLDOWN: Duration = Duration::from_secs(60 * 60);

/// A future a job's `run` produces. Matches `futures::future::BoxFuture`'s
/// shape, spelled out locally so this module does not need the rest of
/// `futures` beyond [`FutureExt::catch_unwind`].
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// One piece of periodic work.
///
/// `run` is `Fn`, not `FnOnce` or `FnMut`, because the scheduler calls it
/// again on every interval for the life of the daemon; it must be safe to
/// invoke concurrently with its own prior invocation completing (the
/// overlap guard keeps that from actually happening, but `run` itself -- a
/// closure over an `Arc<Registry>` or similar -- only needs to be `Sync`,
/// not reentrant-proof).
#[derive(Clone)]
pub struct Job {
    pub name: String,
    pub interval: Duration,
    pub run: Arc<dyn Fn() -> BoxFuture<anyhow::Result<()>> + Send + Sync>,
}

impl Job {
    /// Convenience constructor so callers don't have to spell out the `Arc`
    /// and the `Pin<Box<_>>` at every call site.
    pub fn new<F, Fut>(name: impl Into<String>, interval: Duration, run: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        Job {
            name: name.into(),
            interval,
            run: Arc::new(move || Box::pin(run()) as BoxFuture<anyhow::Result<()>>),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Per-job bookkeeping: schedule, overlap guard, and circuit breaker.
struct JobState {
    job: Job,
    /// When this job last started, so `tick` can tell whether the interval
    /// has elapsed. `None` means it has never run -- which is why the very
    /// first tick always runs every job.
    last_started: Mutex<Option<Instant>>,
    /// Whether an invocation of this job is currently in flight. Guarded by
    /// `compare_exchange` rather than a `Mutex<bool>` so the
    /// check-and-set that decides "is this job already running" is a single
    /// atomic operation with no window for two ticks to both win.
    running: Arc<AtomicBool>,
    /// Consecutive failures (panics counted the same as an `Err`) since the
    /// last success or `reset`.
    failure_count: AtomicU32,
    /// Set once `failure_count` reaches the breaker threshold. A tripped job
    /// is skipped by every `tick` until its `cooldown` has elapsed, which buys
    /// it one half-open attempt -- or until [`Scheduler::reset`] clears it.
    tripped: AtomicBool,
    /// How long this job waits, while tripped, before that half-open attempt.
    /// Starts at the scheduler's [`DEFAULT_BREAKER_COOLDOWN`], doubles every
    /// time a half-open attempt fails, and is capped at
    /// [`MAX_BREAKER_COOLDOWN`]. A success -- or a `reset` -- puts it back.
    cooldown: Mutex<Duration>,
    /// The message from the most recent failure, cleared on the next
    /// success or on `reset`. This is what makes a tripped breaker visible
    /// to `ea status` instead of just a name with no explanation.
    last_error: Mutex<Option<String>>,
}

impl JobState {
    fn new(job: Job, cooldown: Duration) -> Self {
        JobState {
            job,
            last_started: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            failure_count: AtomicU32::new(0),
            tripped: AtomicBool::new(false),
            cooldown: Mutex::new(cooldown),
            last_error: Mutex::new(None),
        }
    }
}

/// Clears a job's `running` flag when dropped -- whether that is because its
/// invocation finished normally, because `catch_unwind` caught a panic, or
/// because the spawned task itself was dropped or aborted before either of
/// those happened. Without this, a panic that somehow escaped `catch_unwind`
/// (or a runtime shutdown mid-poll) would leave the flag set forever and the
/// job would never run again.
struct RunningGuard(Arc<AtomicBool>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "job panicked".to_string()
    }
}

/// Drives every registered [`Job`] on its own interval, isolating failures
/// (including panics) per job behind a circuit breaker, and guaranteeing a
/// job is never invoked while a prior invocation of it is still running.
pub struct Scheduler {
    breaker_threshold: u32,
    /// What a job's cooldown starts at, and is reset to by a success.
    breaker_cooldown: Duration,
    /// What it doubles up to and no further.
    max_breaker_cooldown: Duration,
    jobs: Mutex<Vec<Arc<JobState>>>,
    paused: AtomicBool,
    /// Handles to the invocations [`Scheduler::start`]'s loop has spawned and
    /// that have not been observed to finish.
    ///
    /// `start` used to discard what `tick` returned, which meant there was no
    /// way to wait for in-flight work on the way out: `launchd` sends SIGTERM,
    /// the process exits, and a `watch_poll` that was halfway through writing
    /// events is simply cut off. Aborting them would be *safe* -- the overlap
    /// marker is cleared by a guard the spawned future owns, so it clears on
    /// drop too -- but safe is not the same as finished, and a job mid-write
    /// is better waited for. Reaped on every tick so the vector does not grow
    /// for the life of the process.
    in_flight: Mutex<Vec<JoinHandle<()>>>,
}

impl Scheduler {
    /// How often `start`'s background loop calls `tick`.
    ///
    /// One second. `start` must not busy-loop, so it sleeps between checks
    /// via `tokio::time::interval` rather than spinning; one second is coarse
    /// enough that the loop is never meaningfully warm, yet fine enough that
    /// a job becomes overdue by at most a second before the scheduler notices
    /// -- negligible against the intervals this actually drives (connector
    /// `watch_poll` on the order of minutes, per
    /// `connectors::DEFAULT_WATCH_INTERVAL_SECS`, and a triage pass on a
    /// similar scale). A test-only job with a sub-second interval would run
    /// once a second rather than as fast as the interval nominally allows,
    /// but nothing the daemon actually schedules needs finer resolution than
    /// that.
    const TICK_GRANULARITY: Duration = Duration::from_secs(1);

    pub fn new(breaker_threshold: u32) -> Self {
        Self::with_cooldown(
            breaker_threshold,
            DEFAULT_BREAKER_COOLDOWN,
            MAX_BREAKER_COOLDOWN,
        )
    }

    /// [`Scheduler::new`] with explicit breaker cooldown bounds. Tests use it
    /// to make the half-open path observable without waiting five real
    /// minutes; production goes through `new`.
    pub fn with_cooldown(breaker_threshold: u32, cooldown: Duration, max: Duration) -> Self {
        Scheduler {
            breaker_threshold,
            breaker_cooldown: cooldown,
            max_breaker_cooldown: max.max(cooldown),
            jobs: Mutex::new(Vec::new()),
            paused: AtomicBool::new(false),
            in_flight: Mutex::new(Vec::new()),
        }
    }

    /// Register a job. Jobs are kept in the order they were added; `names`
    /// reflects that order.
    pub fn add(&self, job: Job) {
        lock(&self.jobs).push(Arc::new(JobState::new(job, self.breaker_cooldown)));
    }

    /// Registered job names, in the order they were added.
    pub fn names(&self) -> Vec<String> {
        lock(&self.jobs)
            .iter()
            .map(|s| s.job.name.clone())
            .collect()
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// Whether `name`'s circuit breaker has tripped. `false` for an unknown
    /// name.
    pub fn is_tripped(&self, name: &str) -> bool {
        self.find(name)
            .map(|s| s.tripped.load(Ordering::SeqCst))
            .unwrap_or(false)
    }

    /// The message from `name`'s most recent failure (panic or `Err`), if
    /// any -- present whether or not the breaker has tripped, so `ea status`
    /// can show why a connector is unhealthy even before it trips. Not part
    /// of the brief's interface sketch: added because a tripped breaker with
    /// no recorded reason would leave the owner guessing why, say, a
    /// connector with a lapsed OAuth token stopped polling.
    pub fn last_error(&self, name: &str) -> Option<String> {
        self.find(name).and_then(|s| lock(&s.last_error).clone())
    }

    /// How long until `name`'s next half-open attempt; `None` when the job is
    /// not tripped, or unknown.
    ///
    /// This is the one read here that consults the wall clock, because the one
    /// caller is `ea status` answering "when will this fix itself?" about the
    /// real world rather than about a test's notion of time.
    pub fn retry_in(&self, name: &str) -> Option<Duration> {
        let state = self.find(name)?;
        if !state.tripped.load(Ordering::SeqCst) {
            return None;
        }
        let wait = Self::wait_for(&state, true);
        let last = (*lock(&state.last_started))?;
        Some(wait.saturating_sub(Instant::now().saturating_duration_since(last)))
    }

    /// Clear a tripped breaker -- its failure count, its last error, its
    /// backed-off cooldown, and the schedule it was waiting on -- so the job
    /// runs on the next tick rather than at the end of the interval the
    /// failing attempt started. Returns `false` for an unknown job name, which
    /// is what lets the `resume` IPC method tell a human they typo'd rather
    /// than silently doing nothing.
    ///
    /// This is the manual half of breaker recovery. The automatic half is the
    /// half-open retry in [`Scheduler::maybe_spawn`]; a human should never
    /// *have* to run this, but when a credential has just been fixed, waiting
    /// out the cooldown is a needless hour.
    pub fn reset(&self, name: &str) -> bool {
        let Some(state) = self.find(name) else {
            return false;
        };
        state.tripped.store(false, Ordering::SeqCst);
        state.failure_count.store(0, Ordering::SeqCst);
        *lock(&state.last_error) = None;
        *lock(&state.cooldown) = self.breaker_cooldown;
        *lock(&state.last_started) = None;
        tracing::info!(job = name, "circuit breaker cleared by hand");
        true
    }

    fn find(&self, name: &str) -> Option<Arc<JobState>> {
        lock(&self.jobs)
            .iter()
            .find(|s| s.job.name == name)
            .cloned()
    }

    /// Check every registered job against `now` and spawn the ones that are
    /// due. Returns a handle per job actually started this tick -- empty
    /// while paused. Tests await these handles to observe a tick's effects
    /// deterministically instead of sleeping; `start` discards them, since a
    /// spawned job runs to completion on its own regardless of whether
    /// anything is still holding its handle.
    pub fn tick(&self, now: Instant) -> Vec<JoinHandle<()>> {
        if self.is_paused() {
            return Vec::new();
        }
        let jobs = lock(&self.jobs).clone();
        jobs.into_iter()
            .filter_map(|state| self.maybe_spawn(state, now))
            .collect()
    }

    /// How long this job waits between attempts: its own interval normally,
    /// and at least the breaker's current cooldown while it is tripped.
    ///
    /// `max` rather than a replacement, so a broken 30-minute connector is
    /// never polled *more* often than a healthy one -- the cooldown is a floor
    /// on how quickly a tripped job may be retried, not a new schedule.
    fn wait_for(state: &JobState, tripped: bool) -> Duration {
        if tripped {
            state.job.interval.max(*lock(&state.cooldown))
        } else {
            state.job.interval
        }
    }

    fn maybe_spawn(&self, state: Arc<JobState>, now: Instant) -> Option<JoinHandle<()>> {
        // Overlap guard: `compare_exchange` makes "is a run already in
        // flight, and if not, claim one" a single atomic step. Two ticks
        // racing on the same job can never both see `false` and proceed --
        // exactly the property the no-overlap test relies on.
        if state
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return None;
        }

        // From here on this tick holds exclusive claim on `state.running`
        // for this job, so reading and updating `last_started` needs no
        // separate synchronization against another tick doing the same.
        //
        // A tripped job is not skipped outright: it waits out its cooldown and
        // then gets exactly one attempt through this same path, which is what
        // makes the breaker half-open rather than a latch. The overlap guard
        // above is what keeps "exactly one" true.
        let tripped = state.tripped.load(Ordering::SeqCst);
        let wait = Self::wait_for(&state, tripped);
        let due = {
            let mut last = lock(&state.last_started);
            let due = last
                .map(|t| now.saturating_duration_since(t) >= wait)
                .unwrap_or(true);
            if due {
                *last = Some(now);
            }
            due
        };
        if !due {
            state.running.store(false, Ordering::SeqCst);
            return None;
        }
        if tripped {
            tracing::info!(
                job = %state.job.name,
                waited = ?wait,
                "circuit breaker half-open: retrying this job once"
            );
        }

        let threshold = self.breaker_threshold;
        let initial_cooldown = self.breaker_cooldown;
        let max_cooldown = self.max_breaker_cooldown;
        // Constructed here, on the calling thread, *before* `tokio::spawn` --
        // not as the first statement inside the spawned future. An `async
        // move` block captures its moved-in variables the instant the block
        // expression is evaluated, not on first poll, so by the time this
        // `guard` is moved into the block below it is already owned by the
        // future that `tokio::spawn` receives. If that future is ever
        // dropped without being polled (an `.abort()` or a runtime shutdown
        // landing before the executor gets to it), the future's own `Drop`
        // runs every captured value's destructor, `guard` included, and the
        // marker still clears. There is no longer any window between
        // "claimed" and "guarded": the compare_exchange above and this
        // construction happen back to back on the same thread with nothing
        // async in between.
        let guard = RunningGuard(state.running.clone());
        Some(tokio::spawn(async move {
            let _guard = guard;
            let fut = (state.job.run)();
            match AssertUnwindSafe(fut).catch_unwind().await {
                Ok(Ok(())) => Self::record_success(&state, initial_cooldown),
                // `{:#}` rather than `{}`: an `anyhow` error prints only its
                // outermost context by default, and the outermost context
                // here is something like "polling connector canvas", which is
                // a restatement of the job's name rather than a reason. The
                // alternate form appends the chain -- "...: 401 Unauthorized"
                // -- which is the half `ea status` exists to show.
                Ok(Err(err)) => {
                    Self::record_failure(&state, threshold, max_cooldown, format!("{err:#}"))
                }
                Err(panic) => {
                    Self::record_failure(&state, threshold, max_cooldown, panic_message(panic))
                }
            }
        }))
    }

    /// A success clears everything the breaker was holding, including a
    /// cooldown that had backed off. If this was the half-open attempt, the
    /// breaker closes here -- which is the whole automatic-recovery path, and
    /// it is worth a log line at `info`, because "the daemon started working
    /// again at 04:12" is exactly the thing an owner wants to find afterwards.
    fn record_success(state: &JobState, initial_cooldown: Duration) {
        state.failure_count.store(0, Ordering::SeqCst);
        *lock(&state.last_error) = None;
        *lock(&state.cooldown) = initial_cooldown;
        if state.tripped.swap(false, Ordering::SeqCst) {
            tracing::info!(
                job = %state.job.name,
                "circuit breaker closed: the job recovered on its own"
            );
        }
    }

    fn record_failure(state: &JobState, threshold: u32, max_cooldown: Duration, message: String) {
        let count = state.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
        *lock(&state.last_error) = Some(message);

        if state.tripped.load(Ordering::SeqCst) {
            // The half-open attempt failed: stay open, and wait longer next
            // time. Without the doubling, a connector that is down for a week
            // would be retried every five minutes for a week; with it, the
            // wait walks up to an hour and stays there.
            let mut cooldown = lock(&state.cooldown);
            let next = cooldown.saturating_mul(2).min(max_cooldown);
            let changed = next != *cooldown;
            *cooldown = next;
            drop(cooldown);
            if changed {
                tracing::warn!(
                    job = %state.job.name,
                    cooldown = ?next,
                    "the half-open retry failed; backing off further"
                );
            }
        } else if count >= threshold {
            state.tripped.store(true, Ordering::SeqCst);
            tracing::warn!(
                job = %state.job.name,
                failures = count,
                cooldown = ?*lock(&state.cooldown),
                "circuit breaker tripped; it will retry itself after the cooldown"
            );
        }
    }

    /// Spawn a background task that calls `tick` once per
    /// [`Self::TICK_GRANULARITY`] against the real clock, for the life of the
    /// process (or until the returned handle is aborted). This is the only
    /// place in the module that reads `Instant::now` -- everything else takes
    /// the clock as a parameter -- and the only sleep in the whole module:
    /// `tokio::time::interval` parks the task on the runtime's timer wheel
    /// between ticks rather than spinning, so this does not busy-loop.
    pub fn start(self: &Arc<Self>) -> JoinHandle<()> {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Self::TICK_GRANULARITY);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                interval.tick().await;
                let spawned = this.tick(Instant::now());
                this.retain(spawned);
            }
        })
    }

    /// Keep the handles this tick produced, dropping the ones that have
    /// already finished.
    fn retain(&self, spawned: Vec<JoinHandle<()>>) {
        let mut in_flight = lock(&self.in_flight);
        in_flight.retain(|handle| !handle.is_finished());
        in_flight.extend(spawned);
    }

    /// How many spawned invocations this scheduler is still holding a handle
    /// for. Approximate by nature -- a job that finished a microsecond ago is
    /// still counted until the next tick reaps it -- and used by tests and by
    /// the shutdown path's logging, never as a decision input.
    pub fn in_flight(&self) -> usize {
        lock(&self.in_flight)
            .iter()
            .filter(|handle| !handle.is_finished())
            .count()
    }

    /// Wait, for at most `timeout`, for every in-flight invocation to finish.
    /// Returns `true` if they all did.
    ///
    /// This is the shutdown drain. The caller is expected to [`pause`] first,
    /// so that no *new* invocation is spawned while this is waiting; pausing
    /// is what makes the bound meaningful rather than a race with the tick
    /// loop.
    ///
    /// Anything still running when the timeout expires is **not** aborted
    /// here: the handles are simply dropped, which detaches the tasks and lets
    /// the runtime's own shutdown deal with them. Dropping a `JoinHandle` does
    /// not cancel its task, so a job that is one statement from committing
    /// gets that statement; and because the overlap marker lives in a guard
    /// owned by the spawned future, nothing leaks either way.
    ///
    /// [`pause`]: Scheduler::pause
    pub async fn drain(&self, timeout: Duration) -> bool {
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *lock(&self.in_flight));
        if handles.is_empty() {
            return true;
        }
        tracing::info!(jobs = handles.len(), "draining in-flight scheduler jobs");
        let waited = tokio::time::timeout(timeout, async {
            for handle in handles {
                // A job that panicked is already recorded as a failure by
                // `maybe_spawn`; the join error is of no further interest.
                let _ = handle.await;
            }
        })
        .await;

        match waited {
            Ok(()) => true,
            Err(_elapsed) => {
                tracing::warn!(
                    ?timeout,
                    "scheduler jobs did not finish within the shutdown timeout"
                );
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A job whose body counts invocations, tracks concurrency, and can be
    /// told to fail, panic, or take a while.
    #[derive(Clone)]
    struct Probe {
        calls: Arc<AtomicUsize>,
        concurrency: Arc<AtomicUsize>,
        max_concurrency: Arc<AtomicUsize>,
    }

    impl Probe {
        fn new() -> Self {
            Probe {
                calls: Arc::new(AtomicUsize::new(0)),
                concurrency: Arc::new(AtomicUsize::new(0)),
                max_concurrency: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// A job that always succeeds immediately.
        fn ok_job(&self, name: &str, interval: Duration) -> Job {
            let probe = self.clone();
            Job::new(name, interval, move || {
                probe.calls.fetch_add(1, Ordering::SeqCst);
                async { Ok(()) }
            })
        }

        /// A job that always fails immediately.
        fn err_job(&self, name: &str, interval: Duration) -> Job {
            let probe = self.clone();
            Job::new(name, interval, move || {
                probe.calls.fetch_add(1, Ordering::SeqCst);
                async { Err(anyhow::anyhow!("boom")) }
            })
        }

        /// A job that panics synchronously, before ever reaching an await
        /// point.
        fn panicking_job(&self, name: &str, interval: Duration) -> Job {
            let probe = self.clone();
            Job::new(name, interval, move || {
                probe.calls.fetch_add(1, Ordering::SeqCst);
                #[allow(unreachable_code)]
                async {
                    panic!("job panicked on purpose");
                    #[allow(unused)]
                    Ok(())
                }
            })
        }

        /// A job that records how many copies of itself are running at
        /// once, then sleeps briefly before returning -- long enough for a
        /// second tick, fired before this one finishes, to observe it still
        /// in flight.
        fn slow_job(&self, name: &str, interval: Duration) -> Job {
            let probe = self.clone();
            Job::new(name, interval, move || {
                let probe = probe.clone();
                async move {
                    let now = probe.concurrency.fetch_add(1, Ordering::SeqCst) + 1;
                    probe.max_concurrency.fetch_max(now, Ordering::SeqCst);
                    probe.calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    probe.concurrency.fetch_sub(1, Ordering::SeqCst);
                    Ok(())
                }
            })
        }
    }

    async fn await_all(handles: Vec<JoinHandle<()>>) {
        for h in handles {
            h.await
                .expect("spawned job task should not panic past catch_unwind");
        }
    }

    #[tokio::test]
    async fn runs_on_first_tick() {
        let probe = Probe::new();
        let sched = Scheduler::new(3);
        sched.add(probe.ok_job("job", Duration::from_secs(60)));

        let handles = sched.tick(Instant::now());
        assert_eq!(handles.len(), 1);
        await_all(handles).await;

        assert_eq!(probe.calls(), 1);
    }

    #[tokio::test]
    async fn does_not_rerun_before_interval_elapses() {
        let probe = Probe::new();
        let sched = Scheduler::new(3);
        let interval = Duration::from_secs(60);
        sched.add(probe.ok_job("job", interval));

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert_eq!(probe.calls(), 1);

        let handles = sched.tick(t0 + interval / 2);
        assert!(
            handles.is_empty(),
            "job ran again before its interval elapsed"
        );
        assert_eq!(probe.calls(), 1);
    }

    #[tokio::test]
    async fn reruns_once_interval_elapses() {
        let probe = Probe::new();
        let sched = Scheduler::new(3);
        let interval = Duration::from_secs(60);
        sched.add(probe.ok_job("job", interval));

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert_eq!(probe.calls(), 1);

        await_all(sched.tick(t0 + interval)).await;
        assert_eq!(probe.calls(), 2);
    }

    #[tokio::test]
    async fn one_job_failing_does_not_stop_another() {
        let failing = Probe::new();
        let healthy = Probe::new();
        let sched = Scheduler::new(3);
        sched.add(failing.err_job("failing", Duration::from_secs(60)));
        sched.add(healthy.ok_job("healthy", Duration::from_secs(60)));

        await_all(sched.tick(Instant::now())).await;

        assert_eq!(failing.calls(), 1);
        assert_eq!(healthy.calls(), 1);
        assert!(!sched.is_tripped("healthy"));
    }

    #[tokio::test]
    async fn breaker_trips_after_n_failures_and_stops_the_job() {
        let probe = Probe::new();
        let sched = Scheduler::new(2);
        let interval = Duration::from_secs(1);
        sched.add(probe.err_job("job", interval));

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert!(!sched.is_tripped("job"), "should not trip before threshold");

        await_all(sched.tick(t0 + interval)).await;
        assert!(sched.is_tripped("job"), "should trip at threshold");
        assert_eq!(probe.calls(), 2);
        assert_eq!(sched.last_error("job").as_deref(), Some("boom"));

        // Further ticks must not call the job at all once tripped.
        let handles = sched.tick(t0 + interval * 3);
        assert!(handles.is_empty());
        assert_eq!(probe.calls(), 2);
    }

    #[tokio::test]
    async fn reset_clears_a_tripped_breaker() {
        let probe = Probe::new();
        let sched = Scheduler::new(1);
        let interval = Duration::from_secs(1);
        sched.add(probe.err_job("job", interval));

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert!(sched.is_tripped("job"));

        sched.reset("job");
        assert!(!sched.is_tripped("job"));
        assert_eq!(sched.last_error("job"), None);

        await_all(sched.tick(t0 + interval)).await;
        assert_eq!(probe.calls(), 2, "job should run again after reset");
    }

    #[tokio::test]
    async fn a_success_resets_the_failure_count() {
        let sched = Scheduler::new(2);
        let interval = Duration::from_secs(1);

        // Fails, then succeeds, then fails once more: with the count reset
        // by the success in between, a single trailing failure must not
        // trip a threshold of 2. Without the reset this would be the second
        // consecutive failure and would trip.
        let outcomes = Arc::new(Mutex::new(vec![anyhow::anyhow!("first")]));
        let calls = Arc::new(AtomicUsize::new(0));
        let calls2 = calls.clone();
        let job = Job::new("job", interval, move || {
            let call = calls2.fetch_add(1, Ordering::SeqCst);
            let outcomes = outcomes.clone();
            async move {
                match call {
                    0 => Err(outcomes.lock().unwrap().remove(0)),
                    1 => Ok(()),
                    _ => Err(anyhow::anyhow!("later failure")),
                }
            }
        });
        sched.add(job);

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await; // fails: count = 1
        assert!(!sched.is_tripped("job"));

        await_all(sched.tick(t0 + interval)).await; // succeeds: count -> 0
        assert!(!sched.is_tripped("job"));

        await_all(sched.tick(t0 + interval * 2)).await; // fails: count = 1
        assert!(
            !sched.is_tripped("job"),
            "the intervening success should have reset the failure count"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn nothing_runs_while_paused_and_resumes_after() {
        let probe = Probe::new();
        let sched = Scheduler::new(3);
        sched.add(probe.ok_job("job", Duration::from_secs(60)));

        sched.pause();
        assert!(sched.is_paused());
        let handles = sched.tick(Instant::now());
        assert!(handles.is_empty());
        assert_eq!(probe.calls(), 0);

        sched.resume();
        assert!(!sched.is_paused());
        await_all(sched.tick(Instant::now())).await;
        assert_eq!(probe.calls(), 1);
    }

    #[tokio::test]
    async fn a_job_never_overlaps_itself() {
        let probe = Probe::new();
        let sched = Scheduler::new(3);
        // An interval far shorter than the job's own 30ms run time, so the
        // job is "due" again long before the first call finishes.
        sched.add(probe.slow_job("job", Duration::from_millis(1)));

        let t0 = Instant::now();
        let first = sched.tick(t0);
        assert_eq!(first.len(), 1, "first tick should start the job");

        // Fired before the first invocation's 30ms sleep has elapsed, at a
        // time the interval has technically elapsed by -- this must still be
        // refused because the first invocation is still in flight.
        let second = sched.tick(t0 + Duration::from_millis(5));
        assert!(
            second.is_empty(),
            "a still-running job must not be started again"
        );

        await_all(first).await;
        await_all(second).await;

        assert_eq!(probe.calls(), 1);
        assert_eq!(probe.max_concurrency.load(Ordering::SeqCst), 1);

        // Now that it has finished, a later tick must be able to run it
        // again -- proving the running flag was cleared, not left stuck.
        await_all(sched.tick(t0 + Duration::from_millis(10))).await;
        assert_eq!(probe.calls(), 2);
    }

    #[tokio::test]
    async fn a_panicking_job_is_contained_and_recorded_as_a_failure() {
        let probe = Probe::new();
        let sched = Scheduler::new(1);
        sched.add(probe.panicking_job("job", Duration::from_secs(60)));

        // The panic happens inside the spawned task, past `catch_unwind`;
        // the task itself must complete normally (not propagate the panic
        // to its `JoinHandle`), which `await_all`'s `.expect` would catch.
        await_all(sched.tick(Instant::now())).await;

        assert_eq!(probe.calls(), 1);
        assert!(sched.is_tripped("job"), "a panic must count as a failure");
        assert_eq!(
            sched.last_error("job").as_deref(),
            Some("job panicked on purpose")
        );

        // The running marker must have cleared despite the panic, or the
        // job would be stuck forever even after the breaker is reset.
        sched.reset("job");
        await_all(sched.tick(Instant::now() + Duration::from_secs(120))).await;
        assert_eq!(probe.calls(), 2);
    }

    #[tokio::test]
    async fn a_dropped_job_future_clears_the_running_marker() {
        // Simulates a job whose task is aborted mid-run (e.g. runtime
        // shutdown): the spawned task -- and with it the `RunningGuard` --
        // is dropped before the job's own future ever completes. The
        // running marker must still clear, or the job would never run
        // again.
        let sched = Arc::new(Scheduler::new(3));
        let started = Arc::new(tokio::sync::Notify::new());
        let started2 = started.clone();
        let job = Job::new("job", Duration::from_millis(1), move || {
            let started = started2.clone();
            async move {
                started.notify_one();
                // Long enough to still be "in flight" when we abort it below.
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(())
            }
        });
        sched.add(job);

        let t0 = Instant::now();
        let handles = sched.tick(t0);
        assert_eq!(handles.len(), 1);
        started.notified().await;

        for h in handles {
            h.abort();
        }

        // Poll until the running flag clears -- bounded, not a fixed sleep:
        // the guard's `Drop` runs as part of the task unwinding from the
        // abort, which is scheduled but not synchronous with `.abort()`
        // returning.
        for _ in 0..200 {
            if sched.tick(t0 + Duration::from_secs(1)).len() == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("running marker was never cleared after the job's task was aborted");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_job_aborted_before_its_first_poll_clears_the_running_marker() {
        // Regression test for the leak the reviewer found: `RunningGuard`
        // used to be the first statement *inside* the spawned future, so it
        // was only constructed on that future's first poll. Aborting (or
        // dropping) a `JoinHandle` before the task was ever polled dropped
        // the future without running any of its body -- `RunningGuard` was
        // never constructed, and the marker stayed `true` forever. This is
        // distinct from `a_dropped_job_future_clears_the_running_marker`
        // above, which only proves the drop-*after*-first-poll case (it
        // waits on `started.notify_one()`, which fires after the guard
        // already exists).
        //
        // Determinism: this test runs on a *current-thread* runtime
        // (`flavor = "current_thread"`, matching this crate's default, made
        // explicit here since it's load-bearing for the test), and there is
        // no `.await` anywhere between `sched.tick` (which calls
        // `tokio::spawn`) and `h.abort()`. A current-thread runtime only
        // polls a newly spawned task when the driving task yields control
        // back to the executor -- at an await point, or by returning -- and
        // this test does neither between spawning and aborting, so the
        // spawned task cannot have been polled even once by the time
        // `.abort()` runs. This is corroborated directly, not just argued:
        // the job body's very first statement increments `probe.calls()`,
        // so if the future had been polled at all -- even partially --
        // `calls()` would already read 1 by the time we check it below. It
        // reads 0, which is direct evidence the abort landed pre-poll, not
        // just a timing assumption.
        let probe = Probe::new();
        let sched = Scheduler::new(3);
        sched.add(probe.ok_job("job", Duration::from_millis(1)));

        let t0 = Instant::now();
        let handles = sched.tick(t0);
        assert_eq!(handles.len(), 1, "first tick should start the job");
        for h in handles {
            h.abort();
        }

        assert_eq!(
            probe.calls(),
            0,
            "the job's body must not have run yet -- this abort should land before first poll"
        );

        // With the leak, `RunningGuard` was never constructed for this
        // invocation, so `running` would stay `true` forever and every
        // subsequent tick would find nothing to spawn. With the fix, the
        // guard was moved into the future before `tokio::spawn` ever saw it,
        // so dropping the unpolled future still runs the guard's destructor
        // and clears the marker -- but that drop is carried out by the
        // runtime, not by `.abort()` returning, so it needs at least one
        // yield back to the executor to actually happen. Poll for it with a
        // bounded loop rather than a fixed sleep (the same pattern the
        // post-poll drop test above uses), so this doesn't assume how many
        // scheduler turns the cleanup takes -- only that it eventually does.
        for _ in 0..200 {
            let handles = sched.tick(t0 + Duration::from_secs(1));
            if !handles.is_empty() {
                await_all(handles).await;
                assert_eq!(
                    probe.calls(),
                    1,
                    "job must run again after being aborted pre-poll"
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("job never became runnable again after being aborted before its first poll");
    }
    // -- the shutdown drain (Task 13, Addition 2) --------------------------

    /// A job that records whether it ran to completion, so a drain can be
    /// distinguished from an abort: an aborted future never reaches the flag.
    fn completing_job(name: &str, interval: Duration, delay: Duration) -> (Job, Arc<AtomicBool>) {
        let done = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&done);
        let job = Job::new(name, interval, move || {
            let flag = Arc::clone(&flag);
            async move {
                tokio::time::sleep(delay).await;
                flag.store(true, Ordering::SeqCst);
                Ok(())
            }
        });
        (job, done)
    }

    /// The point of retaining handles at all: SIGTERM must not cut a
    /// `watch_poll` off halfway through writing events.
    #[tokio::test]
    async fn drain_waits_for_a_job_that_is_still_running() {
        let sched = Arc::new(Scheduler::new(3));
        let (job, done) =
            completing_job("slow", Duration::from_secs(60), Duration::from_millis(80));
        sched.add(job);

        let spawned = sched.tick(Instant::now());
        assert_eq!(spawned.len(), 1);
        sched.retain(spawned);
        assert_eq!(
            sched.in_flight(),
            1,
            "the handle must be retained, not dropped"
        );
        assert!(
            !done.load(Ordering::SeqCst),
            "the job cannot have finished yet"
        );

        assert!(
            sched.drain(Duration::from_secs(5)).await,
            "drain must succeed"
        );
        assert!(
            done.load(Ordering::SeqCst),
            "a drained job must have run to completion, not been cut off"
        );
        assert_eq!(sched.in_flight(), 0);
    }

    /// The bound is real: a job that will not finish must not hold shutdown
    /// open forever.
    #[tokio::test]
    async fn drain_gives_up_after_its_timeout() {
        let sched = Arc::new(Scheduler::new(3));
        let (job, done) = completing_job("stuck", Duration::from_secs(60), Duration::from_secs(30));
        sched.add(job);
        sched.retain(sched.tick(Instant::now()));

        let started = Instant::now();
        assert!(
            !sched.drain(Duration::from_millis(50)).await,
            "drain must report that it gave up"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "and must not block"
        );
        assert!(!done.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn draining_with_nothing_in_flight_returns_immediately() {
        let sched = Arc::new(Scheduler::new(3));
        assert!(sched.drain(Duration::from_millis(1)).await);
    }

    /// Pausing before draining is what makes the bound meaningful: no new
    /// invocation may be spawned while shutdown is waiting.
    #[tokio::test]
    async fn a_paused_scheduler_spawns_nothing_more_while_draining() {
        let sched = Arc::new(Scheduler::new(3));
        let probe = Probe::new();
        sched.add(probe.ok_job("poll", Duration::from_millis(1)));
        sched.retain(sched.tick(Instant::now()));
        assert!(sched.drain(Duration::from_secs(5)).await);
        assert_eq!(probe.calls(), 1);

        sched.pause();
        assert!(sched
            .tick(Instant::now() + Duration::from_secs(10))
            .is_empty());
        assert_eq!(probe.calls(), 1, "a paused scheduler must spawn nothing");
    }

    /// Retained handles must not accumulate for the life of the process.
    #[tokio::test]
    async fn finished_handles_are_reaped_rather_than_accumulating() {
        let sched = Arc::new(Scheduler::new(3));
        let probe = Probe::new();
        sched.add(probe.ok_job("poll", Duration::from_millis(0)));

        for i in 0..5 {
            sched.retain(sched.tick(Instant::now() + Duration::from_secs(i)));
            // The job body is instantaneous; a couple of yields is enough for
            // the spawned task to run and for `is_finished` to say so.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
        }
        // One more tick to reap whatever the last round left behind.
        sched.retain(sched.tick(Instant::now() + Duration::from_secs(10)));
        assert!(
            sched.in_flight() <= 1,
            "finished handles must be reaped, found {}",
            sched.in_flight()
        );
    }

    /// `ea status` shows this string. It has to be the reason, not the label:
    /// an `anyhow` error's default `Display` prints only its outermost
    /// context, which for a connector poll is "polling connector canvas" —
    /// a restatement of the job name.
    #[tokio::test]
    async fn the_recorded_failure_carries_the_whole_error_chain() {
        let sched = Arc::new(Scheduler::new(1));
        sched.add(Job::new("canvas", Duration::from_secs(60), || async {
            Err(anyhow::anyhow!("401 Unauthorized").context("polling connector canvas"))
        }));
        await_all(sched.tick(Instant::now())).await;

        let recorded = sched
            .last_error("canvas")
            .expect("a failure must be recorded");
        assert!(recorded.contains("polling connector canvas"), "{recorded}");
        assert!(
            recorded.contains("401 Unauthorized"),
            "the reason must survive: {recorded}"
        );
        assert!(sched.is_tripped("canvas"));
    }

    // -- half-open recovery -------------------------------------------------

    /// A job whose failures can be switched off part-way through, so one test
    /// can drive "broken, then fixed" without two schedulers.
    fn flaky_job(name: &str, interval: Duration) -> (Job, Arc<AtomicBool>, Arc<AtomicUsize>) {
        let healthy = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let h = healthy.clone();
        let c = calls.clone();
        let job = Job::new(name, interval, move || {
            c.fetch_add(1, Ordering::SeqCst);
            let healthy = h.load(Ordering::SeqCst);
            async move {
                if healthy {
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("still broken"))
                }
            }
        });
        (job, healthy, calls)
    }

    /// The finding, stated as a test: five failures disable the job, and the
    /// daemon is left running for weeks with that job dead. It must retry
    /// itself.
    #[tokio::test]
    async fn a_tripped_breaker_retries_itself_after_the_cooldown() {
        let cooldown = Duration::from_secs(300);
        let sched = Scheduler::with_cooldown(1, cooldown, cooldown * 8);
        let interval = Duration::from_secs(60);
        let (job, _healthy, calls) = flaky_job("canvas", interval);
        sched.add(job);

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert!(sched.is_tripped("canvas"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Inside the cooldown: nothing, however many ticks arrive.
        for secs in [60, 120, 299] {
            assert!(sched.tick(t0 + Duration::from_secs(secs)).is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // Past it: exactly one attempt.
        await_all(sched.tick(t0 + cooldown)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "one half-open attempt");
        assert!(sched.is_tripped("canvas"), "it failed, so it stays open");
    }

    #[tokio::test]
    async fn a_successful_half_open_attempt_closes_the_breaker() {
        let cooldown = Duration::from_secs(300);
        let sched = Scheduler::with_cooldown(1, cooldown, cooldown * 8);
        let interval = Duration::from_secs(60);
        let (job, healthy, calls) = flaky_job("canvas", interval);
        sched.add(job);

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert!(sched.is_tripped("canvas"));

        // The outage ends.
        healthy.store(true, Ordering::SeqCst);
        await_all(sched.tick(t0 + cooldown)).await;

        assert!(!sched.is_tripped("canvas"), "a good probe must close it");
        assert_eq!(sched.last_error("canvas"), None);
        assert_eq!(sched.retry_in("canvas"), None);

        // And it is back on its own interval, not the cooldown.
        await_all(sched.tick(t0 + cooldown + interval)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_failed_half_open_attempt_backs_the_cooldown_off_to_the_cap() {
        let cooldown = Duration::from_secs(300);
        let max = Duration::from_secs(1200);
        let sched = Scheduler::with_cooldown(1, cooldown, max);
        let (job, _healthy, calls) = flaky_job("canvas", Duration::from_secs(60));
        sched.add(job);

        let mut at = Instant::now();
        await_all(sched.tick(at)).await;
        assert!(sched.is_tripped("canvas"));

        // 300s, then 600s, then 1200s, then 1200s again: doubling, capped.
        for expected in [300u64, 600, 1200, 1200] {
            let wait = Duration::from_secs(expected);
            assert!(
                sched.tick(at + wait - Duration::from_secs(1)).is_empty(),
                "must still be waiting {expected}s in"
            );
            at += wait;
            await_all(sched.tick(at)).await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 5, "one probe per cooldown");
    }

    /// The cooldown is a floor on retries, not a schedule: a connector that
    /// polls every 30 minutes must not be polled every 5 while it is broken.
    #[tokio::test]
    async fn a_job_slower_than_the_cooldown_keeps_its_own_interval() {
        let sched = Scheduler::with_cooldown(1, Duration::from_secs(300), Duration::from_secs(600));
        let interval = Duration::from_secs(1800);
        let (job, _healthy, calls) = flaky_job("canvas", interval);
        sched.add(job);

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert!(sched.tick(t0 + Duration::from_secs(300)).is_empty());
        assert!(sched.tick(t0 + Duration::from_secs(1799)).is_empty());
        await_all(sched.tick(t0 + interval)).await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reset_answers_whether_the_job_exists_and_makes_it_due_at_once() {
        let probe = Probe::new();
        let sched = Scheduler::new(1);
        // An interval far longer than anything this test waits: without
        // `reset` clearing the schedule too, the job would not be due again
        // for an hour and `ea resume canvas` would look like it did nothing.
        sched.add(probe.err_job("canvas", Duration::from_secs(3600)));

        let t0 = Instant::now();
        await_all(sched.tick(t0)).await;
        assert!(sched.is_tripped("canvas"));
        assert!(sched.retry_in("canvas").is_some());

        assert!(!sched.reset("nosuchjob"), "an unknown job must say so");
        assert!(sched.reset("canvas"));
        assert!(!sched.is_tripped("canvas"));
        assert_eq!(sched.retry_in("canvas"), None);

        await_all(sched.tick(t0 + Duration::from_secs(1))).await;
        assert_eq!(probe.calls(), 2, "resume must not wait out the interval");
    }
}
