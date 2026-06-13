//! S392 — NAV invoice-number existence pre-flight.
//!
//! # The bug this closes
//!
//! ABERP claims the next invoice sequence number from the local DuckDB
//! counter alone. NAV's **shared TEST endpoint** remembers every invoice
//! number a previous DEV cycle submitted. So after a local DB reset the
//! local counter restarts at `start_value` while NAV still holds the old
//! numbers — a fresh local `seq=48` collides with NAV's prior
//! `TEST-ABERP/2026/0048` and the submit ABORTs with
//! `INVOICE_NUMBER_NOT_UNIQUE` (operator hit this on 2026-06-13).
//!
//! # The fix
//!
//! Before the allocator transaction opens, probe NAV's `queryInvoiceCheck`
//! for each candidate number and skip the ones NAV already holds, so the
//! reservation only commits on a NAV-clear number. The first clear
//! sequence is threaded back as [`aberp_billing::AllocateArgs::sequence_floor`].
//!
//! # Why pre-tx and not inside the allocator
//!
//! The allocator (`aberp_billing::allocate_in_tx`) is a synchronous
//! DuckDB transaction in a NAV-agnostic crate, and the issuance pipeline
//! is `async`. A `block_on` inside that tx would panic (nested runtime)
//! and would hold the single-writer DuckDB lock across NAV network
//! round-trips. Probing pre-tx in the async context avoids both: the tx
//! opens only once a clear floor is known, holding the write-lock for the
//! local writes alone.
//!
//! # Conservatism (network resilience)
//!
//! `queryInvoiceCheck` can 5xx / time out / reject. The pre-flight is a
//! convenience, never a gate on business: on ANY probe failure it stops
//! at the current candidate and ALLOWS the reservation (logged at WARN).
//! NAV may then reject at submit — the same failure mode that exists
//! today — but issuance is never blocked by a flaky pre-flight.
//!
//! # DEV-only
//!
//! Gated on the compile-time [`crate::build_profile::IS_PRODUCTION_BUILD`]
//! (the repo's single test-vs-prod lever — there is no env override by
//! design). Production builds pass `None`: prod numbers are strictly
//! monotonic and never collide, so the probe is pure overhead there.
//!
//! # Audit
//!
//! Each skipped number is recorded as a reused
//! `EventKind::InvoiceCheckPerformed` entry with `outcome = "exists"`
//! (every skip IS a positive existence check), carrying the rejected
//! number + NAV's verbatim response. No new EventKind is introduced, so
//! the NAV-leak firewall (which already handles `InvoiceCheckPerformed`'s
//! NAV bytes) needs no change.

use aberp_nav_transport::operations::query_invoice_check;
use aberp_nav_transport::soap::InvoiceDirection;
use aberp_nav_transport::{NavCredentials, NavTransport};
use anyhow::{anyhow, Result};
use async_trait::async_trait;

use crate::numbering::NumberingTemplate;

/// Hard cap on the skip loop (brief item 2). A run of this many
/// consecutive NAV-claimed numbers is pathological — almost certainly a
/// large prior-cycle range on NAV's TEST endpoint — and is surfaced as an
/// actionable error rather than silently skipping forever.
pub const MAX_NAV_NUMBER_SKIPS: u32 = 1000;

/// Verdict for a single candidate invoice number.
#[derive(Debug)]
pub enum NavNumberProbeOutcome {
    /// NAV has no record — this number is safe to reserve.
    Clear,
    /// NAV already holds this number — skip it. Carries the verbatim
    /// request/response bytes so the caller can record an
    /// `InvoiceCheckPerformed(outcome="exists")` audit entry.
    Taken {
        request_xml: Vec<u8>,
        response_xml: Vec<u8>,
    },
    /// Transient/uncertain NAV failure (5xx, timeout, transport, envelope,
    /// credential, or a NAV ERROR funcCode). Conservative: the caller
    /// ALLOWS the reservation at the current candidate and logs a warning.
    /// Carries the operator-visible message for that warning only.
    Unavailable { message: String },
}

/// A number the pre-flight skipped because NAV already held it. Carried
/// from the probe loop into the issuance transaction, where it becomes an
/// `InvoiceCheckPerformed(outcome="exists")` audit entry.
#[derive(Debug, Clone)]
pub struct SkippedNavNumber {
    /// The NAV-facing `<invoiceNumber>` string that was found to exist.
    pub nav_invoice_number: String,
    /// Verbatim `<QueryInvoiceCheckRequest>` bytes.
    pub request_xml: Vec<u8>,
    /// Verbatim `<QueryInvoiceCheckResponse>` bytes.
    pub response_xml: Vec<u8>,
}

/// Result of resolving the first NAV-clear sequence number.
#[derive(Debug)]
pub struct ClearSequence {
    /// The sequence number the allocator should reserve (the first one
    /// NAV did not already hold, or the start when probing was disabled /
    /// the very first candidate was already clear / NAV was unavailable).
    pub floor: u64,
    /// The numbers skipped to reach `floor`, in skip order. Empty when no
    /// skip occurred.
    pub skipped: Vec<SkippedNavNumber>,
}

/// Existence-checks a candidate invoice number against NAV. Injected into
/// the issuance pipeline so the loop is unit-testable with a mock and so
/// production swaps in [`LiveNavInvoiceNumberProbe`].
#[async_trait]
pub trait NavInvoiceNumberProbe: Send + Sync {
    /// Check `nav_invoice_number` against NAV's `queryInvoiceCheck`.
    /// Infallible by contract: transport/parse/NAV-error failures fold
    /// into [`NavNumberProbeOutcome::Unavailable`] (the conservative
    /// "allow" path), never a hard error that would block issuance.
    async fn check(&self, nav_invoice_number: &str) -> NavNumberProbeOutcome;
}

/// Walk candidate sequence numbers from `start_seq` upward, skipping the
/// ones NAV already holds, and return the first NAV-clear number plus the
/// skip records. `probe == None` (production / pre-flight disabled)
/// short-circuits to `floor = start_seq` with no NAV calls.
///
/// Stops and ALLOWS the current candidate on the first `Clear` OR the
/// first `Unavailable` (conservative). Errors only when more than
/// `max_skips` consecutive numbers are claimed (brief item 2 cap).
pub async fn resolve_clear_sequence(
    probe: Option<&dyn NavInvoiceNumberProbe>,
    template: &NumberingTemplate,
    issue_year: i32,
    start_seq: u64,
    max_skips: u32,
) -> Result<ClearSequence> {
    let probe = match probe {
        Some(p) => p,
        None => {
            return Ok(ClearSequence {
                floor: start_seq,
                skipped: Vec::new(),
            })
        }
    };

    let mut skipped: Vec<SkippedNavNumber> = Vec::new();
    let mut candidate = start_seq;
    loop {
        // Render exactly what the allocator's `render_and_write` closure
        // would emit for this sequence (template + dev `TEST-` prefix), so
        // the number we existence-check is byte-identical to the number
        // that would hit NAV at submit.
        let nav_invoice_number = template.render_for_build(issue_year, candidate);
        match probe.check(&nav_invoice_number).await {
            NavNumberProbeOutcome::Clear => {
                return Ok(ClearSequence {
                    floor: candidate,
                    skipped,
                });
            }
            NavNumberProbeOutcome::Unavailable { message } => {
                tracing::warn!(
                    nav_invoice_number = %nav_invoice_number,
                    reason = %message,
                    "S392 NAV queryInvoiceCheck unavailable; allowing reservation \
                     (conservative — NAV may reject at submit)"
                );
                return Ok(ClearSequence {
                    floor: candidate,
                    skipped,
                });
            }
            NavNumberProbeOutcome::Taken {
                request_xml,
                response_xml,
            } => {
                tracing::info!(
                    nav_invoice_number = %nav_invoice_number,
                    "S392 NAV already holds this invoice number (prior DEV cycle); skipping"
                );
                skipped.push(SkippedNavNumber {
                    nav_invoice_number,
                    request_xml,
                    response_xml,
                });
                if skipped.len() as u32 >= max_skips {
                    return Err(anyhow!(
                        "NAV queryInvoiceCheck reported {max_skips} consecutive \
                         already-submitted invoice numbers starting at sequence {start_seq} \
                         — refusing to skip further. NAV's shared TEST endpoint likely holds a \
                         large prior-cycle range; bump [seller.numbering].start_value past the \
                         burned range in seller.toml, or investigate NAV TEST state."
                    ));
                }
                candidate += 1;
            }
        }
    }
}

/// Production probe: wraps a [`NavTransport`] + [`NavCredentials`] +
/// supplier tax number and drives the existing
/// `query_invoice_check::{build_request, send_built_request}` wrapper
/// (PR-20 / ADR-0033). The transport (and its pooled connection) is built
/// once and reused across the loop.
pub struct LiveNavInvoiceNumberProbe {
    transport: NavTransport,
    credentials: NavCredentials,
    tax_number_8: String,
}

impl LiveNavInvoiceNumberProbe {
    /// Build a live probe targeting `endpoint`. `supplier_tax_number` is
    /// any accepted Hungarian form (`12345678`, `12345678-1`,
    /// `12345678-1-42`); its 8-digit base is what NAV's query block needs.
    /// Loud-fails on a malformed tax number or transport-build error — the
    /// caller treats that as "pre-flight unavailable" and proceeds without
    /// it (never blocking issuance).
    pub fn new(
        endpoint: aberp_nav_transport::NavEndpoint,
        credentials: NavCredentials,
        supplier_tax_number: &str,
    ) -> Result<Self> {
        let tax_number_8 = tax_number_8(supplier_tax_number)?;
        let transport = NavTransport::new(endpoint)
            .map_err(|e| anyhow!("build NAV transport for queryInvoiceCheck pre-flight: {e}"))?;
        Ok(Self {
            transport,
            credentials,
            tax_number_8,
        })
    }
}

#[async_trait]
impl NavInvoiceNumberProbe for LiveNavInvoiceNumberProbe {
    async fn check(&self, nav_invoice_number: &str) -> NavNumberProbeOutcome {
        let request_xml = match query_invoice_check::build_request(
            &self.credentials,
            &self.tax_number_8,
            nav_invoice_number,
            InvoiceDirection::Outbound,
        ) {
            Ok(bytes) => bytes,
            Err(e) => {
                return NavNumberProbeOutcome::Unavailable {
                    message: format!("queryInvoiceCheck envelope construction failed: {e}"),
                }
            }
        };
        match query_invoice_check::send_built_request(&self.transport, &request_xml).await {
            Ok(outcome) if outcome.check_result => NavNumberProbeOutcome::Taken {
                request_xml,
                response_xml: outcome.response_xml,
            },
            Ok(_) => NavNumberProbeOutcome::Clear,
            // Any NAV-side failure (HTTP status, transport, parse,
            // retryable/non-retryable funcCode) is treated conservatively
            // as "unavailable → allow". The pre-flight must never harden a
            // transient NAV hiccup into an issuance block.
            Err(e) => NavNumberProbeOutcome::Unavailable {
                message: format!("{e}"),
            },
        }
    }
}

/// Build the issuance NAV pre-flight probe, or `None` when disabled.
/// Disabled on production builds (monotonic numbers never collide) and on
/// any probe-build failure (conservative — issuance proceeds without it).
/// Consumes `credentials` (the probe owns them for the loop's lifetime).
pub fn build_issue_probe(
    credentials: NavCredentials,
    supplier_tax_number: &str,
) -> Option<Box<dyn NavInvoiceNumberProbe>> {
    if crate::build_profile::IS_PRODUCTION_BUILD {
        return None;
    }
    match LiveNavInvoiceNumberProbe::new(
        crate::build_profile::nav_endpoint(),
        credentials,
        supplier_tax_number,
    ) {
        Ok(probe) => Some(Box::new(probe)),
        Err(e) => {
            tracing::warn!(
                err = %e,
                "S392 NAV number pre-flight disabled (probe build failed); \
                 issuance proceeds without it"
            );
            None
        }
    }
}

/// 8-digit base of a Hungarian tax number. Mirror of
/// `retry_submission::parse_tax_number_8` / `submit_invoice`'s — same
/// loud-fail shape, kept local so this module has no cross-module dep.
fn tax_number_8(raw: &str) -> Result<String> {
    let base = raw.split('-').next().unwrap_or(raw);
    if base.len() != 8 || !base.chars().all(|c| c.is_ascii_digit()) {
        return Err(anyhow!(
            "supplier tax number '{raw}' base is not 8 ASCII digits \
             (expected forms: 12345678, 12345678-1, 12345678-1-42)"
        ));
    }
    Ok(base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// Canned per-call response for the scripted mock.
    #[derive(Clone)]
    enum Resp {
        Taken,
        Clear,
        Unavailable,
    }

    /// Deterministic mock — pops `scripted` responses in call order and
    /// falls back to `default` when exhausted. Records every number it was
    /// asked to check so tests can assert the exact probe sequence (and
    /// that a disabled probe is never called).
    struct ScriptedProbe {
        scripted: Mutex<VecDeque<Resp>>,
        default: Resp,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedProbe {
        fn new(scripted: Vec<Resp>, default: Resp) -> Self {
            Self {
                scripted: Mutex::new(scripted.into()),
                default,
                calls: Mutex::new(Vec::new()),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl NavInvoiceNumberProbe for ScriptedProbe {
        async fn check(&self, nav_invoice_number: &str) -> NavNumberProbeOutcome {
            self.calls
                .lock()
                .unwrap()
                .push(nav_invoice_number.to_string());
            let resp = self
                .scripted
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| self.default.clone());
            match resp {
                Resp::Taken => NavNumberProbeOutcome::Taken {
                    request_xml: b"<QueryInvoiceCheckRequest/>".to_vec(),
                    response_xml: b"<invoiceCheckResult>true</invoiceCheckResult>".to_vec(),
                },
                Resp::Clear => NavNumberProbeOutcome::Clear,
                Resp::Unavailable => NavNumberProbeOutcome::Unavailable {
                    message: "HTTP 503".to_string(),
                },
            }
        }
    }

    fn tmpl() -> NumberingTemplate {
        crate::numbering::default_template()
    }

    /// Brief item: NAV says EXISTS for seq=48 → allocator skips to 49 and
    /// records one skip carrying the rejected number.
    #[tokio::test]
    async fn exists_then_absent_skips_to_next() {
        let probe = ScriptedProbe::new(vec![Resp::Taken], Resp::Clear);
        let template = tmpl();
        let out = resolve_clear_sequence(Some(&probe), &template, 2026, 48, MAX_NAV_NUMBER_SKIPS)
            .await
            .expect("clear sequence resolves");
        assert_eq!(out.floor, 49, "must skip the NAV-claimed 48 to 49");
        assert_eq!(out.skipped.len(), 1);
        assert_eq!(
            out.skipped[0].nav_invoice_number,
            template.render_for_build(2026, 48)
        );
        // Probed exactly 48 (taken) then 49 (clear).
        assert_eq!(
            probe.calls(),
            vec![
                template.render_for_build(2026, 48),
                template.render_for_build(2026, 49),
            ]
        );
    }

    /// Brief item: NAV says DOES_NOT_EXIST → allocator commits the
    /// original number, no skip.
    #[tokio::test]
    async fn absent_keeps_original_number() {
        let probe = ScriptedProbe::new(vec![], Resp::Clear);
        let template = tmpl();
        let out = resolve_clear_sequence(Some(&probe), &template, 2026, 48, MAX_NAV_NUMBER_SKIPS)
            .await
            .expect("clear sequence resolves");
        assert_eq!(out.floor, 48);
        assert!(out.skipped.is_empty());
        assert_eq!(probe.calls(), vec![template.render_for_build(2026, 48)]);
    }

    /// Brief item: NAV 5xx → log warning, allocate the original number
    /// (conservative). No skip.
    #[tokio::test]
    async fn unavailable_allows_original_number() {
        let probe = ScriptedProbe::new(vec![Resp::Unavailable], Resp::Clear);
        let template = tmpl();
        let out = resolve_clear_sequence(Some(&probe), &template, 2026, 48, MAX_NAV_NUMBER_SKIPS)
            .await
            .expect("clear sequence resolves");
        assert_eq!(out.floor, 48, "transient failure must not advance");
        assert!(out.skipped.is_empty());
    }

    /// Brief item: PROD-config (probe disabled) → queryInvoiceCheck is
    /// never called and the original number is kept.
    #[tokio::test]
    async fn disabled_probe_never_checks() {
        let out = resolve_clear_sequence(None, &tmpl(), 2026, 48, MAX_NAV_NUMBER_SKIPS)
            .await
            .expect("disabled resolves trivially");
        assert_eq!(out.floor, 48);
        assert!(out.skipped.is_empty());
    }

    /// Brief item 2: the skip loop is capped — an unbounded run of
    /// NAV-claimed numbers surfaces an actionable error instead of
    /// skipping forever.
    #[tokio::test]
    async fn cap_exceeded_is_actionable_error() {
        let probe = ScriptedProbe::new(vec![], Resp::Taken); // always taken
        let err = resolve_clear_sequence(Some(&probe), &tmpl(), 2026, 1, 5)
            .await
            .expect_err("must error past the cap");
        let msg = format!("{err}");
        assert!(
            msg.contains("refusing to skip"),
            "actionable message: {msg}"
        );
        // Probed exactly `max_skips` numbers before bailing.
        assert_eq!(probe.calls().len(), 5);
    }

    /// Malformed supplier tax number is rejected at probe construction so
    /// the caller falls back to "no pre-flight" rather than building a
    /// probe that would send a malformed query for every issuance.
    #[test]
    fn tax_number_8_rejects_malformed() {
        assert_eq!(tax_number_8("12345678-1-42").unwrap(), "12345678");
        assert_eq!(tax_number_8("12345678").unwrap(), "12345678");
        assert!(tax_number_8("1234567").is_err());
        assert!(tax_number_8("ABCDEFGH").is_err());
    }
}
