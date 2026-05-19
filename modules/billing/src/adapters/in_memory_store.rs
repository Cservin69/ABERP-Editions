//! In-memory [`BillingStore`] adapter for tests.
//!
//! Mirrors the DuckDB adapter's behaviour exactly. The single-writer
//! lock (`&mut self` on every mutating method) gives us the same atomic
//! "allocate-and-insert" semantics as DuckDB's `FOR UPDATE` row lock
//! without needing a real transaction.

use std::collections::HashMap;

use time::OffsetDateTime;

use crate::app::error::BillingError;
use crate::app::issue_invoice::IdempotencyKey;
use crate::domain::ids::{InvoiceId, ReservationId, SeriesId};
use crate::domain::invoice::ReadyInvoice;
use crate::domain::reservation::{ReservationStatus, SequenceReservation};
use crate::domain::series::{InvoiceSeries, SeriesCode};
use crate::ports::storage::{AllocateArgs, AllocateOutcome, BillingStore};

#[derive(Debug, Default)]
pub struct InMemoryBillingStore {
    series_by_id: HashMap<SeriesId, InvoiceSeries>,
    series_by_code: HashMap<String, SeriesId>,
    /// Next number to allocate, keyed by (series_id, fiscal_year).
    next_number: HashMap<(SeriesId, i32), u64>,
    reservations: Vec<SequenceReservation>,
    invoices: HashMap<InvoiceId, ReadyInvoice>,
    /// Idempotency cache: command id -> (invoice_id, reservation_id).
    /// On replay the allocator returns the stored entry verbatim.
    idempotency: HashMap<IdempotencyKey, (InvoiceId, ReservationId)>,
}

impl InMemoryBillingStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl BillingStore for InMemoryBillingStore {
    fn ensure_schema(&mut self) -> Result<(), BillingError> {
        // In-memory: schema is the Rust structs. Nothing to do.
        Ok(())
    }

    fn create_series(&mut self, series: &InvoiceSeries) -> Result<(), BillingError> {
        let code_key = series.code.as_str().to_string();
        if self.series_by_code.contains_key(&code_key) {
            return Err(BillingError::Invalid("series code already exists"));
        }
        self.series_by_code.insert(code_key, series.id);
        self.series_by_id.insert(series.id, series.clone());
        Ok(())
    }

    fn find_series_by_code(
        &self,
        code: &SeriesCode,
    ) -> Result<Option<InvoiceSeries>, BillingError> {
        Ok(self
            .series_by_code
            .get(code.as_str())
            .and_then(|id| self.series_by_id.get(id))
            .cloned())
    }

    fn find_series_by_id(&self, id: SeriesId) -> Result<Option<InvoiceSeries>, BillingError> {
        Ok(self.series_by_id.get(&id).cloned())
    }

    fn allocate_and_insert(
        &mut self,
        args: AllocateArgs,
        now: OffsetDateTime,
    ) -> Result<AllocateOutcome, BillingError> {
        // ── Idempotency: same command id => return prior outcome.
        if let Some((invoice_id, reservation_id)) = self.idempotency.get(&args.idempotency_key) {
            let invoice = self
                .invoices
                .get(invoice_id)
                .cloned()
                .ok_or(BillingError::Invalid(
                    "idempotency cache points to missing invoice",
                ))?;
            let reservation = self
                .reservations
                .iter()
                .find(|r| r.id == *reservation_id)
                .cloned()
                .ok_or(BillingError::Invalid(
                    "idempotency cache points to missing reservation",
                ))?;
            return Ok(AllocateOutcome::Replay {
                invoice,
                reservation,
            });
        }

        // ── Resolve the series. PR-4 supports `Never` only; the handler
        //    rejects `AnnualOnFiscalYear` before reaching the store, so
        //    fiscal_year is always 0 here.
        let series = self
            .series_by_id
            .get(&args.series_id)
            .cloned()
            .ok_or(BillingError::SeriesNotFound("<unknown id>".to_string()))?;
        let fiscal_year: i32 = match series.reset_policy {
            crate::domain::series::ResetPolicy::Never => 0,
            crate::domain::series::ResetPolicy::AnnualOnFiscalYear => {
                return Err(BillingError::AnnualResetUnimplemented);
            }
        };

        // ── Allocate the next number atomically.
        let next = self
            .next_number
            .entry((args.series_id, fiscal_year))
            .or_insert(1);
        let allocated = *next;
        *next = next.checked_add(1).ok_or(BillingError::Invalid(
            "sequence counter overflowed u64::MAX (impossible in practice)",
        ))?;

        // ── Build the ReadyInvoice from the Draft.
        let draft = args.draft;
        let invoice = ReadyInvoice {
            id: draft.id,
            series_id: draft.series_id,
            customer_id: draft.customer_id,
            lines: draft.lines,
            issue_date: draft.issue_date,
            sequence_number: allocated,
            fiscal_year,
        };

        // ── Build the reservation row.
        let reservation = SequenceReservation {
            id: ReservationId::new(),
            series_id: series.id,
            fiscal_year,
            number: allocated,
            invoice_id: invoice.id,
            status: ReservationStatus::Reserved,
            void_reason: None,
            reserved_at: now,
            used_at: None,
            voided_at: None,
        };

        // ── Commit both, plus the idempotency record.
        self.invoices.insert(invoice.id, invoice.clone());
        self.reservations.push(reservation.clone());
        self.idempotency
            .insert(args.idempotency_key, (invoice.id, reservation.id));

        Ok(AllocateOutcome::Fresh {
            invoice,
            reservation,
        })
    }

    fn void_reservation(
        &mut self,
        invoice_id: InvoiceId,
        void_reason: String,
        voided_at: OffsetDateTime,
    ) -> Result<(), BillingError> {
        let r = self
            .reservations
            .iter_mut()
            .find(|r| r.invoice_id == invoice_id)
            .ok_or(BillingError::Invalid(
                "no reservation found for that invoice_id",
            ))?;
        if r.status != ReservationStatus::Reserved {
            return Err(BillingError::Invalid(
                "only Reserved reservations may be Voided",
            ));
        }
        r.status = ReservationStatus::Voided;
        r.void_reason = Some(void_reason);
        r.voided_at = Some(voided_at);
        Ok(())
    }

    fn list_reservations(
        &self,
        series_id: SeriesId,
    ) -> Result<Vec<SequenceReservation>, BillingError> {
        let mut out: Vec<_> = self
            .reservations
            .iter()
            .filter(|r| r.series_id == series_id)
            .cloned()
            .collect();
        out.sort_by_key(|r| (r.fiscal_year, r.number));
        Ok(out)
    }
}
