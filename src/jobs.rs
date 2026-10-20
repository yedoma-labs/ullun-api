//! In-process job queue with retries
//!
//! Work that must not block a request, may fail, and has to be tried again:
//! sending webhooks, e-mails, thumbnails, imports. Jobs are persisted if you
//! give the queue a file, retried with exponential backoff, and parked in a
//! dead-letter list when they keep failing so that nothing disappears silently.
//!
//! ```no_run
//! use std::time::Duration;
//! use ullun::jobs::{JobError, JobQueue, QueueConfig};
//! use ullun::prelude::*;
//!
//! #[derive(Clone, serde::Serialize, serde::Deserialize)]
//! struct SendMail { to: String }
//!
//! # async fn run() {
//! let queue: JobQueue<SendMail> = JobQueue::new(QueueConfig::new().max_attempts(5));
//! let worker_queue = queue.clone();
//!
//! App::new()
//!     .state(queue)
//!     .post("/signup", |req: Request| async move {
//!         req.state::<JobQueue<SendMail>>()?.enqueue(SendMail { to: "ann@example.com".into() });
//!         Ok(Response::text("welcome"))        // the mail is sent later, with retries
//!     })
//!     .background(move |ctx: Context| async move {
//!         worker_queue
//!             .run(ctx.shutdown(), 4, |job| async move {
//!                 // job.attempt is 1 on the first try
//!                 send(&job.payload.to).await.map_err(|e| JobError::retry(e.to_string()))
//!             })
//!             .await;
//!     })
//!     .run("127.0.0.1:3000")
//!     .await
//!     .unwrap();
//! # }
//! # async fn send(_to: &str) -> std::result::Result<(), std::io::Error> { Ok(()) }
//! ```
//!
//! **Delivery is at-least-once**: a job that was running when the process died
//! is run again after a restart, so handlers must be safe to repeat (use the
//! job id or your own key as an idempotency key towards other systems). The
//! queue lives in one process; for several instances use a database-backed queue.

use crate::shutdown::Shutdown;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Notify;

pub type JobId = u64;

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Queue behaviour
#[derive(Clone)]
pub struct QueueConfig {
    max_attempts: u32,
    base_delay: Duration,
    max_delay: Duration,
    jitter: bool,
    persist_path: Option<PathBuf>,
    dead_letter_limit: usize,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl QueueConfig {
    /// 5 attempts, backoff 1 s doubling up to 1 h with +-20 % jitter, nothing
    /// persisted, the last 1000 dead jobs kept
    pub fn new() -> Self {
        Self {
            max_attempts: 5,
            base_delay: Duration::from_secs(1),
            max_delay: Duration::from_secs(3600),
            jitter: true,
            persist_path: None,
            dead_letter_limit: 1000,
            clock: Arc::new(system_now_ms),
        }
    }

    /// Total tries per job including the first (at least 1)
    pub fn max_attempts(mut self, n: u32) -> Self {
        self.max_attempts = n.max(1);
        self
    }

    /// Delay before the first retry; it doubles with every failed attempt
    pub fn base_delay(mut self, d: Duration) -> Self {
        self.base_delay = d;
        self
    }

    pub fn max_delay(mut self, d: Duration) -> Self {
        self.max_delay = d;
        self
    }

    /// Spread retries by +-20 % so that jobs failing together do not retry together
    pub fn jitter(mut self, on: bool) -> Self {
        self.jitter = on;
        self
    }

    /// Keep the queue in this JSON file (written atomically after changes)
    pub fn persist_to(mut self, path: impl Into<PathBuf>) -> Self {
        self.persist_path = Some(path.into());
        self
    }

    /// How many dead jobs to remember (the oldest are dropped)
    pub fn dead_letter_limit(mut self, n: usize) -> Self {
        self.dead_letter_limit = n;
        self
    }

    /// Replace the time source, in milliseconds since the Unix epoch (tests)
    pub fn clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    /// Delay before retry number `attempt` (1 = first retry), without jitter
    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32
            .checked_shl(attempt.saturating_sub(1).min(30))
            .unwrap_or(u32::MAX);
        self.base_delay.saturating_mul(factor).min(self.max_delay)
    }
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// How a handler reports failure
#[derive(Debug, Clone)]
pub struct JobError {
    message: String,
    kind: ErrorKind,
}

#[derive(Debug, Clone)]
enum ErrorKind {
    Retry(Option<Duration>),
    Fatal,
}

impl JobError {
    /// Try again later, after the queue's backoff
    pub fn retry(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Retry(None),
        }
    }

    /// Try again after exactly `after` (e.g. a `Retry-After` the other side sent)
    pub fn retry_after(message: impl Into<String>, after: Duration) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Retry(Some(after)),
        }
    }

    /// Do not retry: the job goes straight to the dead-letter list
    pub fn fatal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: ErrorKind::Fatal,
        }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<String> for JobError {
    fn from(message: String) -> Self {
        Self::retry(message)
    }
}

impl From<&str> for JobError {
    fn from(message: &str) -> Self {
        Self::retry(message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    Dead,
}

/// A job as stored by the queue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobRecord<J> {
    pub id: JobId,
    pub payload: J,
    /// Failed attempts so far
    pub attempts: u32,
    pub state: JobState,
    pub run_at_ms: u64,
    pub created_at_ms: u64,
    pub last_error: Option<String>,
    pub key: Option<String>,
}

/// What a handler receives
#[derive(Debug, Clone)]
pub struct Job<J> {
    pub id: JobId,
    pub payload: J,
    /// 1 on the first try, 2 on the first retry, ...
    pub attempt: u32,
    pub key: Option<String>,
}

/// Optional settings for [`JobQueue::enqueue_with`]
#[derive(Debug, Clone, Default)]
pub struct EnqueueOptions {
    /// Do not run before this much time has passed
    pub delay: Duration,
    /// While a job with this key is waiting or running, enqueueing it again
    /// returns the existing job instead of adding a duplicate
    pub key: Option<String>,
}

/// Result of [`JobQueue::enqueue_with`]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    pub id: JobId,
    /// An existing job with the same key was returned
    pub duplicate: bool,
}

/// Counters for dashboards and tests
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Stats {
    pub pending: usize,
    pub running: usize,
    pub dead: usize,
    /// Jobs finished successfully since start
    pub completed: u64,
    /// Failed attempts that were scheduled for another try since start
    pub retried: u64,
}

struct State<J> {
    next_id: JobId,
    jobs: HashMap<JobId, JobRecord<J>>,
    /// Pending jobs ordered by when they are due
    due: BTreeSet<(u64, JobId)>,
    keys: HashMap<String, JobId>,
    completed: u64,
    retried: u64,
}

struct Inner<J> {
    state: Mutex<State<J>>,
    wake: Notify,
    dirty: Notify,
    config: QueueConfig,
}

/// The queue; cheap to clone, all clones are the same queue
pub struct JobQueue<J> {
    inner: Arc<Inner<J>>,
}

impl<J> Clone for JobQueue<J> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Snapshot<J> {
    next_id: JobId,
    jobs: Vec<JobRecord<J>>,
}

impl<J> JobQueue<J>
where
    J: Serialize + DeserializeOwned + Clone + Send + Sync + 'static,
{
    /// An empty queue, or the one saved in `persist_to`'s file. Jobs that were
    /// running when the file was written are pending again.
    pub fn new(config: QueueConfig) -> Self {
        let mut state = State {
            next_id: 1,
            jobs: HashMap::new(),
            due: BTreeSet::new(),
            keys: HashMap::new(),
            completed: 0,
            retried: 0,
        };
        if let Some(path) = &config.persist_path {
            match std::fs::read(path) {
                Ok(bytes) => match serde_json::from_slice::<Snapshot<J>>(&bytes) {
                    Ok(snapshot) => {
                        state.next_id = snapshot.next_id.max(1);
                        for mut job in snapshot.jobs {
                            if job.state == JobState::Running {
                                job.state = JobState::Pending;
                            }
                            if job.state == JobState::Pending {
                                state.due.insert((job.run_at_ms, job.id));
                                if let Some(k) = &job.key {
                                    state.keys.insert(k.clone(), job.id);
                                }
                            }
                            state.next_id = state.next_id.max(job.id + 1);
                            state.jobs.insert(job.id, job);
                        }
                    }
                    Err(e) => eprintln!("job queue: ignoring unreadable {}: {}", path.display(), e),
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => eprintln!("job queue: cannot read {}: {}", path.display(), e),
            }
        }
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(state),
                wake: Notify::new(),
                dirty: Notify::new(),
                config,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<J>> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn now_ms(&self) -> u64 {
        (self.inner.config.clock)()
    }

    fn changed(&self) {
        self.inner.dirty.notify_one();
        self.inner.wake.notify_waiters();
    }

    /// Add a job to run as soon as a worker is free
    pub fn enqueue(&self, payload: J) -> JobId {
        self.enqueue_with(payload, EnqueueOptions::default()).id
    }

    pub fn enqueue_with(&self, payload: J, options: EnqueueOptions) -> Enqueued {
        let now = self.now_ms();
        let enqueued = {
            let mut st = self.lock();
            if let Some(existing) = options.key.as_ref().and_then(|k| st.keys.get(k)).copied() {
                return Enqueued {
                    id: existing,
                    duplicate: true,
                };
            }
            let id = st.next_id;
            st.next_id += 1;
            let run_at_ms = now + options.delay.as_millis() as u64;
            if let Some(k) = &options.key {
                st.keys.insert(k.clone(), id);
            }
            st.due.insert((run_at_ms, id));
            st.jobs.insert(
                id,
                JobRecord {
                    id,
                    payload,
                    attempts: 0,
                    state: JobState::Pending,
                    run_at_ms,
                    created_at_ms: now,
                    last_error: None,
                    key: options.key,
                },
            );
            Enqueued {
                id,
                duplicate: false,
            }
        };
        self.changed();
        enqueued
    }

    /// Counters
    pub fn stats(&self) -> Stats {
        let st = self.lock();
        let mut stats = Stats {
            completed: st.completed,
            retried: st.retried,
            ..Stats::default()
        };
        for job in st.jobs.values() {
            match job.state {
                JobState::Pending => stats.pending += 1,
                JobState::Running => stats.running += 1,
                JobState::Dead => stats.dead += 1,
            }
        }
        stats
    }

    /// A job that is waiting, running or dead
    pub fn get(&self, id: JobId) -> Option<JobRecord<J>> {
        self.lock().jobs.get(&id).cloned()
    }

    /// Jobs that ran out of attempts (or failed fatally), oldest first
    pub fn dead_jobs(&self) -> Vec<JobRecord<J>> {
        let st = self.lock();
        let mut dead: Vec<_> = st
            .jobs
            .values()
            .filter(|j| j.state == JobState::Dead)
            .cloned()
            .collect();
        dead.sort_by_key(|j| j.id);
        dead
    }

    /// Give a dead job a fresh start; `false` if there is no such dead job
    pub fn retry_dead(&self, id: JobId) -> bool {
        let now = self.now_ms();
        let revived = {
            let mut st = self.lock();
            let State {
                jobs, due, keys, ..
            } = &mut *st;
            match jobs.get_mut(&id) {
                Some(job) if job.state == JobState::Dead => {
                    job.state = JobState::Pending;
                    job.attempts = 0;
                    job.run_at_ms = now;
                    due.insert((now, id));
                    if let Some(k) = &job.key {
                        keys.insert(k.clone(), id);
                    }
                    true
                }
                _ => false,
            }
        };
        if revived {
            self.changed();
        }
        revived
    }

    /// Remove a dead job for good; `false` if there is no such dead job
    pub fn delete_dead(&self, id: JobId) -> bool {
        let removed = {
            let mut st = self.lock();
            if st.jobs.get(&id).is_some_and(|j| j.state == JobState::Dead) {
                st.jobs.remove(&id);
                true
            } else {
                false
            }
        };
        if removed {
            self.changed();
        }
        removed
    }

    /// Claim the next due job, if any
    fn take_due(&self) -> Result<Job<J>, Option<u64>> {
        let now = self.now_ms();
        let mut st = self.lock();
        let Some(&(run_at, id)) = st.due.first() else {
            return Err(None);
        };
        if run_at > now {
            return Err(Some(run_at - now));
        }
        st.due.remove(&(run_at, id));
        let job = st.jobs.get_mut(&id).expect("due jobs exist");
        job.state = JobState::Running;
        Ok(Job {
            id,
            payload: job.payload.clone(),
            attempt: job.attempts + 1,
            key: job.key.clone(),
        })
    }

    fn jitter(&self, delay: Duration) -> Duration {
        if !self.inner.config.jitter {
            return delay;
        }
        // uniformly within +-20 %
        let r = crate::security::random_bytes(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .unwrap_or(32768);
        let factor = 0.8 + (f64::from(r) / f64::from(u16::MAX)) * 0.4;
        delay.mul_f64(factor)
    }

    fn finish(&self, id: JobId, result: Result<(), JobError>) {
        let cfg = &self.inner.config;
        let now = self.now_ms();
        {
            let mut st = self.lock();
            let State {
                jobs,
                due,
                keys,
                completed,
                retried,
                ..
            } = &mut *st;
            let Some(job) = jobs.get_mut(&id) else { return };
            match result {
                Ok(()) => {
                    if let Some(k) = &job.key {
                        keys.remove(k);
                    }
                    jobs.remove(&id);
                    *completed += 1;
                }
                Err(err) => {
                    job.attempts += 1;
                    job.last_error = Some(err.message.clone());
                    let give_up =
                        matches!(err.kind, ErrorKind::Fatal) || job.attempts >= cfg.max_attempts;
                    if give_up {
                        job.state = JobState::Dead;
                        if let Some(k) = &job.key {
                            keys.remove(k);
                        }
                    } else {
                        let delay = match err.kind {
                            ErrorKind::Retry(Some(after)) => after,
                            _ => self.jitter(cfg.backoff(job.attempts)),
                        };
                        job.state = JobState::Pending;
                        job.run_at_ms = now + delay.as_millis() as u64;
                        due.insert((job.run_at_ms, id));
                        *retried += 1;
                    }
                }
            }
            // keep only the most recent dead jobs
            let dead = jobs.values().filter(|j| j.state == JobState::Dead).count();
            if dead > cfg.dead_letter_limit {
                let mut ids: Vec<JobId> = jobs
                    .values()
                    .filter(|j| j.state == JobState::Dead)
                    .map(|j| j.id)
                    .collect();
                ids.sort_unstable();
                for old in ids.into_iter().take(dead - cfg.dead_letter_limit) {
                    jobs.remove(&old);
                }
            }
        }
        self.changed();
    }

    /// Run `workers` workers until `shutdown` fires; returns when the jobs that
    /// were in flight have finished and the queue has been saved.
    ///
    /// Typically called from [`App::background`](crate::app::App::background).
    /// A handler that panics counts as a failed attempt.
    pub async fn run<F, Fut>(&self, shutdown: Shutdown, workers: usize, handler: F)
    where
        F: Fn(Job<J>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), JobError>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..workers.max(1) {
            let (queue, handler, shutdown) = (self.clone(), handler.clone(), shutdown.clone());
            tasks.spawn(async move { queue.worker(shutdown, handler).await });
        }
        if self.inner.config.persist_path.is_some() {
            let (queue, shutdown) = (self.clone(), shutdown.clone());
            tasks.spawn(async move { queue.persister(shutdown).await });
        }
        while tasks.join_next().await.is_some() {}
        self.persist_now().await;
    }

    async fn worker<F, Fut>(&self, shutdown: Shutdown, handler: Arc<F>)
    where
        F: Fn(Job<J>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), JobError>> + Send + 'static,
    {
        loop {
            if shutdown.is_triggered() {
                return;
            }
            match self.take_due() {
                Ok(job) => {
                    let id = job.id;
                    let fut = handler(job);
                    // a separate task, so that a panic is a failed attempt, not a dead worker
                    let outcome = tokio::spawn(fut).await;
                    let result = match outcome {
                        Ok(r) => r,
                        Err(_) => Err(JobError::retry("handler panicked")),
                    };
                    self.finish(id, result);
                }
                Err(next_in) => {
                    let nap = Duration::from_millis(next_in.unwrap_or(500).min(500));
                    tokio::select! {
                        _ = self.inner.wake.notified() => {}
                        _ = tokio::time::sleep(nap) => {}
                        _ = shutdown.wait() => return,
                    }
                }
            }
        }
    }

    async fn persister(&self, shutdown: Shutdown) {
        loop {
            tokio::select! {
                _ = self.inner.dirty.notified() => {}
                _ = shutdown.wait() => return,
            }
            // coalesce bursts of changes into one write
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.persist_now().await;
        }
    }

    /// Write the queue to its file now (also done automatically while running)
    pub async fn persist_now(&self) {
        let Some(path) = &self.inner.config.persist_path else {
            return;
        };
        let bytes = {
            let st = self.lock();
            serde_json::to_vec(&Snapshot {
                next_id: st.next_id,
                jobs: st.jobs.values().cloned().collect(),
            })
        };
        let Ok(bytes) = bytes else { return };
        let tmp = path.with_extension("tmp");
        if tokio::fs::write(&tmp, bytes).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, path).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    use tokio::sync::watch;

    type Q = JobQueue<String>;

    fn fast() -> QueueConfig {
        QueueConfig::new()
            .base_delay(Duration::from_millis(20))
            .max_delay(Duration::from_millis(200))
            .jitter(false)
    }

    /// A queue with workers running in the background; `stop()` ends them
    struct Running {
        queue: Q,
        stop: watch::Sender<bool>,
        done: tokio::task::JoinHandle<()>,
    }

    impl Running {
        fn start<F, Fut>(queue: Q, workers: usize, handler: F) -> Self
        where
            F: Fn(Job<String>) -> Fut + Send + Sync + 'static,
            Fut: Future<Output = Result<(), JobError>> + Send + 'static,
        {
            let (stop, rx) = watch::channel(false);
            let q = queue.clone();
            let done =
                tokio::spawn(async move { q.run(Shutdown::new(rx), workers, handler).await });
            Self { queue, stop, done }
        }

        async fn stop(self) {
            let _ = self.stop.send(true);
            tokio::time::timeout(Duration::from_secs(5), self.done)
                .await
                .expect("workers did not stop")
                .unwrap();
        }
    }

    async fn until(mut cond: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !cond() {
            assert!(
                std::time::Instant::now() < deadline,
                "condition not reached in 5 s"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let cfg = QueueConfig::new()
            .base_delay(Duration::from_millis(100))
            .max_delay(Duration::from_secs(1));
        let ms: Vec<u128> = (1..=6).map(|a| cfg.backoff(a).as_millis()).collect();
        assert_eq!(ms, [100, 200, 400, 800, 1000, 1000]);
        assert_eq!(cfg.backoff(0).as_millis(), 100);
        assert_eq!(cfg.backoff(1000).as_millis(), 1000); // no overflow
    }

    #[test]
    fn jitter_stays_within_twenty_percent() {
        let q = Q::new(QueueConfig::new());
        for _ in 0..200 {
            let d = q.jitter(Duration::from_millis(1000)).as_millis();
            assert!((800..=1200).contains(&d), "{d}");
        }
        let still = Q::new(QueueConfig::new().jitter(false));
        assert_eq!(still.jitter(Duration::from_millis(1000)).as_millis(), 1000);
    }

    #[tokio::test]
    async fn jobs_run_once_and_are_removed() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let r = Running::start(Q::new(fast()), 2, move |job| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push((job.payload, job.attempt));
                Ok(())
            }
        });
        for n in ["a", "b", "c"] {
            r.queue.enqueue(n.to_string());
        }
        until(|| r.queue.stats().completed == 3).await;
        let mut got = seen.lock().unwrap().clone();
        got.sort();
        assert_eq!(got, [("a".into(), 1), ("b".into(), 1), ("c".into(), 1)]);
        assert_eq!(
            r.queue.stats(),
            Stats {
                completed: 3,
                ..Stats::default()
            }
        );
        r.stop().await;
    }

    #[tokio::test]
    async fn failures_are_retried_with_backoff_then_succeed() {
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let a = attempts.clone();
        let r = Running::start(Q::new(fast()), 1, move |job| {
            let a = a.clone();
            async move {
                a.lock().unwrap().push(std::time::Instant::now());
                if job.attempt < 3 {
                    Err(JobError::retry(format!("try {}", job.attempt)))
                } else {
                    Ok(())
                }
            }
        });
        let id = r.queue.enqueue("flaky".into());
        until(|| r.queue.stats().completed == 1).await;
        assert!(r.queue.get(id).is_none());
        assert_eq!(r.queue.stats().retried, 2);
        let times = attempts.lock().unwrap().clone();
        assert_eq!(times.len(), 3);
        // waited about 20 ms, then about 40 ms
        assert!(times[1] - times[0] >= Duration::from_millis(18));
        assert!(times[2] - times[1] >= Duration::from_millis(36));
        r.stop().await;
    }

    #[tokio::test]
    async fn exhausted_and_fatal_jobs_go_to_the_dead_letter_list() {
        let r = Running::start(Q::new(fast().max_attempts(3)), 2, |job| async move {
            if job.payload == "fatal" {
                Err(JobError::fatal("bad payload"))
            } else {
                Err(JobError::from("still broken"))
            }
        });
        let flaky = r.queue.enqueue("flaky".into());
        let fatal = r.queue.enqueue("fatal".into());
        until(|| r.queue.stats().dead == 2).await;

        let dead = r.queue.dead_jobs();
        assert_eq!(
            dead.iter().map(|j| j.id).collect::<Vec<_>>(),
            [flaky, fatal]
        );
        assert_eq!(
            (dead[0].attempts, dead[0].last_error.as_deref()),
            (3, Some("still broken"))
        );
        assert_eq!(
            (dead[1].attempts, dead[1].last_error.as_deref()),
            (1, Some("bad payload"))
        );
        assert_eq!(r.queue.stats().running + r.queue.stats().pending, 0);

        // dead jobs can be revived or deleted
        assert!(!r.queue.retry_dead(9999));
        assert!(r.queue.delete_dead(fatal));
        assert!(!r.queue.delete_dead(fatal));
        assert!(r.queue.retry_dead(flaky));
        until(|| {
            r.queue
                .get(flaky)
                .is_some_and(|j| j.state == JobState::Dead && j.attempts == 3)
        })
        .await;
        r.stop().await;
    }

    #[tokio::test]
    async fn retry_after_overrides_the_backoff_and_panics_count_as_failures() {
        let tries = Arc::new(AtomicU32::new(0));
        let t = tries.clone();
        let r = Running::start(
            Q::new(fast().base_delay(Duration::from_secs(60))),
            1,
            move |job| {
                let t = t.clone();
                async move {
                    match job.payload.as_str() {
                        "after" => {
                            if t.fetch_add(1, Ordering::SeqCst) == 0 {
                                Err(JobError::retry_after("later", Duration::from_millis(30)))
                            } else {
                                Ok(())
                            }
                        }
                        _ => panic!("boom"),
                    }
                }
            },
        );
        r.queue.enqueue("after".into());
        // would take a minute without the explicit delay
        until(|| r.queue.stats().completed == 1).await;

        let id = r.queue.enqueue("panics".into());
        until(|| r.queue.get(id).is_some_and(|j| j.attempts == 1)).await;
        assert_eq!(
            r.queue.get(id).unwrap().last_error.as_deref(),
            Some("handler panicked")
        );
        // the worker survived: it still processes jobs
        r.queue.enqueue("after".into());
        until(|| r.queue.stats().completed == 2).await;
        r.stop().await;
    }

    #[tokio::test]
    async fn delayed_jobs_wait_and_keys_deduplicate() {
        let ran = Arc::new(AtomicUsize::new(0));
        let n = ran.clone();
        let r = Running::start(Q::new(fast()), 1, move |_job| {
            let n = n.clone();
            async move {
                n.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        let started = std::time::Instant::now();
        let first = r.queue.enqueue_with(
            "x".into(),
            EnqueueOptions {
                delay: Duration::from_millis(150),
                key: Some("k".into()),
            },
        );
        let again = r.queue.enqueue_with(
            "y".into(),
            EnqueueOptions {
                key: Some("k".into()),
                ..Default::default()
            },
        );
        assert!(!first.duplicate && again.duplicate);
        assert_eq!(first.id, again.id);

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert_eq!(ran.load(Ordering::SeqCst), 0, "not due yet");
        until(|| ran.load(Ordering::SeqCst) == 1).await;
        assert!(started.elapsed() >= Duration::from_millis(140));

        // once the job is done the key is free again
        until(|| r.queue.stats().completed == 1).await;
        let reuse = r.queue.enqueue_with(
            "z".into(),
            EnqueueOptions {
                key: Some("k".into()),
                ..Default::default()
            },
        );
        assert!(!reuse.duplicate && reuse.id != first.id);
        until(|| ran.load(Ordering::SeqCst) == 2).await;
        r.stop().await;
    }

    #[tokio::test]
    async fn concurrency_never_exceeds_the_worker_count() {
        let (current, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (c, p) = (current.clone(), peak.clone());
        let r = Running::start(Q::new(fast()), 3, move |_job| {
            let (c, p) = (c.clone(), p.clone());
            async move {
                let now = c.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                c.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            }
        });
        for i in 0..12 {
            r.queue.enqueue(format!("job{i}"));
        }
        until(|| r.queue.stats().completed == 12).await;
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        r.stop().await;
    }

    #[tokio::test]
    async fn shutdown_lets_running_jobs_finish_and_leaves_the_rest_queued() {
        let started = Arc::new(Notify::new());
        let (s, finished) = (started.clone(), Arc::new(AtomicUsize::new(0)));
        let f = finished.clone();
        let r = Running::start(Q::new(fast()), 1, move |_job| {
            let (s, f) = (s.clone(), f.clone());
            async move {
                s.notify_one();
                tokio::time::sleep(Duration::from_millis(150)).await;
                f.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        });
        r.queue.enqueue("first".into());
        r.queue.enqueue("second".into());
        started.notified().await;

        let queue = r.queue.clone();
        r.stop().await; // returns only after the running job completed
        assert_eq!(finished.load(Ordering::SeqCst), 1);
        let stats = queue.stats();
        assert_eq!((stats.completed, stats.pending, stats.running), (1, 1, 0));
    }

    #[tokio::test]
    async fn the_queue_survives_a_restart() {
        let file = std::env::temp_dir().join(format!("ullun-jobs-{}.json", std::process::id()));
        std::fs::remove_file(&file).ok();
        let cfg = || fast().persist_to(file.clone());

        // jobs are enqueued but never run; one is "running" when the process dies
        let q = Q::new(cfg());
        let a = q.enqueue("a".into());
        let b = q
            .enqueue_with(
                "b".into(),
                EnqueueOptions {
                    key: Some("bk".into()),
                    delay: Duration::from_secs(60),
                },
            )
            .id;
        let claimed = q.take_due().map(|j| j.id).unwrap_or(0);
        assert_eq!(claimed, a);
        q.persist_now().await;
        drop(q);

        let q2 = Q::new(cfg());
        assert_eq!(q2.stats().pending, 2);
        assert_eq!(
            q2.get(a).unwrap().state,
            JobState::Pending,
            "interrupted jobs are run again"
        );
        assert_eq!(q2.get(b).unwrap().payload, "b");
        // keys and ids carry over
        assert!(
            q2.enqueue_with(
                "dup".into(),
                EnqueueOptions {
                    key: Some("bk".into()),
                    ..Default::default()
                }
            )
            .duplicate
        );
        assert!(q2.enqueue("new".into()) > b);

        // and the revived queue works
        let seen = Arc::new(Mutex::new(Vec::new()));
        let s = seen.clone();
        let r = Running::start(q2, 1, move |job| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push(job.payload);
                Ok(())
            }
        });
        until(|| seen.lock().unwrap().len() >= 2).await;
        let got = seen.lock().unwrap().clone();
        assert!(got.contains(&"a".to_string()) && got.contains(&"new".to_string()));
        r.stop().await;

        // the file reflects what is left (the delayed job "b")
        let q3 = Q::new(cfg());
        assert_eq!(q3.stats().pending, 1);

        std::fs::write(&file, b"garbage").unwrap();
        assert_eq!(
            Q::new(cfg()).stats(),
            Stats::default(),
            "a corrupt file must not stop startup"
        );
        std::fs::remove_file(&file).ok();
    }

    #[tokio::test]
    async fn only_the_newest_dead_jobs_are_kept() {
        let r = Running::start(
            Q::new(fast().max_attempts(1).dead_letter_limit(2)),
            1,
            |_job| async { Err(JobError::fatal("no")) },
        );
        let ids: Vec<JobId> = (0..4).map(|i| r.queue.enqueue(format!("j{i}"))).collect();
        until(|| {
            let s = r.queue.stats();
            s.pending == 0 && s.running == 0 && s.dead == 2
        })
        .await;
        let dead: Vec<JobId> = r.queue.dead_jobs().iter().map(|j| j.id).collect();
        assert_eq!(dead, ids[2..]);
        r.stop().await;
    }
}
