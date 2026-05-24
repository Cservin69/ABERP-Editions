//! [`IssueInvoiceCommand`] and its handler.
//!
//! The command's purpose: take a `DraftInvoice`-shaped payload plus a
//! series code, run the ADR-0009 §3 atomic allocator, and return either a
//! freshly-burned `ReadyInvoice` or — on retry of the same idempotency
//! key — the original outcome unchanged.
//!
//! The handler is `fn handle(...)` (free function) rather than a method
//! on a struct so it can be called against any [`BillingStore`] adapter
//! without ceremony. The store + clock are passed in explicitly; no
//! global state.

use ulid::Ulid;

use crate::app::error::BillingError;
use crate::domain::ids::CustomerId;
use crate::domain::invoice::{DraftInvoice, LineItem, ReadyInvoice};
use crate::domain::reservation::SequenceReservation;
use crate::domain::series::{ResetPolicy, SeriesCode};
use crate::ports::clock::Clock;
use crate::ports::storage::{AllocateArgs, AllocateOutcome, BillingStore};

/// Idempotency key per ADR-0009 §5 Layer 1: the ULID of the
/// `IssueInvoiceCommand` itself. The caller generates this once and
/// retries the command with the same key on failure; the allocator
/// returns the prior outcome without burning a new number.
///
/// # Canonical string form (API contract)
///
/// The on-disk format — written to the `audit_ledger.idempotency_key`
/// column and used for Layer-1 lookups — is the prefixed ULID per
/// ADR-0005 §"Entity prefixes":
///
/// ```text
/// idem_<26-character-Crockford-base32-ULID>
/// ```
///
/// [`IdempotencyKey::to_canonical_string`] and
/// [`IdempotencyKey::from_canonical_string`] are the one round-trip
/// pair. The `Debug` derivation is **not** the on-disk format and is
/// not stable across crate versions — anyone reaching for
/// `format!("{:?}", key)` to persist or compare across processes is
/// breaking the contract. PR-6.1 surfaced this trap (Fortnightly
/// review F8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(pub Ulid);

impl IdempotencyKey {
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    /// Render in the ADR-0005 prefixed form: `idem_<ULID>`. This is the
    /// authoritative on-disk and on-the-wire string for an
    /// idempotency key. See the type-level doc-comment for the
    /// contract.
    pub fn to_canonical_string(&self) -> String {
        format!("idem_{}", self.0)
    }

    /// Inverse of [`IdempotencyKey::to_canonical_string`]. Returns
    /// `None` if `s` is missing the `idem_` prefix or if the bare ULID
    /// part is not a valid Crockford-base32 ULID. Loud-fail rather
    /// than producing a silently-wrong key.
    pub fn from_canonical_string(s: &str) -> Option<Self> {
        let bare = s.strip_prefix("idem_")?;
        Ulid::from_string(bare).ok().map(Self)
    }
}

impl Default for IdempotencyKey {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod idempotency_key_tests {
    use super::*;

    /// Round-trip: every key built by `new()` must survive
    /// `to_canonical_string` followed by `from_canonical_string`. If
    /// this test ever fails, the on-disk format has drifted and
    /// audit-ledger rows from prior versions become unreadable.
    #[test]
    fn canonical_string_round_trip() {
        for _ in 0..32 {
            let original = IdempotencyKey::new();
            let s = original.to_canonical_string();
            assert!(
                s.starts_with("idem_"),
                "canonical string must carry the `idem_` prefix per ADR-0005"
            );
            assert_eq!(s.len(), 5 + 26, "prefix + 26-char ULID");
            let parsed =
                IdempotencyKey::from_canonical_string(&s).expect("round-trip parse must succeed");
            assert_eq!(parsed, original);
        }
    }

    #[test]
    fn from_canonical_string_rejects_missing_prefix() {
        let key = IdempotencyKey::new();
        let bare_ulid = key.0.to_string();
        assert!(
            IdempotencyKey::from_canonical_string(&bare_ulid).is_none(),
            "bare ULID without prefix must be rejected (no silent parse)"
        );
    }

    #[test]
    fn from_canonical_string_rejects_malformed_ulid() {
        assert!(
            IdempotencyKey::from_canonical_string("idem_not-a-real-ulid").is_none(),
            "prefix-present-but-body-garbage must be rejected"
        );
        assert!(
            IdempotencyKey::from_canonical_string("idem_").is_none(),
            "prefix-only string must be rejected"
        );
    }
}

/// Caller-supplied data for issuing an invoice. The command does NOT
/// carry `issue_date`, `invoice_id`, `sequence_number`, or `fiscal_year`
/// — those are decided by the allocator using the injected clock per
/// ADR-0007 §"Operator-as-threat-actor".
#[derive(Debug, Clone)]
pub struct IssueInvoiceCommand {
    pub idempotency_key: IdempotencyKey,
    pub series_code: SeriesCode,
    pub customer_id: CustomerId,
    pub lines: Vec<LineItem>,
}

/// Result of a successful issuance. Mirrors [`AllocateOutcome`] at the
/// command level so callers can branch on "fresh vs replay" loudly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueInvoiceOutcome {
    Fresh {
        invoice: ReadyInvoice,
        reservation: SequenceReservation,
    },
    Replay {
        invoice: ReadyInvoice,
        reservation: SequenceReservation,
    },
}

impl IssueInvoiceOutcome {
    pub fn invoice(&self) -> &ReadyInvoice {
        match self {
            Self::Fresh { invoice, .. } | Self::Replay { invoice, .. } => invoice,
        }
    }

    pub fn reservation(&self) -> &SequenceReservation {
        match self {
            Self::Fresh { reservation, .. } | Self::Replay { reservation, .. } => reservation,
        }
    }
}

/// The command handler. Resolves the series, validates line items,
/// captures the issue date from the clock, and delegates to the store's
/// atomic allocator.
pub fn handle<S, C>(
    store: &mut S,
    clock: &C,
    cmd: IssueInvoiceCommand,
) -> Result<IssueInvoiceOutcome, BillingError>
where
    S: BillingStore + ?Sized,
    C: Clock + ?Sized,
{
    // 1. Validate the command shape. Empty line lists are a caller bug;
    //    fail loud rather than producing a zero-total invoice.
    if cmd.lines.is_empty() {
        return Err(BillingError::Invalid(
            "IssueInvoiceCommand.lines must contain at least one line item",
        ));
    }
    for (line_index, line) in cmd.lines.iter().enumerate() {
        // Each line must be arithmetic-sound; overflow is loud.
        if line.gross_total().is_none() {
            return Err(BillingError::MoneyOverflow { line_index });
        }
    }

    // 2. Resolve the series. Unknown series is operator-actionable, not
    //    a silent allocator failure.
    let series = store
        .find_series_by_code(&cmd.series_code)?
        .ok_or_else(|| BillingError::SeriesNotFound(cmd.series_code.as_str().to_string()))?;

    // PR-4 implements `Never` only. Loud-fail on Annual until the
    //    follow-up PR fills in year-roll.
    if matches!(series.reset_policy, ResetPolicy::AnnualOnFiscalYear) {
        return Err(BillingError::AnnualResetUnimplemented);
    }

    // 3. Build the draft. `issue_date` and `id` are decided here — never
    //    pulled from the command — per ADR-0007 §"Operator-as-threat-actor".
    let issue_date = clock.now_utc();
    let draft = DraftInvoice {
        id: crate::domain::ids::InvoiceId::new(),
        series_id: series.id,
        customer_id: cmd.customer_id,
        lines: cmd.lines,
        issue_date,
    };

    // 4. Delegate to the storage adapter for the atomic allocate.
    //
    //    PR-44γ — this handler is the in-process aberp-billing
    //    surface (used by unit tests + the in-memory store). It
    //    defaults `currency: Huf` / `rate_metadata: None` because
    //    the CLI binary is the surface that fetches the MNB rate
    //    and constructs the EUR `AllocateArgs` (per ADR-0037 §2
    //    — the rate fetch is an orchestration concern, not a
    //    domain-handler concern). When a future PR moves the
    //    EUR-aware command-build into the handler, this default
    //    becomes the closed-vocab default and the handler takes
    //    a typed `Currency` parameter.
    let outcome = store.allocate_and_insert(
        AllocateArgs {
            series_id: series.id,
            draft,
            idempotency_key: cmd.idempotency_key,
            currency: crate::domain::money::Currency::Huf,
            rate_metadata: None,
        },
        issue_date,
    )?;

    Ok(match outcome {
        AllocateOutcome::Fresh {
            invoice,
            reservation,
        } => IssueInvoiceOutcome::Fresh {
            invoice,
            reservation,
        },
        AllocateOutcome::Replay {
            invoice,
            reservation,
        } => IssueInvoiceOutcome::Replay {
            invoice,
            reservation,
        },
    })
}
