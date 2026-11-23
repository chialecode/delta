use crate::process::apply_process_quiet_guards;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAX_RETAINED_JOBS: usize = 256;

thread_local! {
    static ISOLATED_WORKER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(super) type SyncControl = (
    Arc<AtomicBool>,
    Instant,
    Arc<Mutex<JobState>>,
    Arc<Mutex<JobProgressDto>>,
);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub trace_id: String,
}

impl ErrorEnvelope {
    pub fn new(code: impl Into<String>, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
            trace_id: format!("TRC-{}", Utc::now().timestamp_micros()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed(ErrorEnvelope),
    Cancelled(ErrorEnvelope),
    TimedOut(ErrorEnvelope),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub id: JobId,
    pub name: String,
    pub state: JobState,
    pub elapsed_ms: u64,
    #[serde(default)]
    pub worker_finished: bool,
    #[serde(default)]
    pub progress: JobProgressDto,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobProgressDto {
    pub phase: String,
    pub records_prepared: usize,
    pub records_published: usize,
}

struct ManagedJob {
    id: JobId,
    name: String,
    cancel_flag: Arc<AtomicBool>,
    state: Arc<Mutex<JobState>>,
    started_at: Instant,
    finished: Arc<AtomicBool>,
    progress: Arc<Mutex<JobProgressDto>>,
}

/// Background job controller isolating panics, timeouts, and cancellation (S-02 / AC-16).
pub struct JobController {
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<u64, ManagedJob>>,
}

impl Default for JobController {
    fn default() -> Self {
        Self::new()
    }
}

impl JobController {
    pub fn new() -> Self {
        static PANIC_HOOK: std::sync::Once = std::sync::Once::new();
        PANIC_HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if !ISOLATED_WORKER.with(|flag| flag.get()) {
                    previous(info);
                }
            }));
        });
        Self {
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn spawn_job<F>(&self, name: impl Into<String>, timeout: Duration, task: F) -> JobId
    where
        F: FnOnce(Arc<AtomicBool>) -> Result<(), ErrorEnvelope> + Send + 'static,
    {
        self.spawn_controlled_job(
            name,
            timeout,
            move |cancel, _state, _deadline, _progress| task(cancel),
        )
    }

    pub(super) fn spawn_controlled_job<F>(
        &self,
        name: impl Into<String>,
        timeout: Duration,
        task: F,
    ) -> JobId
    where
        F: FnOnce(
                Arc<AtomicBool>,
                Arc<Mutex<JobState>>,
                Instant,
                Arc<Mutex<JobProgressDto>>,
            ) -> Result<(), ErrorEnvelope>
            + Send
            + 'static,
    {
        let id_num = self.next_id.fetch_add(1, Ordering::SeqCst);
        let job_id = JobId(id_num);
        let job_name = name.into();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(JobState::Running));
        let started_at = Instant::now();
        let finished = Arc::new(AtomicBool::new(false));
        let finished_worker = Arc::clone(&finished);
        let progress = Arc::new(Mutex::new(JobProgressDto {
            phase: "启动中".into(),
            ..Default::default()
        }));
        let progress_worker = Arc::clone(&progress);

        let cancel_clone = Arc::clone(&cancel_flag);
        let state_clone = Arc::clone(&state);
        let name_clone = job_name.clone();
        let (completion_sender, completion_receiver) = std::sync::mpsc::channel();
        let deadline_state = Arc::clone(&state);
        let deadline_cancel = Arc::clone(&cancel_flag);
        thread::spawn(move || {
            if completion_receiver.recv_timeout(timeout).is_err() {
                let mut state = deadline_state.lock().unwrap_or_else(|e| e.into_inner());
                if matches!(*state, JobState::Running) {
                    deadline_cancel.store(true, Ordering::SeqCst);
                    *state = JobState::TimedOut(ErrorEnvelope::new(
                        "job_timed_out",
                        "job deadline expired; cancellation requested",
                        true,
                    ));
                }
            }
        });

        thread::spawn(move || {
            apply_process_quiet_guards();
            ISOLATED_WORKER.with(|flag| flag.set(true));
            let result = catch_unwind(AssertUnwindSafe(|| {
                task(
                    Arc::clone(&cancel_clone),
                    Arc::clone(&state_clone),
                    started_at + timeout,
                    progress_worker,
                )
            }));
            ISOLATED_WORKER.with(|flag| flag.set(false));
            let mut guard = state_clone.lock().unwrap_or_else(|e| e.into_inner());
            if !matches!(*guard, JobState::Running) {
                // Cancellation/deadline may have published a terminal state already.
            } else if cancel_clone.load(Ordering::SeqCst) {
                *guard = JobState::Cancelled(ErrorEnvelope::new(
                    "job_cancelled",
                    format!("background job '{name_clone}' was cancelled"),
                    true,
                ));
            } else if started_at.elapsed() > timeout {
                *guard = JobState::TimedOut(ErrorEnvelope::new(
                    "job_timed_out",
                    format!(
                        "background job '{name_clone}' exceeded timeout of {} ms",
                        timeout.as_millis()
                    ),
                    true,
                ));
            } else {
                match result {
                    Ok(Ok(())) => {
                        *guard = JobState::Succeeded;
                    }
                    Ok(Err(env)) => {
                        *guard = JobState::Failed(env);
                    }
                    Err(_) => {
                        *guard = JobState::Failed(ErrorEnvelope::new(
                            "job_panicked",
                            format!("background job '{name_clone}' panicked; see trace ID"),
                            false,
                        ));
                    }
                }
            }
            // A terminal state and released task resources must precede completion.
            finished_worker.store(true, Ordering::SeqCst);
            drop(guard);
            let _ = completion_sender.send(());
        });

        let mut jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        jobs.insert(
            id_num,
            ManagedJob {
                id: job_id,
                name: job_name,
                cancel_flag,
                state,
                started_at,
                finished,
                progress,
            },
        );
        if jobs.len() > MAX_RETAINED_JOBS {
            let to_evict = jobs.len() - MAX_RETAINED_JOBS;
            let stale_ids: Vec<u64> = jobs
                .iter()
                .filter(|(&k, job)| k != id_num && job.finished.load(Ordering::SeqCst))
                .map(|(&k, _)| k)
                .take(to_evict)
                .collect();
            for stale_id in stale_ids {
                jobs.remove(&stale_id);
            }
        }
        job_id
    }

    pub fn cancel_job(&self, id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let job = jobs.get(&id.0).ok_or_else(|| {
            ErrorEnvelope::new("job_not_found", format!("no job {}", id.0), false)
        })?;
        let mut st = job.state.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(*st, JobState::Running) {
            job.cancel_flag.store(true, Ordering::SeqCst);
            *st = JobState::Cancelled(ErrorEnvelope::new(
                "job_cancelled",
                format!("job '{}' cancelled by caller", job.name),
                true,
            ));
        }
        let progress = job
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ok(JobSnapshot {
            id: job.id,
            name: job.name.clone(),
            state: st.clone(),
            elapsed_ms: job.started_at.elapsed().as_millis() as u64,
            worker_finished: job.finished.load(Ordering::SeqCst),
            progress,
        })
    }

    pub fn wait_job(&self, id: JobId, max_wait: Duration) -> Result<JobSnapshot, ErrorEnvelope> {
        let start = Instant::now();
        loop {
            let (name, state_arc, started_at, finished, progress) = {
                let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
                let job = jobs.get(&id.0).ok_or_else(|| {
                    ErrorEnvelope::new("job_not_found", format!("no job {}", id.0), false)
                })?;
                (
                    job.name.clone(),
                    Arc::clone(&job.state),
                    job.started_at,
                    Arc::clone(&job.finished),
                    Arc::clone(&job.progress),
                )
            };
            {
                let st = state_arc.lock().unwrap_or_else(|e| e.into_inner());
                if !matches!(*st, JobState::Running) {
                    return Ok(JobSnapshot {
                        id,
                        name,
                        state: st.clone(),
                        elapsed_ms: started_at.elapsed().as_millis() as u64,
                        worker_finished: finished.load(Ordering::SeqCst),
                        progress: progress.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                    });
                }
                if start.elapsed() > max_wait {
                    return Err(ErrorEnvelope::new(
                        "wait_timeout",
                        format!("waiting for {name} exceeded caller deadline"),
                        true,
                    ));
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn snapshot(&self, id: JobId) -> Result<JobSnapshot, ErrorEnvelope> {
        let jobs = self.jobs.lock().unwrap_or_else(|e| e.into_inner());
        let job = jobs
            .get(&id.0)
            .ok_or_else(|| ErrorEnvelope::new("job_not_found", "unknown background job", false))?;
        let state = job.state.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let progress = job
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        Ok(JobSnapshot {
            id,
            name: job.name.clone(),
            state,
            elapsed_ms: job.started_at.elapsed().as_millis() as u64,
            worker_finished: job.finished.load(Ordering::SeqCst),
            progress,
        })
    }

    pub fn wait_finished(
        &self,
        id: JobId,
        timeout: Duration,
    ) -> Result<JobSnapshot, ErrorEnvelope> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = self.snapshot(id)?;
            if snapshot.worker_finished {
                return Ok(snapshot);
            }
            if Instant::now() >= deadline {
                return Err(ErrorEnvelope::new(
                    "wait_timeout",
                    "worker has not released its resources yet",
                    true,
                ));
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn cancel_all_and_wait(&self, timeout: Duration) -> Result<(), ErrorEnvelope> {
        let ids: Vec<_> = self
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|job| !job.finished.load(Ordering::SeqCst))
            .map(|job| job.id)
            .collect();
        for &id in &ids {
            let _ = self.cancel_job(id);
        }
        let deadline = Instant::now() + timeout;
        for id in ids {
            self.wait_finished(id, deadline.saturating_duration_since(Instant::now()))?;
        }
        Ok(())
    }
}
