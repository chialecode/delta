//! Latency probes for R1-A-19 (window close within 1 s) and R1-A-24 (interaction
//! P95 and chart frame rate).
//!
//! These only record. They measure CPU time in the handlers and the paint pass,
//! and the time from a close request to the end of the event loop. They do not
//! measure GPU presentation, so they cannot show 50 FPS, and nothing here claims
//! a target is met: that needs the reference device in ACT-02. The summary is
//! written to the log (`delta::perf`) when the application exits.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Samples kept per probe; older ones are dropped.
const KEEP: usize = 2000;

#[derive(Default)]
pub struct LatencyLog {
    samples: Vec<Duration>,
}

impl LatencyLog {
    pub fn record(&mut self, sample: Duration) {
        if self.samples.len() == KEEP {
            self.samples.remove(0);
        }
        self.samples.push(sample);
    }

    pub fn count(&self) -> usize {
        self.samples.len()
    }

    /// Nearest-rank percentile (`p` in 0..=100); `None` without samples.
    pub fn percentile(&self, p: f64) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted = self.samples.clone();
        sorted.sort();
        let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
        Some(sorted[rank.clamp(1, sorted.len()) - 1])
    }

    pub fn p95(&self) -> Option<Duration> {
        self.percentile(95.0)
    }
}

static INTERACTIONS: Mutex<LatencyLog> = Mutex::new(LatencyLog {
    samples: Vec::new(),
});
static FRAMES: Mutex<LatencyLog> = Mutex::new(LatencyLog {
    samples: Vec::new(),
});
static CLOSE_REQUESTED: OnceLock<Instant> = OnceLock::new();

fn with_log<R>(log: &Mutex<LatencyLog>, f: impl FnOnce(&mut LatencyLog) -> R) -> R {
    f(&mut log.lock().unwrap_or_else(|e| e.into_inner()))
}

/// Time one user interaction handler (search, watch toggle) until it returns.
pub struct InteractionTimer(Instant);

impl InteractionTimer {
    pub fn start() -> Self {
        Self(Instant::now())
    }
}

impl Drop for InteractionTimer {
    fn drop(&mut self) {
        with_log(&INTERACTIONS, |log| log.record(self.0.elapsed()));
    }
}

/// Time one chart paint pass (CPU side) until it ends, including early returns.
pub struct FrameTimer(Instant);

impl FrameTimer {
    pub fn start() -> Self {
        Self(Instant::now())
    }
}

impl Drop for FrameTimer {
    fn drop(&mut self) {
        with_log(&FRAMES, |log| log.record(self.0.elapsed()));
    }
}

/// The user asked to close the window.
pub fn mark_close_requested() {
    let _ = CLOSE_REQUESTED.set(Instant::now());
}

/// Write what was recorded to the log. Called once when the event loop ends.
pub fn log_summary() {
    let ms = |d: Option<Duration>| d.map(|d| d.as_secs_f64() * 1000.0);
    if let Some(requested) = CLOSE_REQUESTED.get() {
        tracing::info!(
            target: "delta::perf",
            close_to_exit_ms = requested.elapsed().as_secs_f64() * 1000.0,
            "window close to end of event loop (R1-A-19 probe; the target needs a device)"
        );
    }
    with_log(&INTERACTIONS, |log| {
        tracing::info!(
            target: "delta::perf",
            samples = log.count(),
            p95_ms = ms(log.p95()),
            "interaction handler time (R1-A-24 probe; CPU only)"
        );
    });
    with_log(&FRAMES, |log| {
        tracing::info!(
            target: "delta::perf",
            samples = log.count(),
            p95_ms = ms(log.p95()),
            "chart paint time (R1-A-24 probe; CPU only, not GPU frames per second)"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log_of(ms: &[u64]) -> LatencyLog {
        let mut log = LatencyLog::default();
        for m in ms {
            log.record(Duration::from_millis(*m));
        }
        log
    }

    #[test]
    fn percentile_uses_nearest_rank() {
        assert!(LatencyLog::default().p95().is_none());
        let hundred: Vec<u64> = (1..=100).collect();
        assert_eq!(
            log_of(&hundred).p95(),
            Some(Duration::from_millis(95)),
            "the 95th of 100 ordered samples"
        );
        assert_eq!(
            log_of(&[7]).p95(),
            Some(Duration::from_millis(7)),
            "a single sample is its own P95"
        );
        // Order of arrival does not matter; the slow tail does.
        let mut shuffled = hundred.clone();
        shuffled.reverse();
        assert_eq!(log_of(&shuffled).p95(), Some(Duration::from_millis(95)));
        assert_eq!(
            log_of(&hundred).percentile(100.0),
            Some(Duration::from_millis(100))
        );
    }

    #[test]
    fn the_log_keeps_the_newest_samples_only() {
        let mut log = LatencyLog::default();
        for i in 0..(KEEP as u64 + 10) {
            log.record(Duration::from_millis(i));
        }
        assert_eq!(log.count(), KEEP);
        assert_eq!(
            log.percentile(100.0),
            Some(Duration::from_millis(KEEP as u64 + 9))
        );
    }

    #[test]
    fn timers_record_when_dropped() {
        let before = with_log(&INTERACTIONS, |log| log.count());
        drop(InteractionTimer::start());
        assert!(with_log(&INTERACTIONS, |log| log.count()) > before);
        let before = with_log(&FRAMES, |log| log.count());
        drop(FrameTimer::start());
        assert!(with_log(&FRAMES, |log| log.count()) > before);
    }
}
