//! PR-60 / session-80 pin tests — the MNB-rates provider trait,
//! `issue_from_parsed`, `fetch_and_stamp_rate`, and the SPA-route
//! library helper `issue_invoice_request` MUST stay `async`. None of
//! the modules below may build an internal tokio runtime or call
//! `block_on` (the CLI's `issue_invoice::run` is the single owner of
//! the runtime at the top-level sync boundary — the SPA's axum handler
//! reaches the same pipeline via `.await` on the runtime axum already
//! owns).
//!
//! # Why this PR exists
//!
//! Pre-PR-60 `LiveMnbRatesProvider` constructed a current-thread tokio
//! runtime in its `new()` and `block_on`'d the async
//! `MnbClient::fetch_official_rate` per call. That works for the CLI
//! (called from sync `main`, outside any runtime) but the
//! `POST /invoices/issue` route on the EUR branch reached this same
//! provider from inside axum's already-running multi-thread runtime —
//! at which point the nested `block_on` panicked with the structural
//!
//!   thread 'tokio-rt-worker' panicked at apps/aberp/src/mnb_rates_provider.rs:110:
//!   Cannot start a runtime from within a runtime.
//!
//! Identical shape to the PR-56 / session-76 panic on the submit +
//! poll-ack paths. PR-60 closes the deferred follow-up named in
//! session-76's close ("`mnb_rates_provider` carries the identical
//! anti-pattern on the EUR-issuance path").
//!
//! # What this file pins
//!
//! Source-text invariants — A151 source-grep posture (see session 55):
//!
//!   1. `MnbRatesProvider::fetch_official_rate` is declared `async fn`
//!      (under the `#[async_trait]` macro that preserves dyn-
//!      compatibility for the serve route's `Box<dyn MnbRatesProvider>`
//!      consumer).
//!   2. `mnb_rates_provider.rs` does NOT construct a tokio runtime —
//!      no `Builder::new_current_thread(` and no `block_on(` calls.
//!      Both are red flags for a regression that re-introduces the
//!      nested-runtime panic.
//!   3. `issue_invoice::issue_from_parsed` is declared
//!      `pub async fn`.
//!   4. `issue_invoice::fetch_and_stamp_rate` is declared
//!      `pub async fn`.
//!   5. `serve::issue_invoice_request` is declared `pub async fn`.
//!   6. `issue_invoice.rs` constructs `Builder::new_current_thread(`
//!      AT MOST ONCE — that single occurrence is the CLI's top-level
//!      `run` build. A second occurrence is almost certainly a
//!      regression that pushed a runtime build back inside
//!      `run_with_provider` or `issue_from_parsed`, which panics when
//!      the SPA route handler `.await`s the same pipeline.
//!
//! A future regression that flattens any of these back to sync or
//! adds a nested runtime re-introduces the panic on every EUR
//! issuance and loud-fails here at compile + test time, BEFORE the
//! operator hits it in the running app.

const MNB_PROVIDER_SRC: &str = include_str!("../src/mnb_rates_provider.rs");
const ISSUE_INVOICE_SRC: &str = include_str!("../src/issue_invoice.rs");
const SERVE_SRC: &str = include_str!("../src/serve.rs");

#[test]
fn mnb_rates_provider_trait_method_is_async_fn() {
    assert!(
        MNB_PROVIDER_SRC.contains("async fn fetch_official_rate("),
        "MnbRatesProvider::fetch_official_rate MUST be declared `async fn` \
         (PR-60 / session-80 invariant). A sync trait method would force \
         the production impl to build its own tokio runtime and `block_on` \
         the underlying `MnbClient::fetch_official_rate`, which panics \
         when the SPA's EUR issuance route reaches it from inside axum's \
         already-running runtime: `Cannot start a runtime from within a \
         runtime`. The CLI's `issue_invoice::run` now owns the runtime \
         at the top of `run` and `block_on`s `run_with_provider` exactly \
         once; the SPA `.await`s the same async pipeline."
    );
}

#[test]
fn mnb_rates_provider_constructs_no_internal_runtime() {
    assert!(
        !MNB_PROVIDER_SRC.contains("Builder::new_current_thread("),
        "mnb_rates_provider.rs MUST NOT construct a tokio runtime \
         (PR-60 / session-80 invariant). The pre-PR-60 \
         `LiveMnbRatesProvider::new` built a per-instance runtime and \
         `block_on`'d the async MNB fetch per call; the SPA's EUR \
         issuance route panicked when reaching this from axum's \
         already-running runtime. The CLI's `issue_invoice::run` now \
         owns the runtime at the top-level sync boundary; this module \
         must stay runtime-free."
    );
    assert!(
        !MNB_PROVIDER_SRC.contains("block_on("),
        "mnb_rates_provider.rs MUST NOT call `block_on(` \
         (PR-60 / session-80 invariant). Same nested-runtime hazard as \
         above — the async-native impl `.await`s the underlying client \
         directly."
    );
}

#[test]
fn issue_from_parsed_is_async_fn() {
    assert!(
        ISSUE_INVOICE_SRC.contains("pub async fn issue_from_parsed"),
        "issue_invoice::issue_from_parsed MUST be declared `pub async fn` \
         (PR-60 / session-80 invariant). The SPA's `POST /invoices/issue` \
         route calls this helper from inside axum's runtime; a sync \
         wrapper that built its own runtime to drive the MNB fetch's \
         `.await` would panic with the nested-runtime error."
    );
    assert!(
        !ISSUE_INVOICE_SRC.contains("pub fn issue_from_parsed"),
        "issue_invoice::issue_from_parsed MUST NOT be declared as a sync \
         `pub fn`. PR-60 lifted it to `pub async fn`; a sync wrapper \
         under the same name would re-introduce the nested-runtime panic."
    );
}

#[test]
fn fetch_and_stamp_rate_is_async_fn() {
    assert!(
        ISSUE_INVOICE_SRC.contains("pub async fn fetch_and_stamp_rate"),
        "issue_invoice::fetch_and_stamp_rate MUST be declared `pub async fn` \
         (PR-60 / session-80 invariant). This is the walk-back loop that \
         calls `MnbRatesProvider::fetch_official_rate` up to A139's 7-day \
         cap; it must thread `.await` through to the provider."
    );
    assert!(
        !ISSUE_INVOICE_SRC.contains("pub fn fetch_and_stamp_rate"),
        "issue_invoice::fetch_and_stamp_rate MUST NOT be declared as a \
         sync `pub fn`. PR-60 lifted it to `pub async fn`; a sync wrapper \
         under the same name would force a nested `block_on` to drive the \
         async provider, re-introducing the panic."
    );
}

#[test]
fn serve_issue_invoice_request_is_async_fn() {
    assert!(
        SERVE_SRC.contains("pub async fn issue_invoice_request"),
        "serve::issue_invoice_request MUST be declared `pub async fn` \
         (PR-60 / session-80 invariant). The axum handler `.await`s this \
         helper directly; a sync wrapper that built its own runtime to \
         drive `issue_from_parsed` would panic in the nested-runtime \
         shape."
    );
    assert!(
        !SERVE_SRC.contains("pub fn issue_invoice_request"),
        "serve::issue_invoice_request MUST NOT be declared as a sync \
         `pub fn`. PR-60 lifted it to `pub async fn`."
    );
}

/// Defence-in-depth: the body of `issue_from_parsed`, `fetch_and_stamp_rate`,
/// and `run_with_provider` (the library helpers, NOT the CLI's `run`
/// wrapper) must not construct a tokio runtime. The CLI's `run`
/// correctly builds one at the top — exactly one occurrence of
/// `Builder::new_current_thread(` is permitted in the module (the CLI
/// top-level build); a second occurrence would be the regression.
#[test]
fn issue_invoice_module_builds_at_most_one_runtime() {
    let count = ISSUE_INVOICE_SRC
        .matches("Builder::new_current_thread(")
        .count();
    assert!(
        count <= 1,
        "issue_invoice.rs constructs `Builder::new_current_thread(` {count} times; \
         PR-60 invariant allows AT MOST 1 (the CLI's top-level `run` build). \
         A second occurrence is almost certainly a regression that pushed a \
         runtime build back inside `run_with_provider` or `issue_from_parsed` \
         or `fetch_and_stamp_rate`, which panics when the SPA route handler \
         awaits any of them."
    );
}

/// Defence-in-depth: the only `block_on(` call permitted in
/// `issue_invoice.rs` is the CLI's top-level `run` boundary (where
/// the sync `pub fn run` bridges into the async pipeline). The
/// library helpers must stay `.await`-driven.
#[test]
fn issue_invoice_module_block_on_count_at_most_one() {
    let count = ISSUE_INVOICE_SRC.matches("block_on(").count();
    assert!(
        count <= 1,
        "issue_invoice.rs calls `block_on(` {count} times; PR-60 invariant \
         allows AT MOST 1 (the CLI's top-level `run` bridge). A second \
         occurrence is almost certainly a regression that pushed a \
         sync-over-async bridge back inside one of the library helpers, \
         which panics when the SPA route handler reaches the same code \
         path via `.await` on axum's runtime."
    );
}
