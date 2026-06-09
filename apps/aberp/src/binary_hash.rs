//! Compute the SHA-256 of the running binary at process start, per
//! ADR-0008 §"Entry shape": "binary_hash — SHA-256 of the binary that
//! produced the entry (recorded once per process start; referenced)".
//!
//! # Why a background handle (PR-45a / session-61)
//!
//! On a fresh launch with a cold disk cache, reading a debug `aberp`
//! binary (several hundred MB once Tauri + reqwest + rustls + duckdb
//! + …  are in the picture) and SHA-256-ing it takes 5-10 seconds.
//! `aberp serve` used to do this synchronously between the
//! `loopback TLS certificate ready` log and the `starting HTTPS
//! loopback listener` log; the desktop Tauri shell's
//! `HANDSHAKE_TIMEOUT` (10s) then fired before the listener was
//! bound, and the shell exited.
//!
//! The serve path now spawns this compute on a background OS thread
//! at startup and stashes a [`BinaryHashHandle`] on `AppState`.
//! Request handlers call [`BinaryHashHandle::wait`] when they need
//! the hash; by the time any HTTP request lands the compute is
//! virtually always already done. If a handler arrives first (rare),
//! `wait` blocks on a `Condvar` rather than spin-polling. If the
//! compute itself failed (e.g. macOS sandbox denial), `wait` returns
//! the error — loud per CLAUDE.md rule 12.

use std::fs;
use std::io;
use std::sync::{Arc, Condvar, Mutex};

use aberp_audit_ledger::BinaryHash;
use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

/// Compute the SHA-256 of `std::env::current_exe()` and wrap it in
/// [`BinaryHash`]. On failure (e.g. macOS sandbox where the exe path is
/// inaccessible), returns the I/O error to the caller; ADR-0008 makes
/// the hash a hard requirement, so this is fail-loud per ADR-0007.
pub fn compute() -> io::Result<BinaryHash> {
    let path = std::env::current_exe()?;
    let bytes = fs::read(&path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash: [u8; 32] = hasher.finalize().into();
    Ok(BinaryHash::from_bytes(hash))
}

/// Handle to a background `binary_hash::compute()` task. Cloneable
/// across threads (cheap `Arc` clone). The single computation is
/// shared by every clone.
#[derive(Clone)]
pub struct BinaryHashHandle {
    inner: Arc<(Mutex<Option<Result<BinaryHash, String>>>, Condvar)>,
}

impl BinaryHashHandle {
    /// Spawn an OS thread that calls [`compute`] and parks the result.
    /// Returns immediately. Consumers call [`wait`] to read the
    /// result, blocking until the background thread finishes.
    pub fn start_background() -> Self {
        let inner = Arc::new((Mutex::new(None), Condvar::new()));
        let inner_for_thread = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("aberp-binary-hash".to_string())
            .spawn(move || {
                let started = std::time::Instant::now();
                let result = compute().map_err(|e| e.to_string());
                let elapsed_ms = started.elapsed().as_millis();
                match &result {
                    Ok(_) => tracing::info!(
                        elapsed_ms = elapsed_ms as u64,
                        "binary hash compute (background) ready"
                    ),
                    Err(e) => tracing::error!(
                        elapsed_ms = elapsed_ms as u64,
                        error = %e,
                        "binary hash compute (background) failed"
                    ),
                }
                let (lock, cvar) = &*inner_for_thread;
                *lock.lock().expect("binary-hash mutex poisoned") = Some(result);
                cvar.notify_all();
            })
            .expect("spawn aberp-binary-hash worker thread");
        BinaryHashHandle { inner }
    }

    /// Construct a handle that already holds a precomputed hash —
    /// used by unit tests that build `AppState` without launching the
    /// serve subprocess.
    pub fn from_ready(hash: BinaryHash) -> Self {
        let inner = Arc::new((Mutex::new(Some(Ok(hash))), Condvar::new()));
        BinaryHashHandle { inner }
    }

    /// Block until the background compute finishes; return the hash.
    /// On compute failure, returns the error verbatim — surfaced to
    /// the HTTP handler so the operator sees the real cause rather
    /// than a silent fallback.
    pub fn wait(&self) -> Result<BinaryHash> {
        let (lock, cvar) = &*self.inner;
        let mut guard = lock.lock().expect("binary-hash mutex poisoned");
        while guard.is_none() {
            guard = cvar.wait(guard).expect("binary-hash condvar wait poisoned");
        }
        match guard.as_ref().expect("just-checked is_some") {
            Ok(h) => Ok(*h),
            Err(e) => Err(anyhow!("binary hash compute failed in background: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_ready_returns_the_provided_hash() {
        let original = BinaryHash::from_bytes([0x42; 32]);
        let handle = BinaryHashHandle::from_ready(original);
        let read = handle.wait().expect("from_ready handle must yield Ok");
        assert_eq!(read, original);
    }

    #[test]
    fn from_ready_is_cloneable_and_consistent_across_clones() {
        let original = BinaryHash::from_bytes([0x7e; 32]);
        let a = BinaryHashHandle::from_ready(original);
        let b = a.clone();
        assert_eq!(a.wait().unwrap(), original);
        assert_eq!(b.wait().unwrap(), original);
    }

    #[test]
    fn start_background_compute_eventually_returns_a_hash() {
        // We can't pin the hash value without re-hashing the test
        // binary, but we can confirm the background path returns a
        // 32-byte hash within a reasonable budget. The test binary is
        // small; this should resolve in well under 5s on Apple Silicon
        // dev hardware. The 120s upper bound is a watchdog tuned for
        // GitHub Actions ubuntu-latest, where SHA-256-ing a debug
        // workspace binary on a virtualised disk has been observed at
        // ~37s (S303). The bound exists to catch a real hang/deadlock,
        // not to assert perf.
        let handle = BinaryHashHandle::start_background();
        let started = std::time::Instant::now();
        let result = handle.wait();
        let elapsed = started.elapsed();
        let h = result.expect("background compute over test binary must succeed");
        assert_eq!(h.as_bytes().len(), 32);
        assert!(
            elapsed < std::time::Duration::from_secs(120),
            "background compute over test binary took {:?}",
            elapsed
        );
    }
}
