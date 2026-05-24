//! Boot-time budget pin for `aberp serve` (PR-45a / session-61).
//!
//! ENV-GATED. Body runs only when `ABERP_BOOT_BUDGET_TEST=1` is set;
//! otherwise the test returns early. Matches the env-gating posture
//! of the existing live tests in this directory (e.g.
//! `submit_invoice_live.rs`) so CI does not need a populated
//! keychain and offline contributors do not have a flaky-by-design
//! test.
//!
//! # What this pins
//!
//! Pre-PR-45a, `aberp serve` synchronously read `current_exe()` and
//! SHA-256'd it between the loopback-cert log and the listener-bind
//! log. On a cold disk cache that took 5-10s, which blew the
//! desktop Tauri shell's 10s `HANDSHAKE_TIMEOUT` and left the
//! operator looking at a blank window. PR-45a moved the
//! binary-hash compute onto a background OS thread; the
//! `aberp serve` cold-start to the `READY 127.0.0.1:<port>` line
//! should now resolve in under 2 seconds even on a fresh launch.
//!
//! This test is the regression net for that budget.
//!
//! # Required environment when ABERP_BOOT_BUDGET_TEST=1
//!
//!   ABERP_BOOT_BUDGET_TEST=1
//!   ABERP_BOOT_BUDGET_TENANT=<tenant id whose keychain is populated>
//!     (NAV credentials + session token entries must exist; run
//!     `aberp setup-nav-credentials --tenant <id>` and a one-shot
//!     `aberp serve --tenant <id>` first to mint both).
//!
//! Optional:
//!
//!   ABERP_BOOT_BUDGET_MS=<u64 milliseconds budget; default 2000>
//!   ABERP_BOOT_BUDGET_DB=<absolute path to the per-tenant DuckDB
//!     file the test should use; default uses a per-process temp
//!     path so a stale lock from the previous test run doesn't
//!     bleed in>.
//!
//! # Why we test the READY line directly
//!
//! The README-line shape is the same shape the Tauri shell parses
//! at boot — pinning that boot-time-to-handshake matches the
//! operator-perceived launch latency exactly. The handshake-parser
//! conformance lives in `apps/aberp-ui/tests/handshake_parse.rs`;
//! this test pins the producer side's timing.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn aberp_serve_cold_start_to_ready_is_within_budget() {
    if std::env::var("ABERP_BOOT_BUDGET_TEST").ok().as_deref() != Some("1") {
        eprintln!(
            "skipping aberp_serve_cold_start_to_ready_is_within_budget: \
             ABERP_BOOT_BUDGET_TEST not set"
        );
        return;
    }
    let tenant = std::env::var("ABERP_BOOT_BUDGET_TENANT").expect(
        "ABERP_BOOT_BUDGET_TENANT must be set when ABERP_BOOT_BUDGET_TEST=1 \
         (point at a tenant whose keychain has NAV credentials and a session token)",
    );
    let budget_ms: u64 = std::env::var("ABERP_BOOT_BUDGET_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000);
    let budget = Duration::from_millis(budget_ms);

    let db_path: PathBuf = match std::env::var("ABERP_BOOT_BUDGET_DB").ok() {
        Some(p) => PathBuf::from(p),
        None => {
            let mut p = std::env::temp_dir();
            p.push(format!(
                "aberp-boot-budget-{}-{}.duckdb",
                std::process::id(),
                ulid::Ulid::new(),
            ));
            p
        }
    };

    // CARGO_BIN_EXE_aberp is set by Cargo when running integration
    // tests in this crate. It points at the compiled `aberp` binary
    // — the same one the Tauri shell resolves at runtime.
    let aberp_bin = env!("CARGO_BIN_EXE_aberp");

    let started = Instant::now();
    let mut child = Command::new(aberp_bin)
        .arg("serve")
        .arg("--tenant")
        .arg(&tenant)
        .arg("--db")
        .arg(&db_path)
        .arg("--port")
        .arg("0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn aberp serve subprocess for boot-budget pin");

    let stdout = child
        .stdout
        .take()
        .expect("aberp serve subprocess stdout pipe");

    let mut ready_line: Option<String> = None;
    let mut elapsed: Option<Duration> = None;
    let mut reader = BufReader::new(stdout);

    // Read lines until we see the READY handshake or the budget
    // expires. We intentionally do not enforce the budget inside the
    // read loop — if the process is slow we want a clear assertion
    // failure with the *actual* time, not "read timed out".
    let mut line_buf = String::new();
    while ready_line.is_none() {
        line_buf.clear();
        let bytes = reader
            .read_line(&mut line_buf)
            .expect("read stdout of aberp serve");
        if bytes == 0 {
            // EOF — backend died.
            break;
        }
        let trimmed = line_buf.trim();
        if trimmed.starts_with("READY ") {
            elapsed = Some(started.elapsed());
            ready_line = Some(trimmed.to_string());
        }
        // Safety net: if we've been reading for > 30s, bail with a
        // diagnostic before this dominates the test runner clock.
        if started.elapsed() > Duration::from_secs(30) {
            break;
        }
    }

    // Tear the subprocess down before the assertion so a failure
    // doesn't leave a stray listener bound.
    let _ = child.kill();
    let _ = child.wait();

    let ready_line = ready_line.expect(
        "aberp serve must emit a `READY 127.0.0.1:<port> sha256:<hex>` line within \
         30s; it never did. Either the boot path stalled OR the printed line shape \
         drifted away from the handshake contract (see `apps/aberp-ui/src/handshake.rs`).",
    );
    let elapsed = elapsed.expect("ready_line is Some implies elapsed is Some");

    assert!(
        ready_line.starts_with("READY 127.0.0.1:"),
        "READY line shape regressed; got `{}`",
        ready_line
    );
    assert!(
        elapsed <= budget,
        "aberp serve cold-start to READY was {:?}; budget is {:?}. \
         Pre-PR-45a binary_hash::compute() (running synchronously on the boot \
         path) took 5-10s on a cold disk cache; a regression that re-introduces \
         a synchronous slow step would surface here. The READY line we got was \
         `{}`.",
        elapsed,
        budget,
        ready_line,
    );
}
