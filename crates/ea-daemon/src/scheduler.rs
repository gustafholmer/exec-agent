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
    /// is skipped by every subsequent `tick` until `reset`.
    tripped: AtomicBool,
    /// The message from the most recent failure, cleared on the next
    /// success or on `reset`. This is what makes a tripped breaker visible
    /// to `ea status` instead of just a name with no explanation.
    last_error: Mutex<Option<String>>,
}

impl JobState {
    fn new(job: Job) -> Self {
        JobState {
            job,
            last_started: Mutex::new(None),
            running: Arc::new(AtomicBool::new(false)),
            failure_count: AtomicU32::new(0),
            tripped: AtomicBool::new(false),
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
    jobs: Mutex<Vec<Arc<JobState>>>,
    paused: AtomicBool,
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
        Scheduler {
            breaker_threshold,
            jobs: Mutex::new(Vec::new()),
            paused: AtomicBool::new(false),
        }
    }

    /// Register a job. Jobs are kept in the order they were added; `names`
    /// reflects that order.
    pub fn add(&self, job: Job) {
        lock(&self.jobs).push(Arc::new(JobState::new(job)));
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

    /// Clear a tripped breaker (and its failure count and last error) so the
    /// job is eligible to run again on the next tick it is due.
    pub fn reset(&self, name: &str) {
        if let Some(state) = self.find(name) {
            state.tripped.store(false, Ordering::SeqCst);
            state.failure_count.store(0, Ordering::SeqCst);
            *lock(&state.last_error) = None;
        }
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

    fn maybe_spawn(&self, state: Arc<JobState>, now: Instant) -> Option<JoinHandle<()>> {
        if state.tripped.load(Ordering::SeqCst) {
            return None;
        }

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
        let due = {
            let mut last = lock(&state.last_started);
            let due = last
                .map(|t| now.saturating_duration_since(t) >= state.job.interval)
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

        let threshold = self.breaker_threshold;
        Some(tokio::spawn(async move {
            let _guard = RunningGuard(state.running.clone());
            let fut = (state.job.run)();
            match AssertUnwindSafe(fut).catch_unwind().await {
                Ok(Ok(())) => {
                    state.failure_count.store(0, Ordering::SeqCst);
                    *lock(&state.last_error) = None;
                }
                Ok(Err(err)) => Self::record_failure(&state, threshold, err.to_string()),
                Err(panic) => Self::record_failure(&state, threshold, panic_message(panic)),
            }
        }))
    }

    fn record_failure(state: &JobState, threshold: u32, message: String) {
        let count = state.failure_count.fetch_add(1, Ordering::SeqCst) + 1;
        *lock(&state.last_error) = Some(message);
        if count >= threshold {
            state.tripped.store(true, Ordering::SeqCst);
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
                let _ = this.tick(Instant::now());
            }
        })
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
}
