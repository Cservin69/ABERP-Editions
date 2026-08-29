//! Retention policy — the pure, heavily-tested core of the snapshot system.
//!
//! [`plan_retention`] is a pure function over a slice of [`SnapshotRecord`]
//! and a [`RetentionPolicy`]; it decides which seqs to keep and which to
//! prune, and is unit-tested exhaustively. [`prune`] is the thin IO that
//! removes the directories a plan condemns.

use std::collections::BTreeSet;

use time::{Duration, OffsetDateTime};

use crate::store::SnapshotRecord;
use crate::{Result, SnapshotError};

/// How many snapshots to retain across three overlapping windows. A
/// snapshot kept by *any* window survives. Defaults from ADR-0082:
/// 24 last-N (4 days at the 4h cadence) + 30 daily + 52 weekly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Keep the most recent `keep_last` snapshots unconditionally.
    pub keep_last: usize,
    /// Keep one snapshot per UTC calendar day, for this many days back.
    pub daily_days: i64,
    /// Keep one snapshot per ISO week, for this many weeks back.
    pub weekly_weeks: i64,
    /// **ADR-0116 D2 / G8** — keep the N most recent snapshots that FAILED
    /// validation, as forensic evidence.
    ///
    /// A snapshot that fails `validate_export` is written to disk, marked
    /// `valid=false`, and — before this rule — pruned by the *same cycle that
    /// created it*, milliseconds later, because every keep rule below
    /// considers only valid snapshots. Prod lost real forensic evidence to
    /// that twice: on 2026-07-08 a snapshot caught a live audit-chain defect
    /// (`"out of order: expected seq=7995, found seq=7994"`) and the system
    /// deleted the only artefact of it in the same cycle; it happened again on
    /// 2026-07-09. All that survived was the error string in the audit
    /// payload, and because the seq was recycled even the identifier is
    /// ambiguous.
    ///
    /// An invalid snapshot has no **restore** value and the highest
    /// **forensic** value of anything the subsystem produces: a complete
    /// logical export taken at the instant a defect was detected. Those are
    /// different questions, and the pre-D2 code answered only the first.
    pub keep_failed: usize,
    /// **ADR-0116 D2 / G8** — additionally keep EVERY failed snapshot within
    /// this many days, whichever set is larger. A cluster of failures inside
    /// one incident is the case the count-based floor alone would truncate.
    pub keep_failed_days: i64,
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        RetentionPolicy {
            keep_last: 24,
            daily_days: 30,
            weekly_weeks: 52,
            keep_failed: 3,
            keep_failed_days: 30,
        }
    }
}

/// Result of [`plan_retention`]: the seqs to keep and the seqs to prune.
/// Disjoint; their union is every input seq.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionPlan {
    pub keep: Vec<u64>,
    pub prune: Vec<u64>,
}

/// Decide retention. Pure — no IO, no clock read (the caller passes `now`).
///
/// Rules (a snapshot survives if ANY apply):
///   - it is the **newest valid** snapshot (never pruned — the last-good
///     rollback point is sacred even outside every window);
///   - it is among the `keep_last` most recent **valid** snapshots;
///   - it is the newest **valid** snapshot of its UTC day, within
///     `daily_days` of `now`;
///   - it is the newest **valid** snapshot of its ISO week, within
///     `weekly_weeks` of `now`.
///
/// **ADR-0116 D2 / G8** — invalid snapshots are kept as FORENSIC evidence:
/// the `keep_failed` most recent, plus every failed snapshot within
/// `keep_failed_days`, whichever set is larger. They have no *restore* value
/// (`restore_into` refuses them) but the highest *forensic* value the
/// subsystem produces, and the pre-D2 code deleted them in the very cycle
/// that created them. They can never displace the newest-valid rule, which
/// only ever keeps a valid snapshot.
///
/// If there are no valid snapshots at all, the single newest snapshot overall
/// is kept as a last resort so the store never drops to zero.
pub fn plan_retention(
    records: &[SnapshotRecord],
    policy: &RetentionPolicy,
    now: OffsetDateTime,
) -> RetentionPlan {
    let mut keep: BTreeSet<u64> = BTreeSet::new();

    // Newest first, by seq (monotonic with creation).
    let mut by_seq_desc: Vec<&SnapshotRecord> = records.iter().collect();
    by_seq_desc.sort_by_key(|r| std::cmp::Reverse(r.meta.seq));

    let valid_desc: Vec<&SnapshotRecord> = by_seq_desc
        .iter()
        .copied()
        .filter(|r| r.meta.valid)
        .collect();

    // Last-resort floor: never let the store go empty. If nothing is valid,
    // keep the single newest snapshot so there is *something* to inspect.
    if valid_desc.is_empty() {
        if let Some(newest) = by_seq_desc.first() {
            keep.insert(newest.meta.seq);
        }
        return finalize(by_seq_desc, keep);
    }

    // 1. Newest valid — sacred.
    keep.insert(valid_desc[0].meta.seq);

    // 2. Last N valid.
    for r in valid_desc.iter().take(policy.keep_last) {
        keep.insert(r.meta.seq);
    }

    // 3. Daily: newest valid per UTC day within the window. Because
    //    valid_desc is newest-first, the FIRST time we see a given day key
    //    is that day's newest valid snapshot.
    let daily_cutoff = now - Duration::days(policy.daily_days);
    let mut seen_days: BTreeSet<(i32, u8, u8)> = BTreeSet::new();
    for r in &valid_desc {
        if r.meta.created_at < daily_cutoff {
            continue;
        }
        let d = r.meta.created_at.date();
        let key = (d.year(), d.month() as u8, d.day());
        if seen_days.insert(key) {
            keep.insert(r.meta.seq);
        }
    }

    // 3b. ADR-0116 G8 — forensic retention of FAILED snapshots. Runs
    //     regardless of the valid-set rules above (a store can be entirely
    //     valid and still have produced a failure worth keeping).
    keep_failed_forensic(&by_seq_desc, policy, now, &mut keep);

    // 4. Weekly: newest valid per week within the window. The week is keyed
    //    by its Monday's (year, day-of-year) — computed via OffsetDateTime
    //    arithmetic, which sidesteps ISO-year boundary subtleties and keeps
    //    one bucket per calendar week.
    let weekly_cutoff = now - Duration::weeks(policy.weekly_weeks);
    let mut seen_weeks: BTreeSet<(i32, u16)> = BTreeSet::new();
    for r in &valid_desc {
        if r.meta.created_at < weekly_cutoff {
            continue;
        }
        let key = week_bucket(r.meta.created_at);
        if seen_weeks.insert(key) {
            keep.insert(r.meta.seq);
        }
    }

    finalize(by_seq_desc, keep)
}

/// ADR-0116 G8 — mark the failed snapshots that are kept for forensics.
///
/// Two overlapping windows, union kept (the same "any window wins" shape the
/// valid rules use): the `keep_failed` most recent failures by seq, and every
/// failure newer than `keep_failed_days`.
///
/// **`records` must be newest-first by seq** — the caller sorts it that way,
/// and this relies on it to take "the N most recent".
fn keep_failed_forensic(
    by_seq_desc: &[&SnapshotRecord],
    policy: &RetentionPolicy,
    now: OffsetDateTime,
    keep: &mut BTreeSet<u64>,
) {
    let failed_desc: Vec<&&SnapshotRecord> = by_seq_desc.iter().filter(|r| !r.meta.valid).collect();
    for r in failed_desc.iter().take(policy.keep_failed) {
        keep.insert(r.meta.seq);
    }
    let cutoff = now - Duration::days(policy.keep_failed_days);
    for r in &failed_desc {
        if r.meta.created_at >= cutoff {
            keep.insert(r.meta.seq);
        }
    }
}

/// Unique key for the calendar week containing `dt`: the (year, ordinal) of
/// that week's Monday. Two timestamps share a key iff they fall in the same
/// Mon–Sun week.
fn week_bucket(dt: OffsetDateTime) -> (i32, u16) {
    let days_from_monday = dt.weekday().number_days_from_monday() as i64;
    let monday = (dt - Duration::days(days_from_monday)).date();
    (monday.year(), monday.ordinal())
}

fn finalize(by_seq_desc: Vec<&SnapshotRecord>, keep: BTreeSet<u64>) -> RetentionPlan {
    let mut prune = Vec::new();
    for r in &by_seq_desc {
        if !keep.contains(&r.meta.seq) {
            prune.push(r.meta.seq);
        }
    }
    prune.sort_unstable();
    RetentionPlan {
        // `keep` comes from a BTreeSet → already ascending.
        keep: keep.into_iter().collect(),
        prune,
    }
}

/// Remove the snapshot directories a [`RetentionPlan`] condemns. Returns the
/// seqs actually removed (a seq with no matching directory is skipped — the
/// goal state is "gone", and an already-gone snapshot satisfies it).
///
/// **ADR-0116 D2 — every removal goes through
/// [`crate::is_protected_evidence`].** `prune` only ever touches the snapshot
/// store, where no `.CORRUPT-*` evidence lives, so this belt protects nothing
/// on its own; the load-bearing half is that every *tenant-home* helper calls
/// the same predicate, which the ADR-0116 cut-gate check enforces. It is here
/// so that the pruner's blindness to evidence is a **deliberate refusal**
/// rather than the structural accident it has been — the failure mode the
/// first draft's guard was written to close, and the one it missed.
///
/// A refusal is LOUD and does NOT fail the whole prune: the remaining
/// condemned snapshots are still removed, and the refused seq is simply not
/// reported as removed. A retention pass that aborted on the first protected
/// path would leave the store growing while looking like it had run.
pub fn prune(records: &[SnapshotRecord], plan: &RetentionPlan) -> Result<Vec<u64>> {
    let mut removed = Vec::new();
    for &seq in &plan.prune {
        let Some(rec) = records.iter().find(|r| r.meta.seq == seq) else {
            continue;
        };
        if crate::evidence::is_protected_evidence(&rec.dir) {
            tracing::error!(
                seq,
                dir = %rec.dir.display(),
                "ADR-0116 D2 — retention condemned a PROTECTED path; refusing to remove it.                  This is either recovery evidence inside the snapshot store or a store                  directory whose name matches an evidence family — investigate before                  pruning again."
            );
            continue;
        }
        match std::fs::remove_dir_all(&rec.dir) {
            Ok(()) => removed.push(seq),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => removed.push(seq),
            Err(e) => return Err(SnapshotError::io(&rec.dir, e)),
        }
    }
    Ok(removed)
}
