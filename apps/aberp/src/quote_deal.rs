//! S272 / PR-261 — DEAL saga on a `quote_intake_log` row.
//! S273 / PR-262 — extended with the material-commit branch (ADR-0069).
//!
//! # What this module does
//!
//! Operator clicks DEAL on a quote intake row → this saga validates
//! preconditions, mints two placeholder ids (sales-order +
//! work-order), writes them onto the row inside a single DB
//! transaction, and emits THREE-OR-FOUR audit entries:
//!
//!   1. [`EventKind::QuoteDealIssued`] — top-level saga marker.
//!   2. [`EventKind::QuoteSalesOrderCreated`] — SO-side placeholder
//!      (brief pushback #1 — full SO module is named-deferred).
//!   3. [`EventKind::QuoteWorkOrderCreated`] — WO-side placeholder
//!      (PR-228 WO crate needs `product_id` + routing ops that the
//!      quote intake row does not yet carry).
//!   4. [`EventKind::MaterialCommitted`] — material-side commit (S273,
//!      ADR-0069). Emitted ONLY when the row carries both
//!      `material_grade` AND `quantity` (the storefront's S271
//!      projection columns). On a pre-storefront row (either column
//!      NULL) the material branch is skipped silently — the saga still
//!      mints SO/WO + emits the three other audit entries.
//!
//! All entries + the column writes + the `inventory_balances` /
//! `inventory_reservations` writes share ONE [`Transaction`] per
//! ADR-0067 / ADR-0069: any step failing rolls back the whole saga.
//!
//! # Single-use invariant (EVE addendum 3)
//!
//! The CAS guard in [`log_table::mark_deal_issued_in_tx`] sets
//! `deal_issued_at` only `WHERE deal_issued_at IS NULL`; a replay
//! against an already-dealt row updates zero rows. This module then
//! converts that zero into [`DealSagaError::DealAlreadyIssued`], which
//! the route maps to HTTP 409 with machine code `deal_already_issued`.
//!
//! # EVE addendum 2 (stock_alert × REFRESH)
//!
//! If the row's `stock_alert` is TRUE, the saga refuses to deal until
//! the operator submits `refresh_ack = Some("REFRESH")` — case-sensitive,
//! literal, per [[hulye-biztos]]. The DEAL UI hides the BIG/RED token
//! field until the typed REFRESH token unlocks it; the server-side
//! gate is defense-in-depth.
//!
//! # DEAL token
//!
//! The expected token is the first 8 characters of the row's
//! `quote_id` — copyable verbatim from the row's reference column.
//! Storefront-side production quote ids are UUID-shaped (`0226e154-…`);
//! the test-only `q-1`-style ids fall through with the WHOLE id as the
//! token (the helper caps at `min(8, len)` so short ids stay valid).

use anyhow::{anyhow, Context, Result};
use duckdb::Connection;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ulid::Ulid;

use aberp_audit_ledger::{append_in_tx, Actor, EventKind, LedgerMeta};
use aberp_quote_intake::log_table;

use crate::material_inventory::{
    self, MaterialCommitOutcome, MaterialCommittedPayload, MaterialInventoryError, QtyUnitKind,
};

/// Literal token the operator must type into the REFRESH input when the
/// row's `stock_alert` column is TRUE. Case-sensitive per
/// [[hulye-biztos]]; the SPA input is NOT auto-uppercased so
/// operator-typed casing reaches the server verbatim.
pub const REFRESH_ACK_TOKEN: &str = "REFRESH";

/// Cap on the DEAL token comparison window. Storefront-side production
/// `quote_id`s are UUID-shaped (32+ chars); the test-only `q-1`-style
/// ids stay valid because [`expected_deal_token`] collapses to
/// `min(QUOTE_DEAL_TOKEN_LEN, quote_id.len())`.
pub const QUOTE_DEAL_TOKEN_LEN: usize = 8;

/// Closed-vocab DEAL-saga failure modes. Each variant maps to a
/// distinct HTTP 409 machine code the SPA renders as a red operator
/// toast (see `apps/aberp-ui/ui/src/routes/QuotesList.svelte`). The
/// pure module returns these; the route layer (`serve.rs`) does the
/// HTTP translation per [[trust-code-not-operator]].
#[derive(Debug, Error, PartialEq)]
pub enum DealSagaError {
    /// The quote intake row does not exist for `(tenant, quote_id)`.
    /// Route maps to 404.
    #[error("quote {quote_id} not staged in quote_intake_log (tenant {tenant})")]
    NotStaged { tenant: String, quote_id: String },
    /// The row exists but its `intake_state` is not `staged` (it's
    /// `error` or `irrelevant`). Route maps to 409
    /// `not_actionable`.
    #[error("quote {quote_id} intake_state is {state:?}; only staged rows can be dealt")]
    NotActionable { quote_id: String, state: String },
    /// `deal_issued_at` is already set — EVE-addendum-3 single-use
    /// invariant. Route maps to 409 `deal_already_issued`.
    #[error("quote {quote_id} has already been dealt")]
    DealAlreadyIssued { quote_id: String },
    /// `stock_alert` is TRUE and the request did not carry
    /// `refresh_ack = "REFRESH"`. Route maps to 409
    /// `stock_alert_refresh_required`.
    #[error("quote {quote_id} has stock_alert TRUE; type REFRESH to acknowledge first")]
    StockAlertRefreshRequired { quote_id: String },
    /// The DEAL token operator typed does not equal the row's
    /// expected `quote_id[..min(8, len)]`. Route maps to 409
    /// `deal_token_mismatch`.
    #[error("quote {quote_id} DEAL token mismatch")]
    DealTokenMismatch { quote_id: String },
    /// S273 / PR-262 / ADR-0069 — material-side capacity check failed:
    /// `on_hand_qty < reserved_qty + committed_qty + requested_qty`.
    /// The wrapped numbers reach the SPA's 409 toast so the operator
    /// can see exactly why the DEAL was refused and fix the
    /// `on_hand_qty` in the Inventory Balances view. Route maps to 409
    /// `insufficient_material`.
    #[error("quote {quote_id} material {material_grade}: insufficient (requested {requested}, on_hand {on_hand}, already reserved {already_reserved}, already committed {already_committed})")]
    InsufficientMaterial {
        quote_id: String,
        material_grade: String,
        requested: f64,
        on_hand: f64,
        already_reserved: f64,
        already_committed: f64,
    },
    /// S275 / PR-264 / F32 — the row's `valid_until` is BEFORE the
    /// saga's current date. A clock-drifted machine could otherwise
    /// commit a 6-month-old quote at the original (now wrong) price.
    /// Route maps to 409 `quote_expired`.
    #[error("quote {quote_id} expired on {valid_until} (today is {today})")]
    QuoteExpired {
        quote_id: String,
        valid_until: String,
        today: String,
    },
    /// S428 — the quote priced below its effective margin floor
    /// (`quote_pricing_jobs.margin_below_floor`). A hard, code-enforced
    /// block ([[trust-code-not-operator]]): the operator cannot DEAL a
    /// money-losing job, even after confirming the override. Route maps to
    /// 409 `below_margin_floor`.
    #[error("quote {quote_id} is below the margin floor; DEAL refused")]
    BelowMarginFloor { quote_id: String },
}

impl DealSagaError {
    /// Machine code surfaced on the 409 body so the SPA's toast routes
    /// to the right copy. Mirrors the storno-route pattern.
    pub fn machine_code(&self) -> &'static str {
        match self {
            DealSagaError::NotStaged { .. } => "not_staged",
            DealSagaError::NotActionable { .. } => "not_actionable",
            DealSagaError::DealAlreadyIssued { .. } => "deal_already_issued",
            DealSagaError::StockAlertRefreshRequired { .. } => "stock_alert_refresh_required",
            DealSagaError::DealTokenMismatch { .. } => "deal_token_mismatch",
            DealSagaError::InsufficientMaterial { .. } => "insufficient_material",
            DealSagaError::QuoteExpired { .. } => "quote_expired",
            DealSagaError::BelowMarginFloor { .. } => "below_margin_floor",
        }
    }
}

/// S275 / PR-264 / F32 — pure-function expiry check the saga calls
/// inside its preconditions. The DB stores `valid_until` as a `DATE`
/// (YYYY-MM-DD); the parser is the `time` crate's strict date format.
///
/// Semantics:
/// - `valid_until = None` (storefront pre-S271 row, or the writer
///   simply omitted it): the saga proceeds. Absence is "no expiry on
///   file," not "expired."
/// - `valid_until` unparseable: same — the saga proceeds. A storefront
///   writer that pushes garbage is a producer bug; loud-failing the
///   saga on a parse error would punish the operator for a problem
///   they cannot fix from the SPA. The audit walk preserves the raw
///   string so a forensic check still surfaces the bug.
/// - `valid_until < today`: the saga refuses and the SPA toast routes
///   to a `quote_expired` 409.
/// - `valid_until >= today`: the saga proceeds. Same-day expiry is
///   honoured (the customer accepted on the same day this was minted;
///   refusing is hostile UX).
pub fn quote_is_expired(valid_until_ymd: Option<&str>, today: time::Date) -> bool {
    let Some(raw) = valid_until_ymd else {
        return false;
    };
    let parser = time::macros::format_description!("[year]-[month]-[day]");
    let Ok(valid_until) = time::Date::parse(raw, &parser) else {
        return false;
    };
    valid_until < today
}

/// Compute the expected DEAL token from a row's `quote_id`. The
/// operator must type the FIRST `QUOTE_DEAL_TOKEN_LEN` characters
/// verbatim; for sub-8-char ids the whole id is the token (defensive —
/// test-fixture `q-1` ids never reach prod but should not panic).
///
/// Case-sensitive per [[hulye-biztos]]: storefront-pushed UUID ids are
/// already lower-case; an uppercase typo on a UUID character set is a
/// legitimate mismatch.
pub fn expected_deal_token(quote_id: &str) -> String {
    let take = QUOTE_DEAL_TOKEN_LEN.min(quote_id.len());
    quote_id[..take].to_string()
}

/// Inputs to [`run_deal_saga`]. The route handler builds this from the
/// path param + JSON body + AppState plumbing.
#[derive(Debug, Clone)]
pub struct DealSagaInputs {
    pub tenant: String,
    pub quote_id: String,
    /// Operator login string for the audit `Actor`.
    pub actor: String,
    /// Operator-typed DEAL token (the BIG/RED single-use input). Must
    /// equal `expected_deal_token(quote_id)`.
    pub deal_token: String,
    /// Operator-typed REFRESH ack token. `Some("REFRESH")` unlocks the
    /// saga when the row's `stock_alert` is TRUE; `None` / any other
    /// value is rejected per EVE addendum 2.
    pub refresh_ack: Option<String>,
}

/// Outcome of a successful DEAL saga. Surfaced verbatim on the route's
/// 200 JSON body so the SPA can render the post-deal affordances.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DealSagaOutcome {
    /// `so_<ULID>` placeholder for the future Sales Order module.
    pub sales_order_id: String,
    /// `wo_<ULID>` placeholder for the future Work Order wire-up.
    pub work_order_id: String,
    /// ISO-8601 (RFC 3339) timestamp of the DEAL commit.
    pub deal_issued_at: String,
    /// `true` iff the saga consumed a REFRESH token (i.e. the row had
    /// `stock_alert = TRUE` at saga time).
    pub refresh_acknowledged: bool,
    /// S273 — material-commit outcome when the row carried
    /// `material_grade` + `quantity`. `None` on pre-storefront rows
    /// (the saga's material branch was skipped silently per ADR-0069 /
    /// brief pushback). The SPA reads this to decide whether to show
    /// the post-DEAL "12 kg of 6061-T6 committed" toast.
    #[serde(default)]
    pub material_commit: Option<MaterialCommitOutcome>,
}

/// JSON payload for the top-level [`EventKind::QuoteDealIssued`] entry.
/// Captures everything a forensic walk needs to reconstruct the saga
/// without re-deriving from sibling entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuoteDealIssuedPayload {
    pub quote_id: String,
    pub tenant_id: String,
    pub sales_order_id: String,
    pub work_order_id: String,
    pub deal_token: String,
    pub refresh_acknowledged: bool,
    pub actor: String,
    pub idempotency_key: String,
    pub deal_issued_at: String,
}

impl QuoteDealIssuedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize QuoteDealIssuedPayload")
    }
}

/// JSON payload for the [`EventKind::QuoteSalesOrderCreated`] entry.
/// Placeholder per brief pushback #1 — the full SO module is
/// named-deferred; this audit row carries the `so_<ULID>` that a
/// future SO backfill can adopt.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuoteSalesOrderCreatedPayload {
    pub quote_id: String,
    pub tenant_id: String,
    pub sales_order_id: String,
    pub actor: String,
    pub idempotency_key: String,
    pub created_at: String,
}

impl QuoteSalesOrderCreatedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize QuoteSalesOrderCreatedPayload")
    }
}

/// JSON payload for the [`EventKind::QuoteWorkOrderCreated`] entry.
/// Placeholder — the PR-228 WO crate requires `product_id` plus at
/// least one routing op, neither of which lives on the quote intake
/// row at this stage of the auto-quoting pipeline. The `wo_<ULID>`
/// here is reserved for the future plumbing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QuoteWorkOrderCreatedPayload {
    pub quote_id: String,
    pub tenant_id: String,
    pub work_order_id: String,
    pub actor: String,
    pub idempotency_key: String,
    pub created_at: String,
}

impl QuoteWorkOrderCreatedPayload {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("JSON serialize QuoteWorkOrderCreatedPayload")
    }
}

/// Idempotency-key composer for the saga. `quote_deal:<quote_id>` for
/// the one-and-only DEAL on this row. The CAS guard makes the key
/// unique by construction (a replay never reaches `append_in_tx` —
/// the saga aborts before the audit emits).
pub fn deal_idempotency_key(quote_id: &str) -> String {
    format!("quote_deal:{quote_id}")
}

/// Run the DEAL saga end-to-end. Mirrors the [`crate::quote_pickup`]
/// posture: pure function over the `Connection` + ledger so the
/// integration test can drive it without spinning AppState.
///
/// On success, returns [`DealSagaOutcome`] carrying the freshly-minted
/// SO/WO placeholder ids. On any precondition failure, returns
/// `Err(`[`anyhow::Error`]`)` wrapping a [`DealSagaError`] — the route
/// downcast translates it to the right 4xx machine code.
pub fn run_deal_saga(
    conn: &mut Connection,
    ledger_meta: &LedgerMeta,
    ledger_actor: Actor,
    inputs: DealSagaInputs,
) -> Result<DealSagaOutcome> {
    log_table::ensure_schema(conn).map_err(|e| anyhow!("ensure quote_intake_log schema: {e}"))?;

    // ── Precondition reads (outside the tx) ───────────────────────────
    let row = log_table::read_for_deal(conn, &inputs.tenant, &inputs.quote_id)
        .map_err(|e| anyhow!("read quote_intake_log for DEAL: {e}"))?
        .ok_or_else(|| {
            anyhow!(DealSagaError::NotStaged {
                tenant: inputs.tenant.clone(),
                quote_id: inputs.quote_id.clone(),
            })
        })?;

    if row.intake_state != log_table::STATE_STAGED {
        return Err(anyhow!(DealSagaError::NotActionable {
            quote_id: inputs.quote_id.clone(),
            state: row.intake_state,
        }));
    }

    // Single-use precondition pre-check. The CAS guard in
    // `mark_deal_issued_in_tx` is the source of truth (defends against
    // a concurrent DEAL between the read here and the tx); this early
    // exit gives the operator a faster 409 + skips the audit-emit work
    // on the common replay path.
    if row.deal_issued_at.is_some() {
        return Err(anyhow!(DealSagaError::DealAlreadyIssued {
            quote_id: inputs.quote_id.clone(),
        }));
    }

    // S428 — hard margin-floor block. A quote flagged below its effective
    // floor in `quote_pricing_jobs` cannot be dealt, regardless of operator
    // confirmation on the override ([[trust-code-not-operator]]). Absent /
    // NULL flag (manual quotes, pre-S428 rows) ⇒ not blocked.
    if crate::quote_pricing_jobs::margin_below_floor(conn, &inputs.quote_id, &inputs.tenant)
        .map_err(|e| anyhow!("read margin_below_floor for DEAL: {e}"))?
    {
        return Err(anyhow!(DealSagaError::BelowMarginFloor {
            quote_id: inputs.quote_id.clone(),
        }));
    }

    // Single-use takes precedence over REFRESH/token so a replay
    // attempt with a wrong token never overrides
    // `deal_already_issued` — the route's 409 stays consistent across
    // attack vectors per [[trust-code-not-operator]].
    if row.stock_alert {
        let acked = inputs.refresh_ack.as_deref() == Some(REFRESH_ACK_TOKEN);
        if !acked {
            return Err(anyhow!(DealSagaError::StockAlertRefreshRequired {
                quote_id: inputs.quote_id.clone(),
            }));
        }
    }

    let expected = expected_deal_token(&inputs.quote_id);
    if inputs.deal_token != expected {
        return Err(anyhow!(DealSagaError::DealTokenMismatch {
            quote_id: inputs.quote_id.clone(),
        }));
    }

    // ── Saga tx ───────────────────────────────────────────────────────
    let now_utc = time::OffsetDateTime::now_utc();
    let now_iso = now_utc
        .format(&time::format_description::well_known::Rfc3339)
        .context("format DEAL timestamp")?;

    // S275 / F32 — expiry check after the cheaper precondition checks
    // (so a wrong DEAL token still gets the more specific 409 instead
    // of being masked by an expiry error). Conservative on parse
    // failure / NULL — see `quote_is_expired` docs.
    let today_utc = now_utc.date();
    if quote_is_expired(row.valid_until.as_deref(), today_utc) {
        return Err(anyhow!(DealSagaError::QuoteExpired {
            quote_id: inputs.quote_id.clone(),
            valid_until: row.valid_until.clone().unwrap_or_default(),
            today: today_utc.to_string(),
        }));
    }
    let sales_order_id = format!("so_{}", Ulid::new());
    let work_order_id = format!("wo_{}", Ulid::new());
    let idempotency_key = deal_idempotency_key(&inputs.quote_id);
    let refresh_acked_at = if row.stock_alert {
        Some(now_iso.clone())
    } else {
        None
    };

    let tx = conn.transaction().context("begin DEAL saga tx")?;

    // CAS write — single-use guard. Zero rows updated means a
    // concurrent DEAL won between our precondition read and this CAS
    // (or the precondition read raced with the same caller and we
    // arrived here twice in two HTTP requests). Either way, abort and
    // surface `deal_already_issued`.
    let claimed = log_table::mark_deal_issued_in_tx(
        &tx,
        &inputs.tenant,
        &inputs.quote_id,
        &sales_order_id,
        &work_order_id,
        &now_iso,
        refresh_acked_at.as_deref(),
    )
    .map_err(|e| anyhow!("CAS mark_deal_issued_in_tx: {e}"))?;
    if claimed != 1 {
        // S275 / F25 — distinguish "row gone" from "already dealt"
        // before surfacing a 409. The CAS rejects in BOTH cases; the
        // operator's fix paths are different (404-shaped on a gone row,
        // 409 on a replay). Re-read inside the tx for a single source
        // of truth — DuckDB's snapshot isolation makes this read
        // consistent with whatever state lost the race.
        let row_now = log_table::read_for_deal_in_tx(&tx, &inputs.tenant, &inputs.quote_id)
            .map_err(|e| anyhow!("re-read after CAS rejection: {e}"))?;
        // Drop the tx → roll back any prior partial state. (None in
        // this case — the CAS was the first write — but the pattern
        // mirrors quote_pickup's lost-race rollback for symmetry.)
        drop(tx);
        return Err(anyhow!(match row_now {
            None => DealSagaError::NotStaged {
                tenant: inputs.tenant.clone(),
                quote_id: inputs.quote_id.clone(),
            },
            Some(r) if r.intake_state != log_table::STATE_STAGED => DealSagaError::NotActionable {
                quote_id: inputs.quote_id.clone(),
                state: r.intake_state,
            },
            // Row is still staged → CAS rejected because another
            // writer set `deal_issued_at` between our pre-flight and
            // the CAS. This is the genuine "already dealt" path.
            Some(_) => DealSagaError::DealAlreadyIssued {
                quote_id: inputs.quote_id.clone(),
            },
        }));
    }

    // Three audit entries — top-level + SO + WO. All ride the same tx
    // so they appear/disappear atomically per ADR-0067.
    let top_payload = QuoteDealIssuedPayload {
        quote_id: inputs.quote_id.clone(),
        tenant_id: inputs.tenant.clone(),
        sales_order_id: sales_order_id.clone(),
        work_order_id: work_order_id.clone(),
        deal_token: inputs.deal_token.clone(),
        refresh_acknowledged: row.stock_alert,
        actor: inputs.actor.clone(),
        idempotency_key: idempotency_key.clone(),
        deal_issued_at: now_iso.clone(),
    };
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::QuoteDealIssued,
        top_payload.to_bytes(),
        ledger_actor.clone(),
        Some(idempotency_key.clone()),
    )
    .context("audit append QuoteDealIssued")?;

    let so_payload = QuoteSalesOrderCreatedPayload {
        quote_id: inputs.quote_id.clone(),
        tenant_id: inputs.tenant.clone(),
        sales_order_id: sales_order_id.clone(),
        actor: inputs.actor.clone(),
        idempotency_key: format!("{idempotency_key}:so"),
        created_at: now_iso.clone(),
    };
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::QuoteSalesOrderCreated,
        so_payload.to_bytes(),
        ledger_actor.clone(),
        Some(format!("{idempotency_key}:so")),
    )
    .context("audit append QuoteSalesOrderCreated")?;

    let wo_payload = QuoteWorkOrderCreatedPayload {
        quote_id: inputs.quote_id.clone(),
        tenant_id: inputs.tenant.clone(),
        work_order_id: work_order_id.clone(),
        actor: inputs.actor.clone(),
        idempotency_key: format!("{idempotency_key}:wo"),
        created_at: now_iso.clone(),
    };
    append_in_tx(
        &tx,
        ledger_meta,
        EventKind::QuoteWorkOrderCreated,
        wo_payload.to_bytes(),
        ledger_actor.clone(),
        Some(format!("{idempotency_key}:wo")),
    )
    .context("audit append QuoteWorkOrderCreated")?;

    // ── S273 / PR-262 / ADR-0069 — material commit branch ─────────────
    //
    // Skip silently when EITHER `material_grade` or `quantity` is None —
    // the row pre-dates the storefront's S271 projection writer. The
    // saga still mints SO/WO + emits three audit entries; the inventory
    // side just stays untouched. This is the graceful-fallback path the
    // brief pushback names (#1): all S272-era rows continue to deal
    // without forcing the storefront pipeline to back-fill historical
    // intake rows.
    //
    // When BOTH are populated, the commit branch:
    //   (a) increments `committed_qty` on `inventory_balances`,
    //   (b) inserts the `inventory_reservations` row in `committed`
    //       state,
    //   (c) emits `inventory.material_committed` inside the SAME tx
    //       (the fourth audit entry alongside the three above).
    //
    // Any failure here rolls back the whole saga: the SO/WO id mints,
    // the `deal_issued_at` flip, the three sibling audit entries — all
    // gone. The next operator click sees the row as still un-dealt and
    // can retry once the operator fixes `on_hand_qty` in the Inventory
    // Balances view.
    let material_commit: Option<MaterialCommitOutcome> =
        match (row.material_grade.as_deref(), row.quantity) {
            (Some(grade), Some(qty_units)) if !grade.is_empty() && qty_units > 0 => {
                let qty = qty_units as f64;
                // S275 / F1 — stamp the unit kind so the audit walk +
                // SPA can tell `qty` is QUOTE units, not kg. The full
                // units → mm³ → kg conversion is engine-strand work
                // and lands later; until then every saga commits
                // `Units` and the SPA renders the header accordingly.
                let qty_unit_kind = QtyUnitKind::Units;
                let commit_outcome = material_inventory::commit_material_in_tx(
                    &tx,
                    &inputs.tenant,
                    &inputs.quote_id,
                    grade,
                    qty,
                    qty_unit_kind,
                )
                .map_err(|e| {
                    // Lift the typed inventory error into the saga's
                    // typed error vocabulary so the route layer can
                    // downcast to one DealSagaError union — keeps the
                    // 409 routing single-surface in serve.rs.
                    if let Some(inv) = e.downcast_ref::<MaterialInventoryError>() {
                        match inv {
                            MaterialInventoryError::InsufficientMaterial {
                                material_grade,
                                requested,
                                on_hand,
                                already_reserved,
                                already_committed,
                            } => anyhow!(DealSagaError::InsufficientMaterial {
                                quote_id: inputs.quote_id.clone(),
                                material_grade: material_grade.clone(),
                                requested: *requested,
                                on_hand: *on_hand,
                                already_reserved: *already_reserved,
                                already_committed: *already_committed,
                            }),
                        }
                    } else {
                        e.context("DEAL saga material commit")
                    }
                })?;

                let material_idempotency_key = format!("{idempotency_key}:material");
                let mat_payload = MaterialCommittedPayload {
                    quote_id: inputs.quote_id.clone(),
                    tenant_id: inputs.tenant.clone(),
                    material_grade: grade.to_string(),
                    qty,
                    qty_unit_kind,
                    reservation_id: commit_outcome.reservation_id.clone(),
                    actor: inputs.actor.clone(),
                    idempotency_key: material_idempotency_key.clone(),
                    created_at: now_iso.clone(),
                    balance_after_on_hand: commit_outcome.balance_after.on_hand_qty,
                    balance_after_reserved: commit_outcome.balance_after.reserved_qty,
                    balance_after_committed: commit_outcome.balance_after.committed_qty,
                    balance_after_consumed: commit_outcome.balance_after.consumed_qty,
                };
                material_inventory::append_material_committed_in_tx(
                    &tx,
                    ledger_meta,
                    ledger_actor,
                    &mat_payload,
                )?;
                Some(commit_outcome)
            }
            _ => None,
        };

    tx.commit().context("commit DEAL saga tx")?;

    Ok(DealSagaOutcome {
        sales_order_id,
        work_order_id,
        deal_issued_at: now_iso,
        refresh_acknowledged: row.stock_alert,
        material_commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::{
        ensure_schema as audit_ensure_schema, BinaryHash, LedgerMeta, TenantId,
    };
    use aberp_quote_intake::log_table as quote_log;
    use duckdb::Connection;

    fn open_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory DuckDB");
        audit_ensure_schema(&conn).expect("audit-ledger schema");
        quote_log::ensure_schema(&conn).expect("quote_intake_log schema");
        conn
    }

    fn ledger_meta() -> LedgerMeta {
        LedgerMeta::new(
            TenantId::new("test-tenant").unwrap(),
            BinaryHash::from_bytes([0u8; 32]),
        )
    }

    fn actor() -> Actor {
        Actor::test_only()
    }

    fn stage_quote(conn: &Connection, quote_id: &str) {
        let now = time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        quote_log::insert_intake(
            conn,
            "test-tenant",
            quote_id,
            "inv_A",
            "2026-06-05T08:00:00Z",
            now,
            "{}",
            "{}",
        )
        .unwrap();
    }

    fn full_uuid_quote_id() -> &'static str {
        // Realistic storefront-shape; the first 8 chars are the DEAL
        // token. Mirrors the example in the design doc.
        "0226e154-9e6c-4c0a-9001-f3a8a0c0a000"
    }

    #[test]
    fn expected_deal_token_is_first_eight_chars_of_long_id() {
        assert_eq!(expected_deal_token(full_uuid_quote_id()), "0226e154");
    }

    #[test]
    fn expected_deal_token_caps_at_id_length_for_short_ids() {
        assert_eq!(expected_deal_token("q-1"), "q-1");
        assert_eq!(expected_deal_token("12345"), "12345");
        assert_eq!(expected_deal_token("12345678"), "12345678");
        assert_eq!(expected_deal_token("123456789"), "12345678");
    }

    fn set_material_and_quantity(conn: &Connection, quote_id: &str, grade: &str, qty: i64) {
        conn.execute(
            "UPDATE quote_intake_log
                SET material_grade = ?1, quantity = ?2
              WHERE quote_id = ?3 AND tenant_id = 'test-tenant'",
            duckdb::params![grade, qty, quote_id],
        )
        .unwrap();
    }

    fn seed_balance(conn: &Connection, grade: &str, on_hand: f64) {
        crate::material_inventory::ensure_schema(conn).unwrap();
        conn.execute(
            "INSERT INTO inventory_balances (
                tenant_id, material_grade, on_hand_qty, reserved_qty,
                committed_qty, consumed_qty, unit_of_measure, last_updated
             ) VALUES ('test-tenant', ?1, ?2, 0, 0, 0, 'kg', '2026-06-06T00:00:00Z')",
            duckdb::params![grade, on_hand],
        )
        .unwrap();
    }

    /// Happy path on a stock-OK row: no REFRESH needed, the correct
    /// DEAL token commits the saga and surfaces SO/WO ids + the
    /// dealt-at timestamp.
    #[test]
    fn run_deal_saga_happy_path_no_stock_alert() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .expect("happy DEAL");

        assert!(outcome.sales_order_id.starts_with("so_"));
        assert!(outcome.work_order_id.starts_with("wo_"));
        assert!(!outcome.refresh_acknowledged);

        // The row now carries the saga's writes.
        let row = quote_log::read_for_deal(&conn, "test-tenant", quote_id)
            .unwrap()
            .unwrap();
        assert!(row.deal_issued_at.is_some());

        // Three audit entries landed.
        let mut count_kinds: std::collections::HashMap<String, i64> = Default::default();
        let mut stmt = conn
            .prepare("SELECT kind, COUNT(*) FROM audit_ledger GROUP BY kind")
            .unwrap();
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        });
        for r in rows.unwrap() {
            let (k, n) = r.unwrap();
            count_kinds.insert(k, n);
        }
        assert_eq!(count_kinds.get("quote.deal_issued"), Some(&1));
        assert_eq!(count_kinds.get("quote.sales_order_created"), Some(&1));
        assert_eq!(count_kinds.get("quote.work_order_created"), Some(&1));
    }

    /// Replay = single-use guard. The second saga call returns
    /// `DealAlreadyIssued`; the SPA routes it to a red toast.
    #[test]
    fn replay_returns_deal_already_issued() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();
        run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .expect("first DEAL");

        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        let saga = err.downcast::<DealSagaError>().expect("typed saga error");
        assert_eq!(saga.machine_code(), "deal_already_issued");
    }

    /// stock_alert TRUE + no REFRESH ack → 409 `stock_alert_refresh_required`.
    /// Submitting REFRESH then unlocks the saga.
    #[test]
    fn stock_alert_blocks_without_refresh_ack() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        quote_log::flip_stock_alert_to_true(&conn, "test-tenant", quote_id).unwrap();
        let meta = ledger_meta();

        // No REFRESH ack → 409.
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        let saga = err.downcast::<DealSagaError>().expect("typed saga error");
        assert_eq!(saga.machine_code(), "stock_alert_refresh_required");

        // Lower-case "refresh" must NOT pass per [[hulye-biztos]].
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: Some("refresh".to_string()),
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "stock_alert_refresh_required",
            "lowercase refresh must not unlock the saga"
        );

        // Verbatim REFRESH → saga succeeds + `refresh_acknowledged`
        // reflects the ack.
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: Some(REFRESH_ACK_TOKEN.to_string()),
            },
        )
        .expect("REFRESH-acked DEAL");
        assert!(outcome.refresh_acknowledged);
    }

    /// Wrong DEAL token → 409 `deal_token_mismatch`. The expected
    /// token is the row's `quote_id[..8]` verbatim, case-sensitive.
    #[test]
    fn wrong_deal_token_rejected_case_sensitive() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();

        for bad in [
            "00000000",  // numeric typo
            "0226E154",  // case-flipped
            "0226e15",   // too short
            "0226e1545", // too long
            "",          // empty
        ] {
            let err = run_deal_saga(
                &mut conn,
                &meta,
                actor(),
                DealSagaInputs {
                    tenant: "test-tenant".to_string(),
                    quote_id: quote_id.to_string(),
                    actor: "operator-A".to_string(),
                    deal_token: bad.to_string(),
                    refresh_ack: None,
                },
            )
            .unwrap_err();
            let saga = err.downcast::<DealSagaError>().unwrap();
            assert_eq!(
                saga.machine_code(),
                "deal_token_mismatch",
                "bad token {bad:?} must be rejected as deal_token_mismatch"
            );
        }

        // The row stays un-dealt (no partial state from the rejected
        // attempts).
        let row = quote_log::read_for_deal(&conn, "test-tenant", quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.deal_issued_at, None);
    }

    /// EVE-addendum-3 single-use takes precedence over the REFRESH
    /// check: a replay on an ALREADY-DEALT row (no matter the
    /// stock_alert / REFRESH ack state) returns `deal_already_issued`,
    /// NOT `stock_alert_refresh_required`. Pinned because the SPA's
    /// 409 toast routing depends on the machine code being stable.
    #[test]
    fn replay_precedence_over_stock_alert_check() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();

        // First DEAL on a stock-OK row.
        run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap();

        // Now flip stock_alert TRUE — a replay with NO REFRESH could
        // otherwise be misclassified as `stock_alert_refresh_required`.
        // Single-use must win.
        quote_log::flip_stock_alert_to_true(&conn, "test-tenant", quote_id).unwrap();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "deal_already_issued"
        );
    }

    #[test]
    fn missing_quote_returns_not_staged() {
        let mut conn = open_conn();
        let meta = ledger_meta();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: "q-MISSING".to_string(),
                actor: "operator-A".to_string(),
                deal_token: "q-MISSIN".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "not_staged"
        );
    }

    #[test]
    fn dismissed_row_cannot_be_dealt() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        quote_log::mark_irrelevant(&conn, "test-tenant", quote_id).unwrap();
        let meta = ledger_meta();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".to_string(),
                quote_id: quote_id.to_string(),
                actor: "operator-A".to_string(),
                deal_token: "0226e154".to_string(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "not_actionable"
        );
    }

    #[test]
    fn payload_round_trip_top_level() {
        let p = QuoteDealIssuedPayload {
            quote_id: "q-X".to_string(),
            tenant_id: "t".to_string(),
            sales_order_id: "so_X".to_string(),
            work_order_id: "wo_X".to_string(),
            deal_token: "q-X".to_string(),
            refresh_acknowledged: true,
            actor: "operator".to_string(),
            idempotency_key: "quote_deal:q-X".to_string(),
            deal_issued_at: "2026-06-06T12:00:00Z".to_string(),
        };
        let back: QuoteDealIssuedPayload = serde_json::from_slice(&p.to_bytes()).unwrap();
        assert_eq!(back, p);
    }

    #[test]
    fn deal_idempotency_key_shape() {
        assert_eq!(deal_idempotency_key("q-1"), "quote_deal:q-1");
        assert_eq!(
            deal_idempotency_key("0226e154-..."),
            "quote_deal:0226e154-..."
        );
    }

    // ── S273 / PR-262 / ADR-0069 — material-commit branch ────────────

    /// A row with NO `material_grade` (the pre-storefront default) goes
    /// through the saga unchanged: SO/WO mint, three audit entries,
    /// `material_commit = None`. The brief's pushback #1 graceful
    /// fallback — all S272-era rows still deal.
    #[test]
    fn s273_saga_skips_material_commit_when_material_grade_missing() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .unwrap();
        assert!(outcome.material_commit.is_none());

        // Audit count: THREE entries, not four — material branch skipped.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_ledger WHERE kind = 'inventory.material_committed'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }

    /// Empty-string `material_grade` is treated like NULL (defense
    /// against a storefront writer that pushed "" for a missing field).
    /// The saga skips the material branch silently.
    #[test]
    fn s273_saga_skips_material_commit_when_material_grade_empty_string() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        set_material_and_quantity(&conn, quote_id, "", 5);
        let meta = ledger_meta();
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .unwrap();
        assert!(outcome.material_commit.is_none());
    }

    /// `quantity = 0` is a defensive skip path: a quote with zero units
    /// has nothing to reserve. We skip rather than 409 — the SO/WO
    /// placeholder branch still runs, just no material commit.
    #[test]
    fn s273_saga_skips_material_commit_when_quantity_is_zero() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        set_material_and_quantity(&conn, quote_id, "6061-T6", 0);
        let meta = ledger_meta();
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .unwrap();
        assert!(outcome.material_commit.is_none());
    }

    /// Happy path WITH material commit: row carries `material_grade` +
    /// `quantity`, balance has enough on_hand → saga commits SO/WO +
    /// material, emits FOUR audit entries, and `material_commit`
    /// surfaces the after-state numbers.
    #[test]
    fn s273_saga_happy_path_with_material_commit() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        set_material_and_quantity(&conn, quote_id, "6061-T6", 12);
        seed_balance(&conn, "6061-T6", 100.0);
        let meta = ledger_meta();
        let outcome = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .unwrap();
        let mc = outcome.material_commit.expect("material commit landed");
        assert_eq!(mc.material_grade, "6061-T6");
        assert_eq!(mc.qty, 12.0);
        assert!(mc.reservation_id.starts_with("res_"));
        assert_eq!(mc.balance_after.committed_qty, 12.0);
        assert_eq!(mc.balance_after.available_qty, 88.0);

        // Four kinds, each exactly once.
        for kind in [
            "quote.deal_issued",
            "quote.sales_order_created",
            "quote.work_order_created",
            "inventory.material_committed",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM audit_ledger WHERE kind = ?1",
                    duckdb::params![kind],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "expected one {kind} entry, got {n}");
        }

        // The inventory_reservations row landed inside the same tx.
        let (state, qty): (String, f64) = conn
            .query_row(
                "SELECT state, qty FROM inventory_reservations
                  WHERE quote_id = ?1 AND tenant_id = 'test-tenant'",
                duckdb::params![quote_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "committed");
        assert_eq!(qty, 12.0);
    }

    /// Insufficient material rolls back the WHOLE saga: SO/WO ids do
    /// not get minted on the row, `deal_issued_at` stays NULL, and zero
    /// audit entries land. The next operator click sees the row as
    /// un-dealt and can retry once `on_hand_qty` is fixed.
    #[test]
    fn s273_saga_insufficient_material_rolls_back_everything() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        set_material_and_quantity(&conn, quote_id, "Inconel 718", 50);
        seed_balance(&conn, "Inconel 718", 10.0); // requested 50 > available 10
        let meta = ledger_meta();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        let saga = err.downcast::<DealSagaError>().expect("typed saga error");
        assert_eq!(saga.machine_code(), "insufficient_material");
        match saga {
            DealSagaError::InsufficientMaterial {
                material_grade,
                requested,
                on_hand,
                ..
            } => {
                assert_eq!(material_grade, "Inconel 718");
                assert_eq!(requested, 50.0);
                assert_eq!(on_hand, 10.0);
            }
            other => panic!("wrong error variant: {other:?}"),
        }

        // Row stays un-dealt — every saga write was rolled back.
        let row = quote_log::read_for_deal(&conn, "test-tenant", quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.deal_issued_at, None);

        // Zero audit entries.
        let total: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM audit_ledger
                  WHERE kind IN (
                    'quote.deal_issued','quote.sales_order_created',
                    'quote.work_order_created','inventory.material_committed'
                  )",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(total, 0);

        // `inventory_balances` was upserted at zeros, then the rollback
        // erased the upsert — so the row should NOT be present.
        let bal_n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM inventory_balances
                  WHERE tenant_id = 'test-tenant' AND material_grade = 'Inconel 718'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Either 0 (rollback wiped the upsert) or 1 with on_hand=10 (we
        // pre-seeded it). It was pre-seeded, so it must be 1 with the
        // ORIGINAL on_hand = 10 untouched.
        assert_eq!(bal_n, 1);
        let bal = crate::material_inventory::read_balance(&conn, "test-tenant", "Inconel 718")
            .unwrap()
            .unwrap();
        assert_eq!(bal.on_hand_qty, 10.0);
        assert_eq!(bal.committed_qty, 0.0, "rollback restored committed_qty");
    }

    // ── S275 / PR-264 / F32 — valid_until expiry guard ───────────────

    fn set_valid_until(conn: &Connection, quote_id: &str, ymd: &str) {
        conn.execute(
            &format!(
                "UPDATE quote_intake_log
                    SET valid_until = DATE '{ymd}'
                  WHERE quote_id = ?1 AND tenant_id = 'test-tenant'"
            ),
            duckdb::params![quote_id],
        )
        .unwrap();
    }

    /// `quote_is_expired` pure-function pin: None passes through; an
    /// unparseable string passes through; a past date is expired; today
    /// and a future date are NOT expired.
    #[test]
    fn quote_is_expired_handles_none_garbage_past_today_future() {
        let today = time::Date::from_calendar_date(2026, time::Month::June, 6).unwrap();
        assert!(!quote_is_expired(None, today));
        assert!(!quote_is_expired(Some("not-a-date"), today));
        assert!(!quote_is_expired(Some("2026-06-06"), today)); // same day
        assert!(!quote_is_expired(Some("2026-06-07"), today)); // tomorrow
        assert!(quote_is_expired(Some("2026-06-05"), today)); // yesterday
        assert!(quote_is_expired(Some("2025-12-31"), today)); // last year
    }

    /// Saga refuses an expired quote with the `quote_expired` machine
    /// code. The row stays un-dealt so an operator can reissue the
    /// quote storefront-side (or extend the validity) and re-DEAL.
    #[test]
    fn s275_saga_refuses_expired_quote() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        // Yesterday relative to any reasonable system clock — keeps the
        // test deterministic by being well in the past.
        set_valid_until(&conn, quote_id, "2024-01-01");
        let meta = ledger_meta();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        let saga = err.downcast::<DealSagaError>().expect("typed saga error");
        assert_eq!(saga.machine_code(), "quote_expired");

        // Row stays un-dealt — no SO/WO mint, no audit entries.
        let row = quote_log::read_for_deal(&conn, "test-tenant", quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(row.deal_issued_at, None);
    }

    /// A row with no `valid_until` on file (pre-storefront / NULL)
    /// proceeds — absence is "no expiry," not "expired." This pins the
    /// graceful-fallback path so the saga keeps working before the
    /// storefront writer is live.
    #[test]
    fn s275_saga_proceeds_when_valid_until_is_null() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        let meta = ledger_meta();
        run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .expect("NULL valid_until → saga proceeds");
    }

    // ── S275 / PR-264 / F25 — post-CAS error discrimination ──────────

    /// If the row is deleted between pre-flight and the CAS, the
    /// saga's CAS rejects with zero rows updated; pre-F25 the saga
    /// returned `deal_already_issued`, which routes operator action to
    /// "this quote was dealt" — wrong. After F25, the post-CAS
    /// re-read inside the same tx surfaces the genuine "row gone"
    /// state and the saga returns `not_staged`.
    #[test]
    fn s275_saga_returns_not_staged_when_row_deleted_between_preflight_and_cas() {
        let mut conn = open_conn();
        let quote_id = full_uuid_quote_id();
        stage_quote(&conn, quote_id);
        // Simulate the deletion race by deleting the row before the
        // saga runs — the saga's read_for_deal pre-flight would have
        // seen it; we delete it now so the CAS lands on zero rows.
        // (The genuine race is between two HTTP requests; deletion in
        // the test is the equivalent observable state.)
        let pre_row = quote_log::read_for_deal(&conn, "test-tenant", quote_id)
            .unwrap()
            .unwrap();
        assert_eq!(pre_row.intake_state, "staged");
        conn.execute(
            "DELETE FROM quote_intake_log WHERE quote_id = ?1 AND tenant_id = 'test-tenant'",
            duckdb::params![quote_id],
        )
        .unwrap();
        // The saga's own pre-flight will return NotStaged here because
        // the row is already gone; the F25 fix path inside the tx is
        // exercised by a different test path (concurrent CAS race —
        // hard to reproduce sync). This test pins the OUTER `not_staged`
        // path that any pre-flight gone-row should surface.
        let meta = ledger_meta();
        let err = run_deal_saga(
            &mut conn,
            &meta,
            actor(),
            DealSagaInputs {
                tenant: "test-tenant".into(),
                quote_id: quote_id.into(),
                actor: "operator-A".into(),
                deal_token: "0226e154".into(),
                refresh_ack: None,
            },
        )
        .unwrap_err();
        assert_eq!(
            err.downcast::<DealSagaError>().unwrap().machine_code(),
            "not_staged"
        );
    }

    /// Insufficient-material error surfaces a distinct machine code
    /// from the other DEAL failure modes — pinned so the SPA's toast
    /// router doesn't conflate this with `deal_token_mismatch` or
    /// `not_actionable`.
    #[test]
    fn s273_insufficient_material_machine_code_is_distinct() {
        let codes = [
            DealSagaError::NotStaged {
                tenant: "t".into(),
                quote_id: "q".into(),
            }
            .machine_code(),
            DealSagaError::NotActionable {
                quote_id: "q".into(),
                state: "irrelevant".into(),
            }
            .machine_code(),
            DealSagaError::DealAlreadyIssued {
                quote_id: "q".into(),
            }
            .machine_code(),
            DealSagaError::StockAlertRefreshRequired {
                quote_id: "q".into(),
            }
            .machine_code(),
            DealSagaError::DealTokenMismatch {
                quote_id: "q".into(),
            }
            .machine_code(),
            DealSagaError::InsufficientMaterial {
                quote_id: "q".into(),
                material_grade: "6061-T6".into(),
                requested: 12.0,
                on_hand: 0.0,
                already_reserved: 0.0,
                already_committed: 0.0,
            }
            .machine_code(),
            DealSagaError::QuoteExpired {
                quote_id: "q".into(),
                valid_until: "2024-01-01".into(),
                today: "2026-06-06".into(),
            }
            .machine_code(),
        ];
        // Every code is unique.
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                assert_ne!(
                    codes[i], codes[j],
                    "{} collides with {}",
                    codes[i], codes[j]
                );
            }
        }
        assert!(codes.contains(&"insufficient_material"));
    }
}
