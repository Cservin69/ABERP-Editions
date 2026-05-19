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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct IdempotencyKey(pub Ulid);

impl IdempotencyKey {
    pub fn new() -> Self {
        Self(Ulid::new())
    }
}

impl Default for IdempotencyKey {
    fn default() -> Self {
        Self::new()
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
    let outcome = store.allocate_and_insert(
        AllocateArgs {
            series_id: series.id,
            draft,
            idempotency_key: cmd.idempotency_key,
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
