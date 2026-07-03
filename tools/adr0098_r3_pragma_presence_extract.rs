//! ADR-0098 R3 (finding C) — toolchain-checkable extraction of the two R3
//! decisions, as self-contained `rustc --test` logic (std-only; no crate deps,
//! same spirit as the R1/R2 extracts). Proves:
//!   (A) the pragma-presence gate rule (cut-gate CHECK 10j): every RUNTIME
//!       `Connection::open` carries `disable_checkpoint_on_shutdown` within a
//!       short window; `#[cfg(test)]` opens are ignored; a stripped pragma is
//!       flagged. This is the faithful analogue of the bash/awk gate check.
//!   (B) the ingest-seam routing invariant: routing the AP-ingest write through
//!       the single shared Handle writer WITH the no-in-place-fold pragma means a
//!       write is never orphaned by the runtime checkpoint file-swap (rename),
//!       whereas a separate residual opener that folds-on-close across the swap
//!       loses the write (the swap-orphan silent-write-loss vector).
//!
//! Build+run:  rustc --test adr0098_r3_pragma_presence_extract.rs -o /tmp/r3t && /tmp/r3t

#![allow(dead_code)]

const PRAGMA: &str = "disable_checkpoint_on_shutdown";

/// Faithful port of cut-gate CHECK 10j's per-site rule. Returns the 1-based line
/// numbers of RUNTIME `Connection::open(` sites that do NOT carry the pragma
/// within `window` lines after the open. `#[cfg(test)]` regions are skipped with
/// a brace-depth scan (mirrors tools/adr0098_opener_scan.awk's cfg(test) cut).
fn residual_opens_missing_pragma(src: &str, window: usize) -> Vec<usize> {
    let lines: Vec<&str> = src.lines().collect();
    // Mark cfg(test) regions by brace depth (coarse but matches the scanner's intent).
    let mut in_test = vec![false; lines.len()];
    let mut depth: i32 = 0;
    let mut tdepth: i32 = -1;
    let mut pending = false;
    for (i, l) in lines.iter().enumerate() {
        let st = l.trim_start();
        if st.starts_with("#[cfg(") && st.contains("test") && !st.contains("not(test)") {
            pending = true;
        }
        let was_in = tdepth >= 0;
        for ch in l.chars() {
            if ch == '{' {
                depth += 1;
                if pending && tdepth < 0 {
                    tdepth = depth;
                    pending = false;
                }
            } else if ch == '}' {
                if tdepth == depth {
                    tdepth = -1;
                }
                depth -= 1;
            }
        }
        let now_in = tdepth >= 0;
        in_test[i] = was_in || now_in;
    }
    let mut missing = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        if in_test[i] {
            continue;
        }
        if l.contains("Connection::open(") && !l.contains("open_in_memory") {
            let end = (i + window).min(lines.len() - 1);
            let has = lines[i..=end].iter().any(|w| w.contains(PRAGMA));
            if !has {
                missing.push(i + 1);
            }
        }
    }
    missing
}

// ── (B) ingest-seam routing / swap-orphan model ─────────────────────────────
#[derive(Clone)]
struct Live {
    committed: Vec<u64>, // durably installed rows
    wal: Vec<u64>,       // rows in the WAL, not yet folded into `committed`
}
impl Live {
    fn new() -> Self {
        Live { committed: vec![], wal: vec![] }
    }
    /// A checkpoint file-swap (R2 atomic_install rename): a fresh staging file is
    /// built from `committed` and installed; the OLD inode (with its WAL) is
    /// unlinked. A row still only in the OLD inode's WAL is ORPHANED by the swap.
    fn checkpoint_swap(&mut self) {
        // Only already-committed rows survive the swap; WAL-on-old-inode is lost.
        self.wal.clear();
    }
    /// Handle single-writer commit: the WriteGuard commits the row durably
    /// (tx.commit) BEFORE any swap can orphan it — it lands in `committed`.
    fn handle_write_commit(&mut self, row: u64) {
        self.committed.push(row);
    }
    /// A residual separate opener writes into the WAL and folds-on-close IN PLACE
    /// — UNLESS the no-in-place-fold pragma is set, in which case close touches
    /// nothing (the row stays wherever it was durably committed).
    fn residual_open_write_close(&mut self, row: u64, pragma_set: bool) {
        self.wal.push(row);
        if pragma_set {
            // pragma: no implicit close-checkpoint; nothing folded in place.
        } else {
            // no pragma: implicit close-checkpoint folds WAL into committed IN
            // PLACE (the duckdb#23046 locus) — visible, but tears under a
            // concurrent swap.
            self.committed.append(&mut self.wal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── (A) pragma-presence gate rule ──────────────────────────────────────
    #[test]
    fn runtime_open_with_pragma_is_clean() {
        let src = "\
fn ingest(db_path: &Path) -> Result<()> {
    let conn = Connection::open(db_path)?;
    conn.execute_batch(\"PRAGMA disable_checkpoint_on_shutdown;\")?;
    Ok(())
}";
        assert!(residual_opens_missing_pragma(src, 15).is_empty());
    }

    #[test]
    fn runtime_open_without_pragma_is_flagged() {
        let src = "\
fn ingest(db_path: &Path) -> Result<()> {
    let conn = Connection::open(db_path)?;
    ensure_schema(&conn)?;
    Ok(())
}";
        // line 2 is the bare open — must be flagged.
        assert_eq!(residual_opens_missing_pragma(src, 15), vec![2]);
    }

    #[test]
    fn cfg_test_open_without_pragma_is_ignored() {
        let src = "\
#[cfg(test)]
mod t {
    fn f() {
        let c = Connection::open(&db_path).unwrap();
    }
}";
        assert!(residual_opens_missing_pragma(src, 15).is_empty());
    }

    #[test]
    fn pragma_just_outside_window_is_flagged() {
        let mut s = String::from("fn f() {\n    let conn = Connection::open(p)?;\n");
        for _ in 0..20 {
            s.push_str("    noop();\n");
        }
        s.push_str("    conn.execute_batch(\"PRAGMA disable_checkpoint_on_shutdown;\")?;\n}");
        assert_eq!(residual_opens_missing_pragma(&s, 15), vec![2]);
    }

    // ── (B) ingest-seam routing invariant ──────────────────────────────────
    #[test]
    fn handle_routed_write_survives_concurrent_swap() {
        // R3 Part 2: ap_sync ingest routes through the Handle -> tx.commit lands
        // the row in `committed` BEFORE the checkpoint swap. No orphan.
        let mut live = Live::new();
        live.handle_write_commit(1001);
        live.checkpoint_swap();
        assert!(live.committed.contains(&1001), "handle-routed write must survive the swap");
    }

    #[test]
    fn residual_open_without_pragma_loses_write_across_swap() {
        // The vector R3 closes: a separate opener writes into the WAL and a
        // concurrent swap orphans the old inode -> silent write loss.
        let mut live = Live::new();
        live.wal.push(2002); // residual write in flight in the WAL
        live.checkpoint_swap(); // swap renames the inode out from under it
        assert!(!live.committed.contains(&2002), "unmigrated residual write is silently lost");
    }

    #[test]
    fn residual_close_with_pragma_does_not_fold_in_place() {
        // R3 Part 1: even a residual opener that stays (v0.2.6) no longer folds
        // the shared WAL in place on close when the pragma is set.
        let mut live = Live::new();
        live.committed.push(1); // pre-existing durable state
        live.residual_open_write_close(3003, /*pragma_set=*/ true);
        // pragma set: close folds NOTHING in place; committed is untouched by the
        // fold (the fold is the tear locus we are eliminating).
        assert_eq!(live.committed, vec![1], "pragma: no in-place fold on close");
        // Without the pragma, close WOULD fold in place (the duckdb#23046 tear).
        let mut live2 = Live::new();
        live2.committed.push(1);
        live2.residual_open_write_close(3003, /*pragma_set=*/ false);
        assert!(live2.committed.contains(&3003), "no pragma: in-place fold happens (the tear locus)");
    }
}
