//! PR-56 / session-76 pin tests — `submit_from_inputs` and
//! `poll_ack_from_inputs` MUST be `pub async fn` (NOT sync helpers
//! that build their own tokio runtime internally).
//!
//! # Why this PR exists
//!
//! Pre-PR-56 both library helpers were sync `pub fn`s that built a
//! per-call `tokio::runtime::Builder::new_current_thread().build()`
//! and `block_on`'d their NAV awaits inside. That works for the CLI
//! (which calls them from `main`, outside any runtime), but the
//! session-60 `POST /invoices/:id/submit` + `POST /invoices/:id/poll-ack`
//! routes call the same helpers from inside the axum handler's
//! already-running tokio runtime — at which point `Runtime::build()`
//! itself succeeds but `block_on` panics with:
//!
//!   thread 'tokio-rt-worker' panicked:
//!   Cannot start a runtime from within a runtime.
//!
//! Session-76 / PR-56 fix: lift both helpers to `pub async fn`.
//! The CLI's `submit_invoice::run` and `poll_ack::run` build the
//! runtime at the top of `run` (the right spot — outside any
//! pre-existing runtime), then `block_on` the async helper exactly
//! once. The SPA route handlers `.await` the helper directly.
//!
//! # What this file pins
//!
//! Source-text invariant — A151 source-grep posture (see session 55):
//!
//!   1. `submit_invoice::submit_from_inputs` is declared
//!      `pub async fn` (NOT `pub fn`).
//!   2. `poll_ack::poll_ack_from_inputs` is declared `pub async fn`
//!      (NOT `pub fn`).
//!
//! A future regression that flattens either back to a sync helper
//! re-introduces the nested-runtime panic on every SPA-side submit
//! or poll-ack and loud-fails here at compile + test time, BEFORE
//! the operator hits it in the running app.

const SUBMIT_INVOICE_SRC: &str = include_str!("../src/submit_invoice.rs");
const POLL_ACK_SRC: &str = include_str!("../src/poll_ack.rs");

#[test]
fn submit_from_inputs_is_async_fn() {
    assert!(
        SUBMIT_INVOICE_SRC.contains("pub async fn submit_from_inputs("),
        "submit_invoice::submit_from_inputs MUST be declared `pub async fn` \
         (PR-56 / session-76 invariant). A sync helper would have to build \
         its own tokio runtime to drive the two NAV awaits inside, which \
         panics when the SPA route handler calls it from within axum's \
         already-running runtime: `Cannot start a runtime from within a \
         runtime`. The CLI's `submit_invoice::run` now owns the runtime \
         at the top of `run` and `block_on`s this helper exactly once."
    );
    // Defence-in-depth: the OLD shape (sync helper, sync `pub fn`
    // signature) must not coexist with the new async one. A partial
    // revert that adds back the old name as a sync wrapper would
    // be a regression we want to surface here.
    assert!(
        !SUBMIT_INVOICE_SRC.contains("pub fn submit_from_inputs("),
        "submit_invoice::submit_from_inputs MUST NOT be declared as a \
         sync `pub fn`. PR-56 lifted it to `pub async fn`; a sync wrapper \
         under the same name would re-introduce the nested-runtime panic."
    );
}

#[test]
fn poll_ack_from_inputs_is_async_fn() {
    assert!(
        POLL_ACK_SRC.contains("pub async fn poll_ack_from_inputs("),
        "poll_ack::poll_ack_from_inputs MUST be declared `pub async fn` \
         (PR-56 / session-76 invariant). Same nested-runtime hazard as \
         `submit_from_inputs` — the SPA's `POST /invoices/:id/poll-ack` \
         handler calls this helper from axum's runtime; a sync wrapper \
         that built its own runtime to drive `poll_loop` would panic on \
         every SPA-driven poll."
    );
    assert!(
        !POLL_ACK_SRC.contains("pub fn poll_ack_from_inputs("),
        "poll_ack::poll_ack_from_inputs MUST NOT be declared as a sync \
         `pub fn`. PR-56 lifted it to `pub async fn`; a sync wrapper \
         under the same name would re-introduce the nested-runtime panic."
    );
}

/// Defence-in-depth: the body of `submit_from_inputs` (the library
/// helper, NOT the CLI's `run` wrapper) must not construct a tokio
/// runtime. The CLI's `run` correctly builds one at the top — exactly
/// one occurrence of `Builder::new_current_thread()` is permitted in
/// the module (the CLI top-level build); a second occurrence would
/// be the regression.
#[test]
fn submit_invoice_module_builds_at_most_one_runtime() {
    let count = SUBMIT_INVOICE_SRC.matches("new_current_thread()").count();
    assert!(
        count <= 1,
        "submit_invoice.rs constructs `new_current_thread()` {count} times; \
         PR-56 invariant allows AT MOST 1 (the CLI's top-level `run` build). \
         A second occurrence is almost certainly a regression that pushed \
         a runtime build back inside `submit_from_inputs`, which panics \
         when the SPA route handler awaits it."
    );
}

#[test]
fn poll_ack_module_builds_at_most_one_runtime() {
    let count = POLL_ACK_SRC.matches("new_current_thread()").count();
    assert!(
        count <= 1,
        "poll_ack.rs constructs `new_current_thread()` {count} times; \
         PR-56 invariant allows AT MOST 1 (the CLI's top-level `run` build). \
         A second occurrence is almost certainly a regression that pushed \
         a runtime build back inside `poll_ack_from_inputs`, which panics \
         when the SPA route handler awaits it."
    );
}
