//! S270 / PR-259 — `aberp-cad-extract-wrapper`, the Rust subprocess
//! shim around the Python `aberp-cad-extract` CLI (S269 / PR-258).
//!
//! ## What this crate is
//!
//! A single deterministic blocking function: [`extract`]. Given an
//! [`ExtractRequest`] (path to a CAD file + a material grade as it
//! appears in `quoting_materials.grade`), it spawns the Python
//! extractor, captures its stdout, parses the JSON into a
//! [`FeatureGraph`] (re-exported from `aberp-quote-engine` — we do
//! **not** duplicate the wire-contract struct), pins the
//! `_schema_version` field against [`EXPECTED_SCHEMA_VERSION`], and
//! returns the validated graph.
//!
//! The narrow waist between the Python geometry world and the Rust
//! scoring crate. Everything subprocess-y stops here.
//!
//! ## Architecture: where this sits
//!
//! ```text
//!  storefront CAD upload
//!       │
//!       ▼
//!  aberp-cad-extract (Python CLI, S269)
//!       │   stdout: feature-graph JSON  ◄── parsed here
//!       ▼
//!  aberp-cad-extract-wrapper (THIS CRATE, S270)
//!       │   validated FeatureGraph
//!       ▼
//!  aberp-quote-engine (pure-function scoring, S268)
//!       │   QuoteBreakdown
//!       ▼
//!  apps/aberp daemon (S271) — persist, audit, email indicative
//! ```
//!
//! ## Why subprocess, not PyO3
//!
//! - **Portability.** No Python ABI lock; a Python minor-version
//!   upgrade on the operator's machine does not force a rebuild of
//!   the Rust workspace.
//! - **Process isolation.** OCCT crashes (PR-273 wired STEP support
//!   through cadquery-ocp) do not take down the ABERP daemon. The
//!   wrapper surfaces an [`ExtractError::NonZeroExit`] and the daemon
//!   moves on.
//! - **Latency budget.** Spawn cost is ~50–100 ms; the operator-
//!   click-to-quote flow targets 1–2 seconds, so the cold-start tax
//!   is noise. If a per-machine extractor pool ever becomes the
//!   bottleneck, the right answer is a long-lived Python worker over
//!   a UNIX socket — not PyO3.
//!
//! ## What this crate is NOT
//!
//! - **No retry logic.** Caller decides retry policy. The wrapper is
//!   blunt: spawn, wait, parse, return — or fail loud. This matches
//!   [[trust-code-not-operator]]: retry is a policy decision that
//!   belongs at the daemon layer where the operator can configure it.
//! - **No Python-version detection.** Caller passes `python_bin`
//!   pointing at a Python ≥ 3.11 interpreter that has the
//!   `aberp_cad_extract` module installed (e.g. via `pip install -e`).
//!   A wrong version surfaces as [`ExtractError::ModuleNotFound`] or
//!   a useful stderr inside [`ExtractError::NonZeroExit`].
//! - **No virtualenv plumbing.** Auto-discovery of the right
//!   interpreter is an app-integration concern (S271), not a wrapper
//!   concern. Several callers may want different policies (system
//!   python, bundled venv, conda); the wrapper stays neutral.
//! - **No global state.** [`CadExtractor`] instances are cheap;
//!   construct one per call or reuse — either is fine.
//! - **No async.** v1 is blocking. If a future caller wants async,
//!   the right move is a `tokio` Cargo feature that swaps
//!   `std::process::Command` for `tokio::process::Command` behind the
//!   same public API. The S271 daemon is sync at the quote-engine
//!   boundary anyway; no benefit yet per CLAUDE.md rule 2.
//!
//! ## Schema versioning
//!
//! [`EXPECTED_SCHEMA_VERSION`] pins the Python-side schema this build
//! of the wrapper understands. When S269+ emits v2, bump the constant
//! in this crate in the same diff. The mismatch error
//! ([`ExtractError::SchemaVersionMismatch`]) names both values so the
//! operator can read it without guessing.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use thiserror::Error;

pub use aberp_quote_engine::FeatureGraph;

/// The Python-side `_schema_version` value this build of the wrapper
/// accepts. Bump in the same diff as the Python extractor when the
/// schema evolves; [`ExtractError::SchemaVersionMismatch`] surfaces a
/// mismatch loud rather than silently mis-parsing fields.
pub const EXPECTED_SCHEMA_VERSION: u32 = 1;

/// Default subprocess timeout. The operator-click-to-quote flow
/// targets 1–2 seconds end-to-end; 30 seconds is the "something is
/// genuinely wrong" ceiling. Caller can override via
/// [`CadExtractor::with_timeout`].
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Configured subprocess invoker.
///
/// Cheap to construct, cheap to clone — reuse across calls or build
/// one per call. No global state, no caches, no internal mutability.
///
/// Builder-style setters return `Self` so a one-shot call site can
/// chain: `CadExtractor::new().with_timeout(Duration::from_secs(5))`.
#[derive(Debug, Clone)]
pub struct CadExtractor {
    python_bin: PathBuf,
    module: String,
    timeout: Duration,
}

impl Default for CadExtractor {
    fn default() -> Self {
        Self {
            python_bin: PathBuf::from("python3"),
            module: "aberp_cad_extract".to_string(),
            timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl CadExtractor {
    /// Construct an extractor with the defaults: `python3` on `$PATH`,
    /// module name `aberp_cad_extract`, 30-second timeout.
    pub fn new() -> Self {
        Self::default()
    }

    /// Override the Python interpreter. Pass an absolute path when the
    /// caller wants to pin a venv (`~/.aberp/venv/bin/python3`).
    pub fn with_python_bin(mut self, python_bin: impl Into<PathBuf>) -> Self {
        self.python_bin = python_bin.into();
        self
    }

    /// Override the importable module name. Almost no caller needs
    /// this — exposed for the test suite, where a stub-python script
    /// gets pointed at a synthetic module to exercise error paths
    /// without the real geometry deps.
    pub fn with_module(mut self, module: impl Into<String>) -> Self {
        self.module = module.into();
        self
    }

    /// Override the subprocess timeout. The child is killed and
    /// [`ExtractError::Timeout`] returned if the deadline is exceeded.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Spawn the Python extractor, parse its stdout, validate the
    /// schema version, and return the [`FeatureGraph`]. See
    /// [`ExtractError`] for the failure taxonomy.
    pub fn extract(&self, req: &ExtractRequest) -> Result<FeatureGraph, ExtractError> {
        if !req.input_path.exists() {
            return Err(ExtractError::InputFileNotFound(req.input_path.clone()));
        }

        let mut child = Command::new(&self.python_bin)
            .arg("-m")
            .arg(&self.module)
            .arg(&req.input_path)
            .arg("--material-grade")
            .arg(&req.material_grade)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ExtractError::PythonNotFound
                } else {
                    ExtractError::Spawn(e.to_string())
                }
            })?;

        let stdout = child.stdout.take().expect("piped stdout was requested");
        let stderr = child.stderr.take().expect("piped stderr was requested");

        let status = match wait_with_timeout(&mut child, self.timeout) {
            Some(s) => s.map_err(|e| ExtractError::Spawn(e.to_string()))?,
            None => {
                let _ = child.kill();
                // Drain so the kernel buffer doesn't pin descriptors.
                let _ = child.wait();
                return Err(ExtractError::Timeout(self.timeout));
            }
        };

        let stdout_text = read_to_string(stdout);
        let stderr_text = read_to_string(stderr);

        if !status.success() {
            // Distinguish "module not importable" from generic
            // non-zero exits: Python writes `No module named '<name>'`
            // to stderr at exit-code 1, and that's an actionable
            // operator-side fix (install the package). Generic non-
            // zero (a runtime ValueError, an OCCT crash) carries the
            // stderr verbatim so the daemon's audit entry has the
            // raw diagnostic.
            if stderr_text.contains("No module named") {
                return Err(ExtractError::ModuleNotFound {
                    module: self.module.clone(),
                    stderr: stderr_text,
                });
            }
            return Err(ExtractError::NonZeroExit {
                code: status.code(),
                stderr: stderr_text,
            });
        }

        let graph: FeatureGraph =
            serde_json::from_str(&stdout_text).map_err(|e| ExtractError::MalformedJson {
                stdout: stdout_text.clone(),
                error: e,
            })?;

        if graph.schema_version != EXPECTED_SCHEMA_VERSION {
            return Err(ExtractError::SchemaVersionMismatch {
                expected: EXPECTED_SCHEMA_VERSION,
                got: graph.schema_version,
            });
        }

        Ok(graph)
    }
}

/// What the caller hands to [`CadExtractor::extract`].
///
/// `material_grade` is operator-supplied at quote time (STL has no
/// material metadata; even STEP rarely does in customer uploads). The
/// Python extractor passes it through verbatim into the FeatureGraph;
/// the quote engine validates it against `quoting_materials.grade`.
#[derive(Debug, Clone)]
pub struct ExtractRequest {
    /// Absolute or working-dir-relative path to the input CAD file.
    /// Both `.stl` and `.step`/`.stp` are supported as of PR-273. STEP
    /// requires the Python `[step]` extra (cadquery-ocp); when that
    /// extra is absent the Python extractor exits with a "not yet
    /// implemented in this build" message that surfaces as
    /// [`ExtractError::NonZeroExit`] and classifies Permanent on the
    /// daemon side.
    pub input_path: PathBuf,
    /// Material grade as it appears in `quoting_materials.grade` —
    /// e.g. `6061-T6`.
    pub material_grade: String,
}

/// Failure taxonomy for [`CadExtractor::extract`].
///
/// Each variant is reachable by exactly one observable failure mode;
/// the daemon (S271) selects audit-event severity per variant and
/// renders the SPA error toast per variant. Don't merge variants.
#[derive(Debug, Error)]
pub enum ExtractError {
    /// `python_bin` is not on `$PATH` (or is not an executable file
    /// when an absolute path was given). Returned when the OS spawn
    /// itself fails with `ErrorKind::NotFound`.
    #[error("python binary not found (check `python_bin` and PATH)")]
    PythonNotFound,

    /// The Python interpreter ran but could not import the named
    /// module — typically because `pip install -e python/aberp-cad-extract`
    /// was never run in the configured interpreter's environment.
    /// `stderr` carries the raw Python `ModuleNotFoundError` line so
    /// the operator can see the exact missing name.
    #[error("python module '{module}' not importable: {stderr}")]
    ModuleNotFound {
        /// The module name the wrapper passed to `python -m`.
        module: String,
        /// Verbatim stderr from the Python interpreter — for the
        /// audit entry and the SPA error toast.
        stderr: String,
    },

    /// The input CAD file did not exist when the wrapper checked, just
    /// before spawn. The check is a cheap pre-flight; the Python side
    /// would also catch this and exit 2, but failing here saves the
    /// subprocess spawn (~50–100 ms).
    #[error("input CAD file not found: {0}")]
    InputFileNotFound(PathBuf),

    /// The configured timeout was exceeded. The wrapper has already
    /// killed the child and reaped its exit status before returning.
    #[error("subprocess exceeded timeout of {0:?}")]
    Timeout(Duration),

    /// The subprocess exited non-zero for a reason that is not module-
    /// not-found — a Python-side `ValueError` for an unknown extension
    /// or a malformed STEP file, a `NotImplementedError` for the
    /// "OCP not installed in this build" path, or any future runtime
    /// crash. `stderr` carries the Python-side structured error JSON
    /// the CLI writes per `cli.py`.
    #[error("subprocess exited with code {code:?}: {stderr}")]
    NonZeroExit {
        /// Exit code, or `None` if the child was killed by signal.
        code: Option<i32>,
        /// Verbatim stderr.
        stderr: String,
    },

    /// The subprocess exited 0 but stdout was not parseable as a
    /// [`FeatureGraph`]. `stdout` is captured verbatim so the audit
    /// entry can include the bytes that broke the parse; `error` is
    /// the [`serde_json`] error with column/line.
    #[error("stdout was not valid FeatureGraph JSON: {error}")]
    MalformedJson {
        /// Verbatim stdout text the wrapper tried to parse.
        stdout: String,
        /// Source serde error with column/line info.
        #[source]
        error: serde_json::Error,
    },

    /// JSON parsed cleanly but the `_schema_version` did not match
    /// [`EXPECTED_SCHEMA_VERSION`]. Either the Python extractor was
    /// upgraded without a matching Rust bump, or the wrapper is stale.
    #[error("FeatureGraph _schema_version mismatch: expected {expected}, got {got}")]
    SchemaVersionMismatch {
        /// The constant this build of the wrapper accepts.
        expected: u32,
        /// The value carried by the JSON we just parsed.
        got: u32,
    },

    /// The OS refused to spawn the child for a reason other than
    /// "binary not found" — out of memory, file-descriptor exhaustion,
    /// sandbox denial, … . Surfaces the raw OS error message; rare in
    /// practice but reachable.
    #[error("subprocess spawn failed: {0}")]
    Spawn(String),
}

/// Poll-until-exit with a wall-clock deadline.
///
/// `std::process::Child` has `try_wait` (non-blocking) and `wait`
/// (blocks forever); composing them with a 10 ms sleep gives a simple
/// timeout primitive that does not require tokio. The 10 ms granularity
/// is fine for the 30-second default — the operator-perceived flow
/// already dominates over polling jitter.
fn wait_with_timeout(
    child: &mut Child,
    timeout: Duration,
) -> Option<std::io::Result<std::process::ExitStatus>> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(Ok(status)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Some(Err(e)),
        }
    }
}

fn read_to_string<R: Read>(mut reader: R) -> String {
    let mut buf = String::new();
    // The Python CLI writes ≤ a few KB to stdout/stderr; `read_to_string`
    // is the simplest correct shape. Lossy on non-UTF8 — but the CLI
    // emits JSON only, which is ASCII-clean.
    let _ = reader.read_to_string(&mut buf);
    buf
}

/// Convenience top-level form: `extract(&req)` against a default
/// [`CadExtractor`]. The struct form is preferred for any caller that
/// configures a non-default interpreter or timeout.
pub fn extract(req: &ExtractRequest) -> Result<FeatureGraph, ExtractError> {
    CadExtractor::new().extract(req)
}

/// Crate version stamp emitted on every breakdown / audit entry, same
/// posture as [`aberp_quote_engine::ENGINE_VERSION`]. The S271 daemon
/// records this on the `quotes` row so a future
/// "priced by extractor-wrapper v0.0.0" PDF stamp is one read away.
pub const WRAPPER_VERSION: &str = env!("CARGO_PKG_VERSION");
