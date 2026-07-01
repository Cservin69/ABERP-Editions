//! ADR-0098 D2 — the checkpoint-cadence coordinator.
//!
//! Pure, clock-injected decision logic with **no DuckDB / no I/O**, so it
//! compiles and unit-tests in the saw-off sandbox (the rest of the crate is
//! gated behind the bundled-libduckdb amalgamation). The [`Handle`] consults
//! this to decide *whether* to run a (relatively expensive) validated
//! [`aberp_snapshot::live_durable_checkpoint`]; it never decides *how* to
//! checkpoint — that stays with the ADR-0095 primitive.
//!
//! [`Handle`]: crate::Handle
//!
//! # The D2 rule, verbatim
//!
//! > coalesce ≤ 1 `durable_checkpoint`/min + one at loop-idle (queue
//! > drained); `checkpoint_is_current` makes quiescent ticks free.
//!
//! Two firing conditions, one suppressor:
//!
//! * **post-write** — after a committed write, checkpoint **iff** the file
//!   is dirty since the last checkpoint **and** at least `min_interval` has
//!   elapsed since the last one (the "≤ 1/min" coalescing).
//! * **loop-idle** — when a daemon drains its queue, checkpoint **iff** the
//!   file is dirty since the last checkpoint, *regardless* of the interval
//!   (the "+ one at loop-idle"): idle is the cheapest moment to pay for a
//!   durable checkpoint, so we take it even inside the 1-min window.
//! * **suppressor** — a checkpoint is only ever *worth* running when the
//!   file changed since the last one; a quiescent period (no writes) leaves
//!   `dirty == false`, so both conditions return `false` for free. (In the
//!   `Handle` this is reinforced by `checkpoint_is_current`, which no-ops a
//!   redundant checkpoint even if we ask for one.)

use std::time::{Duration, Instant};

/// The default coalescing window — ADR-0098 D2's "≤ 1 `durable_checkpoint`
/// per minute". The post-write path will not run a second durable checkpoint
/// until this much wall-clock has elapsed since the last one.
pub const DEFAULT_MIN_CHECKPOINT_INTERVAL: Duration = Duration::from_secs(60);

/// Tracks "is the live file dirty since the last durable checkpoint?" and
/// "how long since the last one?", and answers the two D2 firing questions.
///
/// Deliberately clock-injected (`now: Instant` is passed in, never read from
/// the system clock here) so the logic is deterministic under test. `Instant`
/// is monotonic, so the elapsed comparison cannot be skewed by wall-clock
/// adjustments.
#[derive(Debug, Clone)]
pub struct CheckpointDebouncer {
    min_interval: Duration,
    /// `None` until the first checkpoint is recorded — so the very first
    /// post-write checkpoint is never blocked by the interval (there is no
    /// "last" to be too-soon after).
    last_checkpoint: Option<Instant>,
    /// Set by `note_write`, cleared by `record_checkpoint`. The single
    /// source of "is a checkpoint worth running?".
    dirty: bool,
}

impl CheckpointDebouncer {
    /// New debouncer with the given coalescing window. A fresh debouncer is
    /// **not** dirty (nothing has been written) and has never checkpointed.
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_checkpoint: None,
            dirty: false,
        }
    }

    /// The ADR-0098 D2 default (`≤ 1/min`).
    pub fn with_default_interval() -> Self {
        Self::new(DEFAULT_MIN_CHECKPOINT_INTERVAL)
    }

    /// Record that a committed write changed the live file. Idempotent; only
    /// flips the dirty flag.
    pub fn note_write(&mut self) {
        self.dirty = true;
    }

    /// Whether the file is dirty since the last durable checkpoint.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// **Post-write** firing decision: dirty AND (never checkpointed OR the
    /// coalescing window has fully elapsed since the last checkpoint).
    pub fn should_checkpoint_now(&self, now: Instant) -> bool {
        if !self.dirty {
            return false;
        }
        match self.last_checkpoint {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= self.min_interval,
        }
    }

    /// **Loop-idle** firing decision: dirty, regardless of the interval. Idle
    /// is the cheap moment, so D2 says take the checkpoint even inside the
    /// 1-min window.
    pub fn should_checkpoint_on_idle(&self) -> bool {
        self.dirty
    }

    /// Record that a durable checkpoint just completed at `now`: clears dirty
    /// and resets the coalescing clock. Call this for **both** a real
    /// checkpoint and a `checkpoint_is_current` no-op — in either case the
    /// file is now covered by a verified-good marker, so the window restarts.
    pub fn record_checkpoint(&mut self, now: Instant) {
        self.last_checkpoint = Some(now);
        self.dirty = false;
    }

    /// The configured coalescing window (introspection / tests).
    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }
}

impl Default for CheckpointDebouncer {
    fn default() -> Self {
        Self::with_default_interval()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = Duration::from_secs(60);

    #[test]
    fn fresh_debouncer_never_checkpoints_when_clean() {
        let d = CheckpointDebouncer::new(MIN);
        let now = Instant::now();
        assert!(!d.is_dirty());
        assert!(
            !d.should_checkpoint_now(now),
            "clean file → no post-write ckpt"
        );
        assert!(!d.should_checkpoint_on_idle(), "clean file → no idle ckpt");
    }

    #[test]
    fn first_write_checkpoints_immediately_no_prior_interval() {
        let mut d = CheckpointDebouncer::new(MIN);
        let now = Instant::now();
        d.note_write();
        // No "last" checkpoint to be too-soon after → fires right away.
        assert!(d.should_checkpoint_now(now));
    }

    #[test]
    fn post_write_coalesces_within_the_one_minute_window() {
        let mut d = CheckpointDebouncer::new(MIN);
        let t0 = Instant::now();
        d.note_write();
        d.record_checkpoint(t0); // just checkpointed at t0
                                 // Another write 30s later: dirty again, but inside the window → no fire.
        d.note_write();
        assert!(!d.should_checkpoint_now(t0 + Duration::from_secs(30)));
        // Exactly at the window boundary → fires (>=).
        assert!(d.should_checkpoint_now(t0 + MIN));
        // Well past → fires.
        assert!(d.should_checkpoint_now(t0 + Duration::from_secs(90)));
    }

    #[test]
    fn idle_checkpoints_even_inside_the_window_when_dirty() {
        let mut d = CheckpointDebouncer::new(MIN);
        let t0 = Instant::now();
        d.note_write();
        d.record_checkpoint(t0);
        d.note_write(); // dirtied again, 0s later
                        // Post-write path is coalesced (inside window)…
        assert!(!d.should_checkpoint_now(t0 + Duration::from_secs(5)));
        // …but the loop-idle path takes it anyway (cheapest moment, D2).
        assert!(d.should_checkpoint_on_idle());
    }

    #[test]
    fn idle_is_a_noop_when_not_dirty() {
        let mut d = CheckpointDebouncer::new(MIN);
        let t0 = Instant::now();
        d.note_write();
        d.record_checkpoint(t0); // clean now
        assert!(
            !d.should_checkpoint_on_idle(),
            "nothing written since ckpt → idle no-op"
        );
    }

    #[test]
    fn record_checkpoint_clears_dirty_and_resets_window() {
        let mut d = CheckpointDebouncer::new(MIN);
        let t0 = Instant::now();
        d.note_write();
        assert!(d.is_dirty());
        d.record_checkpoint(t0);
        assert!(!d.is_dirty(), "checkpoint clears dirty");
        // Clean → neither path fires.
        assert!(!d.should_checkpoint_now(t0 + Duration::from_secs(120)));
        assert!(!d.should_checkpoint_on_idle());
    }

    #[test]
    fn a_noop_checkpoint_still_resets_the_window() {
        // The Handle calls record_checkpoint() for BOTH a real checkpoint and
        // a checkpoint_is_current() no-op (Ok(None)); either way the file is
        // covered, so the window must restart. This asserts that contract.
        let mut d = CheckpointDebouncer::new(MIN);
        let t0 = Instant::now();
        d.note_write();
        d.record_checkpoint(t0); // treat as the Ok(None) no-op path
        d.note_write();
        assert!(!d.should_checkpoint_now(t0 + Duration::from_secs(10)));
    }

    #[test]
    fn custom_interval_is_honoured() {
        let mut d = CheckpointDebouncer::new(Duration::from_secs(5));
        let t0 = Instant::now();
        d.note_write();
        d.record_checkpoint(t0);
        d.note_write();
        assert!(!d.should_checkpoint_now(t0 + Duration::from_secs(4)));
        assert!(d.should_checkpoint_now(t0 + Duration::from_secs(5)));
        assert_eq!(d.min_interval(), Duration::from_secs(5));
    }

    #[test]
    fn default_interval_is_one_minute() {
        assert_eq!(
            CheckpointDebouncer::default().min_interval(),
            DEFAULT_MIN_CHECKPOINT_INTERVAL
        );
        assert_eq!(DEFAULT_MIN_CHECKPOINT_INTERVAL, Duration::from_secs(60));
    }
}
