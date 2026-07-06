//! AP-side (Accounts Payable) incoming-invoice store + status workflow.
//!
//! S177 / PR-177 — the BACKEND half of the AP module v1.
//!
//! # Scope
//!
//! INCOMING invoices are issued BY suppliers TO this tenant. They are
//! NOT outgoing invoices and they are NOT regulated by NAV's
//! per-invoice sequence allocator (the tenant did not issue them, did
//! not burn a sequence slot, has no NAV `<invoiceReference>` chain).
//! The local mirror exists so the operator can:
//!
//!   1. See every supplier invoice in one place (eventually pulled
//!      from NAV automatically via `queryInvoiceDigest INBOUND` — that
//!      auto-sync is a SEPARATE follow-on PR; see the deferred-work
//!      note below).
//!   2. Annotate each one as `Outstanding` (default) → `Paid` (the
//!      tenant paid the supplier) → `Outstanding` (operator unwinds)
//!      OR `Outstanding` → `Irrelevant{reason}` (operator declares
//!      the invoice not-our-problem with a required justification).
//!
//! NAV is the source of truth for the invoice's facts (supplier, dates,
//! totals); ABERP adds only the local annotation status + the audit
//! trail of operator decisions.
//!
//! # Architecture decisions for THIS PR (S177)
//!
//!   - **Schema lives in this binary, not in `aberp-billing`.** The
//!     billing module owns the outgoing-invoice allocator + the
//!     regulated NAV state ladder (ADR-0009). Incoming invoices have
//!     none of that — coupling them to billing would burden the
//!     regulated path with operator-metadata concerns. Same posture
//!     `partners` and `products` take (per-tenant operator-managed
//!     data lives in the binary, not the regulated module).
//!   - **No foreign keys** per ADR-0019. The `ap_invoice` table is
//!     keyed by `(supplier_tax_number, nav_invoice_number)` for
//!     idempotency on ingestion; the audit chain reaches the row by
//!     ULID-prefixed `apinv_<ULID>` id.
//!   - **Closed-vocab `IncomingInvoiceStatus`** per CLAUDE.md rule 5
//!     (deterministic transitions; the operator's choice is the only
//!     model-side judgment, and even there the wire vocab is a
//!     three-state enum).
//!   - **Audit kinds use `system.` prefix**, not `invoice.`, so the
//!     per-OUTGOING-invoice export bundle's `invoice.*` glob never
//!     sweeps an AP-side entry. See
//!     `crates/audit-ledger/src/entry/event_kind.rs::IncomingInvoiceIngested`.
//!   - **Idempotent ingestion.** Re-ingesting the same supplier's
//!     same invoice number is a no-op — the `UNIQUE
//!     (supplier_tax_number, nav_invoice_number)` constraint surfaces
//!     duplicates loud, and the helper returns the EXISTING row's id
//!     so the caller can echo it. Matches the operator-twin's mental
//!     model: NAV will re-emit the same digest on every poll; the
//!     daemon must not multiply rows.
//!   - **Race-safe under concurrent ingest (S186 / PR-186).** Two
//!     concurrent callers (the daemon racing a manual `/sync-now`,
//!     two boot-tick paths overlapping, etc.) hitting
//!     [`ingest_incoming_invoice`] are serialized via a process-wide
//!     [`std::sync::Mutex<()>`][`INGEST_SERIALIZER`]. PR-182 review
//!     §S177 flagged the find-then-insert race as a UNIQUE-violation
//!     → 500. Investigation under PR-186 surfaced the deeper
//!     reality: DuckDB's UNIQUE constraint does NOT fire across two
//!     `Connection::open` handles in the same process (each handle
//!     opens its own `Database` instance with no cross-handle index
//!     coordination), AND the audit-ledger's chain-hashing assumes
//!     serial writers (concurrent writers produce
//!     `tamper detected at seq=1` mismatches). Both failure modes
//!     are pinned by tests below
//!     ([`app_layer_dedup_is_authoritative_even_though_db_unique_does_not_fire`],
//!     [`concurrent_ingest_holds_no_error_one_row_id_consistent`]).
//!     The serializer eliminates BOTH races (UNIQUE + audit-chain)
//!     at the source: only one ingest at a time, regardless of
//!     caller. Defence-in-depth: the INSERT arm still catches a
//!     duplicate-key error and recovers gracefully, in case the
//!     serializer is bypassed by a future code path (e.g., a CLI
//!     invocation running in a SEPARATE process from `aberp serve`
//!     — that path would race across the file lock and the
//!     in-process mutex would not see it).
//!
//! # Deferred-work flags
//!
//!   - **Auto-sync via `queryInvoiceDigest INBOUND` is a SEPARATE
//!     PR.** `nav-transport` does not yet provide the digest envelope
//!     renderer or the digest-list response parser — adding it
//!     requires NAV-testbed verification of the response shape (per
//!     the same posture every prior NAV operation took; see
//!     `render_query_invoice_data_request` for the precedent comment).
//!     The session-177 brief named the daemon as part of this PR, but
//!     the conservative call is to land the foundation first so S178's
//!     UI is unblocked and the daemon can be wired in a dedicated PR
//!     with its own NAV-testbed verification window. The
//!     [`ingest_incoming_invoice`] helper below is the exact entry
//!     point the future daemon will call.
//!   - **Raw NAV XML storage.** When the caller supplies a raw NAV
//!     InvoiceData XML, the bytes are written to
//!     `~/.aberp/<tenant>/ap-artifacts/<apinv_id>.xml` (matching the
//!     outgoing side's `nav_xml` path posture from PR-18). The audit
//!     payload carries the SHA-256 hex of those bytes for tamper-
//!     detection; the raw bytes themselves are NOT in the payload (the
//!     hash chain stays compact; an inspector can fetch the bytes from
//!     the well-known path).
//!   - **NAV XML parsing.** This module DOES NOT parse the NAV
//!     InvoiceData XML — the caller (the operator's manual ingest, or
//!     the future daemon) extracts the typed fields and passes them
//!     in. Building a full NAV InvoiceData parser is out of scope for
//!     S177 (it's the symmetric counterpart of `nav_xml.rs` on the
//!     emit side — substantial work that belongs in its own PR with
//!     fixture-based test coverage).

use aberp_audit_ledger::{self as audit_ledger, Actor, EventKind, Ledger, LedgerMeta, TenantId};
use aberp_billing::IdempotencyKey;
use anyhow::{anyhow, Context, Result};
use duckdb::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use ulid::Ulid;

use crate::audit_payloads::{IncomingInvoiceIngestedPayload, IncomingInvoiceStatusChangedPayload};

// ──────────────────────────────────────────────────────────────────────
// IncomingInvoiceId — prefixed-ULID newtype (`apinv_<26-char-ULID>`).
// ──────────────────────────────────────────────────────────────────────

/// ULID newtype rendered as `apinv_<26-char-ULID>` on the wire.
/// Mirrors `ProductId` / `PartnerId` per ADR-0005 — every entity gets
/// its own prefixed-ULID newtype so type confusion at a call site is
/// a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IncomingInvoiceId(pub Ulid);

impl IncomingInvoiceId {
    pub fn new() -> Self {
        Self(Ulid::new())
    }

    pub fn to_prefixed_string(self) -> String {
        format!("apinv_{}", self.0)
    }

    pub fn parse_prefixed(s: &str) -> Result<Self> {
        let body = s.strip_prefix("apinv_").ok_or_else(|| {
            anyhow!(
                "incoming-invoice id `{}` missing `apinv_` prefix per ADR-0005",
                s
            )
        })?;
        let ulid = Ulid::from_string(body).map_err(|e| {
            anyhow!(
                "incoming-invoice id `{}` body is not a valid 26-char ULID: {}",
                s,
                e
            )
        })?;
        Ok(Self(ulid))
    }
}

impl Default for IncomingInvoiceId {
    fn default() -> Self {
        Self::new()
    }
}

// ──────────────────────────────────────────────────────────────────────
// IncomingInvoiceStatus — closed-vocab three-state enum.
// ──────────────────────────────────────────────────────────────────────

/// Operator-decided status of an AP-side incoming invoice.
///
/// Closed vocab per CLAUDE.md rule 5. Transitions allowed by this PR:
///
///   - Ingestion default → `Outstanding`.
///   - `Outstanding` → `Paid` (operator records payment).
///   - `Outstanding` → `Irrelevant` (operator declares not-our-problem;
///     reason required).
///   - `Paid` → `Outstanding` (operator unwinds a prior `Paid` mark).
///   - `Irrelevant` → `Outstanding` (operator unwinds a prior
///     `Irrelevant` mark).
///
/// `Paid` → `Irrelevant` and vice versa are NOT permitted in v1 — the
/// route layer rejects them with 400 to surface the conflict per
/// CLAUDE.md rule 7. The operator who needs that transition can clear
/// to `Outstanding` first; the audit chain records both steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncomingInvoiceStatus {
    /// Initial state at ingestion and the canonical "default" status.
    Outstanding,
    /// The tenant paid the supplier. The audit chain carries
    /// who/when/why via `IncomingInvoiceStatusChanged`.
    Paid,
    /// Operator declared the invoice not-our-problem (typical reasons:
    /// duplicate of another supplier's emission, supplier billed wrong
    /// tenant, test invoice that ended up in production). The audit
    /// payload's `reason` field is REQUIRED for this transition.
    Irrelevant,
}

impl IncomingInvoiceStatus {
    /// Render in the on-disk + wire form. Paired with
    /// [`Self::from_storage_str`] as a round-trip-proven pair (unit
    /// test below).
    pub fn as_str(self) -> &'static str {
        match self {
            IncomingInvoiceStatus::Outstanding => "Outstanding",
            IncomingInvoiceStatus::Paid => "Paid",
            IncomingInvoiceStatus::Irrelevant => "Irrelevant",
        }
    }

    /// Parse the on-disk form back into a status. Errors on unknown
    /// strings — silent fallback would mask schema drift per
    /// CLAUDE.md rule 12 (fail loud).
    pub fn from_storage_str(s: &str) -> Result<Self> {
        match s {
            "Outstanding" => Ok(IncomingInvoiceStatus::Outstanding),
            "Paid" => Ok(IncomingInvoiceStatus::Paid),
            "Irrelevant" => Ok(IncomingInvoiceStatus::Irrelevant),
            other => Err(anyhow!(
                "unknown IncomingInvoiceStatus storage string: `{}` \
                 (expected `Outstanding` / `Paid` / `Irrelevant`)",
                other
            )),
        }
    }
}

// ──────────────────────────────────────────────────────────────────────
// IncomingInvoice — read model.
// ──────────────────────────────────────────────────────────────────────

/// The full row shape served to read-side callers (list + detail).
///
/// Mirrors the `ap_invoice` table 1:1 with one transform: the
/// `local_status` column is a string in the DB and the typed enum on
/// the wire (serde-renames at the boundary).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IncomingInvoice {
    pub id: String,
    pub supplier_tax_number: String,
    pub supplier_name: String,
    pub supplier_address: Option<String>,
    pub nav_invoice_number: String,
    pub issue_date: String,
    pub delivery_date: Option<String>,
    pub payment_deadline: Option<String>,
    pub total_net_minor: i64,
    pub total_vat_minor: i64,
    pub total_gross_minor: i64,
    pub currency: String,
    /// Closed vocab — `IncomingInvoiceStatus::as_str` output.
    pub local_status: String,
    /// `Some(_)` IFF `local_status == "Irrelevant"`. The route layer
    /// enforces this invariant at write time.
    pub irrelevant_reason: Option<String>,
    /// Filesystem path to the raw NAV InvoiceData XML, `None` for
    /// operator-typed entries with no XML supplied. The audit
    /// payload's `nav_xml_sha256` is the tamper-detection hash for
    /// these bytes.
    pub nav_xml_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

// ──────────────────────────────────────────────────────────────────────
// IngestionInput — typed input shape for the ingestion entry point.
// ──────────────────────────────────────────────────────────────────────

/// Inputs to [`ingest_incoming_invoice`]. The caller (manual route
/// handler or future auto-sync daemon) extracts these fields from the
/// supplier's NAV InvoiceData XML — this module does NOT parse XML
/// itself per the architecture decision named in the module docs.
#[derive(Debug, Clone, Deserialize)]
pub struct IngestionInput {
    pub supplier_tax_number: String,
    pub supplier_name: String,
    pub supplier_address: Option<String>,
    pub nav_invoice_number: String,
    pub issue_date: String,
    pub delivery_date: Option<String>,
    pub payment_deadline: Option<String>,
    pub total_net_minor: i64,
    pub total_vat_minor: i64,
    pub total_gross_minor: i64,
    pub currency: String,
    /// Raw NAV InvoiceData XML bytes, if available. The wire shape
    /// is base64 (so JSON-safe); the deserializer decodes inline.
    /// When present, the bytes are written to
    /// `~/.aberp/<tenant>/ap-artifacts/<id>.xml` and the SHA-256 hex
    /// lands in the audit payload.
    #[serde(default, deserialize_with = "deserialize_optional_base64")]
    pub nav_xml: Option<Vec<u8>>,
}

fn deserialize_optional_base64<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use base64::Engine;
    use serde::Deserialize;
    let opt = Option::<String>::deserialize(deserializer)?;
    match opt {
        Some(s) => base64::engine::general_purpose::STANDARD
            .decode(s.as_bytes())
            .map(Some)
            .map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

/// Outcome of [`ingest_incoming_invoice`].
#[derive(Debug, Clone)]
pub enum IngestOutcome {
    /// A new row was inserted and an audit entry written.
    Created { id: String },
    /// The (supplier_tax_number, nav_invoice_number) pair already
    /// existed; no row was inserted, no audit entry was written. The
    /// returned id is the EXISTING row's id so the caller can echo it.
    AlreadyExists { id: String },
}

/// Outcome of a status transition.
#[derive(Debug, Clone)]
pub struct StatusChangeOutcome {
    pub id: String,
    pub from_status: String,
    pub to_status: String,
    pub reason: Option<String>,
    pub entries_verified: u64,
}

/// Typed errors from the status-change path. The route layer maps
/// each variant to the right HTTP status — same closed-vocab posture
/// as `MarkPaidError`.
#[derive(Debug)]
pub enum StatusChangeError {
    /// Unknown `ap_invoice_id`. Maps to 404.
    NotFound,
    /// Operator asked for a transition the closed graph does not
    /// allow (e.g., `Paid` → `Irrelevant` directly). Maps to 400.
    InvalidTransition { from: String, to: String },
    /// `to_status == "Irrelevant"` but no `reason` supplied or
    /// reason was whitespace. Maps to 400.
    ReasonRequiredForIrrelevant,
    /// Storage / audit-write / chain-verify error. Maps to 500.
    Other(anyhow::Error),
}

impl From<anyhow::Error> for StatusChangeError {
    fn from(e: anyhow::Error) -> Self {
        StatusChangeError::Other(e)
    }
}

/// Typed errors from the ingestion path. The route layer maps each
/// variant to the right HTTP status.
#[derive(Debug)]
pub enum IngestError {
    /// One of the typed input fields failed validation (empty
    /// supplier_tax_number, malformed issue_date, etc.). Maps to 400.
    InvalidInput(String),
    /// Storage / audit-write / chain-verify error. Maps to 500.
    Other(anyhow::Error),
}

impl From<anyhow::Error> for IngestError {
    fn from(e: anyhow::Error) -> Self {
        IngestError::Other(e)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Schema.
// ──────────────────────────────────────────────────────────────────────

// S410 / [[no-sql-specific]] — no DB-level CHECK on `currency` /
// `local_status`. The closed vocab is enforced in Rust:
// `validate_ingestion_input` rejects out-of-vocab `currency`, only the
// `IncomingInvoiceStatus` enum ever writes `local_status`, and the read
// path rejects out-of-vocab values via `IncomingInvoiceStatus::from_storage_str`.
const AP_INVOICE_SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS ap_invoice (
    id                   VARCHAR NOT NULL PRIMARY KEY,
    tenant_id            VARCHAR NOT NULL,
    supplier_tax_number  VARCHAR NOT NULL,
    supplier_name        VARCHAR NOT NULL,
    supplier_address     VARCHAR,
    nav_invoice_number   VARCHAR NOT NULL,
    issue_date           VARCHAR NOT NULL,
    delivery_date        VARCHAR,
    payment_deadline     VARCHAR,
    total_net_minor      BIGINT  NOT NULL,
    total_vat_minor      BIGINT  NOT NULL,
    total_gross_minor    BIGINT  NOT NULL,
    currency             VARCHAR NOT NULL,
    local_status         VARCHAR NOT NULL,
    irrelevant_reason    VARCHAR,
    nav_xml_path         VARCHAR,
    created_at           VARCHAR NOT NULL,
    updated_at           VARCHAR NOT NULL,
    UNIQUE (tenant_id, supplier_tax_number, nav_invoice_number)
);
CREATE INDEX IF NOT EXISTS ap_invoice_tenant_status_idx
    ON ap_invoice (tenant_id, local_status);
CREATE INDEX IF NOT EXISTS ap_invoice_tenant_issue_idx
    ON ap_invoice (tenant_id, issue_date);
";

/// Idempotent `CREATE TABLE IF NOT EXISTS` for the `ap_invoice` table.
/// Called at serve boot per the same hot-path posture as
/// `partners::ensure_schema` and `products::ensure_schema`.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    // ADR-0098 C2 fix-forward — no-op on a read-only conn (read_returns_readonly
    // read()-side); the schema is created by a writer before any read reaches
    // here. A genuine write mis-routed through read() still fails loud (F5).
    if aberp_audit_ledger::connection_is_read_only(conn) {
        return Ok(());
    }
    conn.execute_batch(AP_INVOICE_SCHEMA_SQL)
        .context("ensure ap_invoice schema")
}

/// S186 / PR-186 — process-wide serializer around
/// [`ingest_incoming_invoice`]. See the module docs' "Race-safe under
/// concurrent ingest" bullet for the why; this static holds the
/// `Mutex<()>` two callers contend for. A single-tenant-per-process
/// `aberp serve` (the current model) makes process-wide equivalent
/// to per-tenant; a multi-tenant binary would need keyed-by-tenant
/// storage. Granularity-of-one is conservative: the daemon ingests
/// digests sequentially anyway, and the manual `/sync-now` path is
/// operator-paced.
static INGEST_SERIALIZER: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ──────────────────────────────────────────────────────────────────────
// Ingestion.
// ──────────────────────────────────────────────────────────────────────

/// Ingest a supplier-issued INCOMING invoice into the local mirror.
///
/// Idempotent on `(tenant_id, supplier_tax_number, nav_invoice_number)`
/// — re-ingesting the same supplier's same invoice number returns
/// `AlreadyExists { id: <existing> }` without inserting a duplicate
/// and without writing a redundant audit entry. The future auto-sync
/// daemon depends on this idempotency (NAV will re-emit the same
/// digest on every poll cycle).
///
/// When `input.nav_xml` is `Some(bytes)`, the bytes are written to
/// `~/.aberp/<tenant>/ap-artifacts/<apinv_id>.xml` and the SHA-256
/// hex of those bytes is stamped onto the audit payload. The raw
/// bytes themselves are NOT in the audit payload — the hash chain
/// stays compact; an inspector can fetch the bytes from the
/// well-known path.
pub fn ingest_incoming_invoice(
    db: &aberp_db::HandleArc,
    tenant: TenantId,
    binary_hash: audit_ledger::BinaryHash,
    operator_login: &str,
    ap_artifacts_dir: &Path,
    input: IngestionInput,
) -> std::result::Result<IngestOutcome, IngestError> {
    validate_ingestion_input(&input)?;

    // S186 — process-wide serialization. See [`INGEST_SERIALIZER`]'s
    // doc-comment for the rationale: DuckDB does not enforce UNIQUE
    // across two `Connection::open` handles in the same process, and
    // the audit-ledger's chain hashing assumes serial writers. The
    // mutex is held for the ENTIRE critical section (find-or-insert
    // + audit-append + chain-verify + mirror-sync) — granularity-of-
    // one is conservative; the daemon ingests serially anyway and
    // the manual route is operator-paced.
    //
    // `lock()` only returns `Err` on a poisoned mutex (a prior
    // panic mid-critical-section). Such a panic would already be a
    // bug worth surfacing; we propagate it as `IngestError::Other`
    // rather than swallow.
    let _guard = INGEST_SERIALIZER
        .lock()
        .map_err(|_| anyhow!("ingest serializer mutex poisoned by a prior panic"))?;

    // ADR-0098 R3 (finding C) — route the AP-ingest INSERT + audit append through
    // the shared Handle's serialized writer (db.write()) instead of an independent
    // Connection::open of the live path. That separate opener was the
    // daemon-frequency (~2s bootstrap-year backfill cadence, driven by ap_sync)
    // swap-orphan silent-write-loss vector + duckdb#23046 in-place-fold re-open
    // locus. The WriteGuard's post-commit hook runs the lockstep sync_mirror on
    // drop; chain verification reuses a shared read clone below (never a second
    // independent opener).
    let mut conn = db
        .write()
        .map_err(|e| IngestError::Other(anyhow!("shared writer for ap_invoice ingestion: {e}")))?;
    ensure_schema(&conn).context("ensure ap_invoice schema (ingestion)")?;
    audit_ledger::ensure_schema(&conn).context("ensure audit-ledger schema (ingestion)")?;

    // S410 / [[no-sql-specific]] — THE authoritative dedup gate.
    // This pre-insert probe, run while `INGEST_SERIALIZER` is held for
    // the entire critical section (find → insert → audit), is what
    // guarantees at-most-one row per `(tenant, supplier_tax,
    // nav_invoice_number)`. It does NOT depend on the DB firing a UNIQUE
    // violation — the app-level mutex + this probe are the gate, so the
    // dedup is portable to any engine (including ones where UNIQUE does
    // not fire across connections — see the
    // `duckdb_unique_does_not_fire_*` test below). Running it FIRST also
    // means we never write the artifact file or the audit entry for a row
    // that already exists.
    if let Some(existing_id) = find_existing_id(
        &conn,
        tenant.as_str(),
        &input.supplier_tax_number,
        &input.nav_invoice_number,
    )? {
        return Ok(IngestOutcome::AlreadyExists { id: existing_id });
    }

    // Mint the row id BEFORE writing the artifact so the filename
    // matches the row id.
    let id = IncomingInvoiceId::new().to_prefixed_string();

    // Optional NAV XML artifact + its hash.
    let (nav_xml_path, nav_xml_sha256) = match &input.nav_xml {
        Some(bytes) => {
            std::fs::create_dir_all(ap_artifacts_dir).with_context(|| {
                format!(
                    "create AP artifacts directory at {}",
                    ap_artifacts_dir.display()
                )
            })?;
            let path: PathBuf = ap_artifacts_dir.join(format!("{}.xml", id));
            std::fs::write(&path, bytes)
                .with_context(|| format!("write AP NAV XML artifact to {}", path.display()))?;
            let mut hasher = Sha256::new();
            hasher.update(bytes);
            let hash = hex::encode(hasher.finalize());
            (Some(path.to_string_lossy().to_string()), Some(hash))
        }
        None => (None, None),
    };

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format ap_invoice timestamp as Rfc3339")?;

    let idempotency_key = IdempotencyKey::new();
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, operator_login);
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash);

    // INSERT + audit append in ONE transaction so a crash leaves
    // neither a row without an audit entry nor an audit entry without
    // a row. Mirrors the outgoing-invoice billing+audit posture per
    // ADR-0008 / ADR-0009 §3 step 6.
    //
    // S410 — the UNIQUE-violation catch below is a BACKSTOP, not the
    // primary dedup gate. The authoritative gate is the `find_existing_id`
    // probe above, held under `INGEST_SERIALIZER` (see its comment). On
    // the engines we target, a UNIQUE violation here is therefore
    // unreachable in a single-process model. The catch is retained for
    // defence-in-depth (e.g. a hypothetical cross-process writer): if the
    // INSERT ever trips the `(tenant_id, supplier_tax_number,
    // nav_invoice_number)` UNIQUE, we re-look-up and return
    // `AlreadyExists` rather than surfacing a 500 — preserving the
    // idempotency contract the daemon depends on. (Pre-S186 a racing
    // caller surfaced a 500; the mutex closed that race, and this catch
    // remains the belt to its suspenders.)
    let tx = conn
        .transaction()
        .context("begin DuckDB transaction (ap_invoice ingest)")?;
    let insert_result = tx.execute(
        "INSERT INTO ap_invoice (
            id, tenant_id, supplier_tax_number, supplier_name, supplier_address,
            nav_invoice_number, issue_date, delivery_date, payment_deadline,
            total_net_minor, total_vat_minor, total_gross_minor, currency,
            local_status, irrelevant_reason, nav_xml_path, created_at, updated_at
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'Outstanding', NULL, ?, ?, ?);",
        params![
            &id,
            tenant.as_str(),
            &input.supplier_tax_number,
            &input.supplier_name,
            input.supplier_address.as_deref(),
            &input.nav_invoice_number,
            &input.issue_date,
            input.delivery_date.as_deref(),
            input.payment_deadline.as_deref(),
            input.total_net_minor,
            input.total_vat_minor,
            input.total_gross_minor,
            &input.currency,
            nav_xml_path.as_deref(),
            &now,
            &now,
        ],
    );
    if let Err(e) = insert_result {
        if is_duplicate_key_violation(&e) {
            // Race lost — another concurrent caller inserted the
            // same `(tenant, supplier_tax, nav_invoice_number)`
            // between our upfront `find_existing_id` and this
            // INSERT. Roll back, clean up the orphan artifact file
            // we just wrote (the winning row points to ITS own
            // freshly-minted id; ours would dangle), look up the
            // surviving row's id, return `AlreadyExists`.
            drop(tx);
            if let Some(path) = nav_xml_path.as_deref() {
                let _ = std::fs::remove_file(path);
            }
            return match find_existing_id(
                &conn,
                tenant.as_str(),
                &input.supplier_tax_number,
                &input.nav_invoice_number,
            )? {
                Some(existing) => Ok(IngestOutcome::AlreadyExists { id: existing }),
                None => Err(IngestError::Other(anyhow!(
                    "ap_invoice INSERT raised a UNIQUE violation but \
                     no existing row was found for tenant={} \
                     supplier_tax={} nav_invoice_number={} — schema or \
                     constraint drift?",
                    tenant.as_str(),
                    input.supplier_tax_number,
                    input.nav_invoice_number,
                ))),
            };
        }
        return Err(IngestError::Other(anyhow!("INSERT into ap_invoice: {e}")));
    }

    let payload = IncomingInvoiceIngestedPayload {
        ap_invoice_id: id.clone(),
        idempotency_key: idempotency_key.to_canonical_string(),
        supplier_tax_number: input.supplier_tax_number.clone(),
        supplier_name: input.supplier_name.clone(),
        nav_invoice_number: input.nav_invoice_number.clone(),
        issue_date: input.issue_date.clone(),
        payment_deadline: input.payment_deadline.clone(),
        total_gross_minor: input.total_gross_minor,
        currency: input.currency.clone(),
        nav_xml_sha256,
    };
    audit_ledger::append_in_tx(
        &tx,
        &ledger_meta,
        EventKind::IncomingInvoiceIngested,
        payload.to_bytes(),
        actor,
        Some(idempotency_key.to_canonical_string()),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx IncomingInvoiceIngested: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (ap_invoice ingest)")?;

    // ADR-0098 R3 (finding C) / C2 pattern — the WriteGuard's drop runs the
    // lockstep sync_mirror post-commit hook, so the explicit Ledger::open +
    // sync_mirror (a SECOND independent live opener) is removed. Chain
    // verification reuses a shared READ clone (Ledger::from_connection over
    // db.read() — a try_clone of the one instance, coherent), never an
    // independent Connection::open / Ledger::open (the duckdb#23046 replay locus).
    drop(conn); // WriteGuard drop -> post-commit lockstep sync_mirror
    let verify_conn = db.read().map_err(|e| {
        IngestError::Other(anyhow!(
            "shared read to verify chain after ap_invoice ingest: {e}"
        ))
    })?;
    let ledger = Ledger::from_connection(verify_conn, tenant, binary_hash);
    ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER ap_invoice ingest")?;

    Ok(IngestOutcome::Created { id })
}

fn validate_ingestion_input(input: &IngestionInput) -> std::result::Result<(), IngestError> {
    if input.supplier_tax_number.trim().is_empty() {
        return Err(IngestError::InvalidInput(
            "supplier_tax_number must be non-empty".into(),
        ));
    }
    if input.supplier_name.trim().is_empty() {
        return Err(IngestError::InvalidInput(
            "supplier_name must be non-empty".into(),
        ));
    }
    if input.nav_invoice_number.trim().is_empty() {
        return Err(IngestError::InvalidInput(
            "nav_invoice_number must be non-empty".into(),
        ));
    }
    if !is_canonical_iso_date(&input.issue_date) {
        return Err(IngestError::InvalidInput(format!(
            "issue_date `{}` is not a valid ISO-8601 YYYY-MM-DD date",
            input.issue_date
        )));
    }
    if let Some(d) = &input.payment_deadline {
        if !is_canonical_iso_date(d) {
            return Err(IngestError::InvalidInput(format!(
                "payment_deadline `{}` is not a valid ISO-8601 YYYY-MM-DD date",
                d
            )));
        }
    }
    if let Some(d) = &input.delivery_date {
        if !is_canonical_iso_date(d) {
            return Err(IngestError::InvalidInput(format!(
                "delivery_date `{}` is not a valid ISO-8601 YYYY-MM-DD date",
                d
            )));
        }
    }
    match input.currency.as_str() {
        "HUF" | "EUR" => {}
        other => {
            return Err(IngestError::InvalidInput(format!(
                "currency `{}` not in closed vocab (HUF | EUR)",
                other
            )));
        }
    }
    Ok(())
}

/// S186 — true when `e` is a DuckDB UNIQUE / PRIMARY KEY constraint
/// violation. The duckdb-rs crate stores DuckDB's typed error message
/// in `Error::DuckDBFailure(_, Some(msg))`; the `ErrorCode` field is
/// always `Unknown` (the crate does not translate DuckDB's typed
/// errors back into rusqlite-style codes), so message-matching is the
/// only available discriminator. DuckDB's wording is stable
/// (`Constraint Error: Duplicate key ... violates unique constraint`)
/// — covered by the test
/// `is_duplicate_key_violation_matches_duckdb_unique_message` below.
fn is_duplicate_key_violation(e: &duckdb::Error) -> bool {
    if let duckdb::Error::DuckDBFailure(_, Some(msg)) = e {
        let lower = msg.to_ascii_lowercase();
        lower.contains("duplicate key")
            || lower.contains("violates unique constraint")
            || lower.contains("violates primary key constraint")
    } else {
        false
    }
}

fn find_existing_id(
    conn: &Connection,
    tenant: &str,
    supplier_tax_number: &str,
    nav_invoice_number: &str,
) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM ap_invoice
          WHERE tenant_id = ? AND supplier_tax_number = ? AND nav_invoice_number = ?
          LIMIT 1",
    )?;
    let mut rows = stmt.query(params![tenant, supplier_tax_number, nav_invoice_number])?;
    if let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        Ok(Some(id))
    } else {
        Ok(None)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Reads.
// ──────────────────────────────────────────────────────────────────────

/// List incoming invoices for the tenant, newest issue_date first.
///
/// `status_filter` optionally narrows to one of the three closed-vocab
/// values; `None` returns every row. Pagination is offset-based per
/// the SPA list-view's existing posture (`limit` + `offset`).
pub fn list_incoming(
    db_path: &Path,
    tenant: &str,
    status_filter: Option<IncomingInvoiceStatus>,
    limit: u64,
    offset: u64,
) -> Result<Vec<IncomingInvoice>> {
    let conn = Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for ap_invoice list",
            db_path.display()
        )
    })?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;
    ensure_schema(&conn).context("ensure ap_invoice schema (list)")?;

    let mut rows = Vec::new();
    match status_filter {
        Some(status) => {
            let mut stmt = conn.prepare(
                "SELECT id, supplier_tax_number, supplier_name, supplier_address,
                        nav_invoice_number, issue_date, delivery_date, payment_deadline,
                        total_net_minor, total_vat_minor, total_gross_minor, currency,
                        local_status, irrelevant_reason, nav_xml_path, created_at, updated_at
                   FROM ap_invoice
                  WHERE tenant_id = ? AND local_status = ?
                  ORDER BY issue_date DESC, id DESC
                  LIMIT ? OFFSET ?",
            )?;
            let mut q = stmt.query(params![
                tenant,
                status.as_str(),
                limit as i64,
                offset as i64
            ])?;
            while let Some(row) = q.next()? {
                rows.push(row_to_incoming(row)?);
            }
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT id, supplier_tax_number, supplier_name, supplier_address,
                        nav_invoice_number, issue_date, delivery_date, payment_deadline,
                        total_net_minor, total_vat_minor, total_gross_minor, currency,
                        local_status, irrelevant_reason, nav_xml_path, created_at, updated_at
                   FROM ap_invoice
                  WHERE tenant_id = ?
                  ORDER BY issue_date DESC, id DESC
                  LIMIT ? OFFSET ?",
            )?;
            let mut q = stmt.query(params![tenant, limit as i64, offset as i64])?;
            while let Some(row) = q.next()? {
                rows.push(row_to_incoming(row)?);
            }
        }
    }
    Ok(rows)
}

/// Fetch one incoming invoice by id. `None` if the row does not
/// exist (the route layer maps to 404).
pub fn get_incoming(db_path: &Path, tenant: &str, id: &str) -> Result<Option<IncomingInvoice>> {
    let conn = Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for ap_invoice get",
            db_path.display()
        )
    })?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;
    ensure_schema(&conn).context("ensure ap_invoice schema (get)")?;
    let mut stmt = conn.prepare(
        "SELECT id, supplier_tax_number, supplier_name, supplier_address,
                nav_invoice_number, issue_date, delivery_date, payment_deadline,
                total_net_minor, total_vat_minor, total_gross_minor, currency,
                local_status, irrelevant_reason, nav_xml_path, created_at, updated_at
           FROM ap_invoice
          WHERE tenant_id = ? AND id = ?
          LIMIT 1",
    )?;
    let mut rows = stmt.query(params![tenant, id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row_to_incoming(row)?))
    } else {
        Ok(None)
    }
}

/// S197 — read the `nav_xml_path` column for one `ap_invoice` row.
/// Returns `Ok(None)` BOTH when the row is missing AND when the row
/// exists with `nav_xml_path` NULL — disambiguation is not needed by
/// the AP-sync follow-on-fetch caller (which has just inserted /
/// found the row by id, so absence is unreachable in practice).
pub fn get_nav_xml_path(
    db_path: &Path,
    tenant: &str,
    ap_invoice_id: &str,
) -> Result<Option<String>> {
    let conn = Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for ap_invoice nav_xml_path read",
            db_path.display()
        )
    })?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;
    ensure_schema(&conn).context("ensure ap_invoice schema (nav_xml_path read)")?;
    let mut stmt = conn.prepare(
        "SELECT nav_xml_path FROM ap_invoice
          WHERE tenant_id = ? AND id = ? LIMIT 1",
    )?;
    let mut rows = stmt.query(params![tenant, ap_invoice_id])?;
    if let Some(row) = rows.next()? {
        let v: Option<String> = row.get(0)?;
        Ok(v)
    } else {
        Ok(None)
    }
}

/// S197 — set the `nav_xml_path` column for one `ap_invoice` row. Used
/// by the AP-sync follow-on `queryInvoiceData` fetch (additive: enriches
/// a row the S178 daemon already ingested digest-first). Plain UPDATE,
/// no audit entry — the `IncomingInvoiceIngested` payload that already
/// landed at ingest time covered the row; this is operator-invisible
/// enrichment, not a state change. The row's `updated_at` bumps so
/// the SPA refresh-time sort surfaces the enrichment.
pub fn set_nav_xml_path(
    db_path: &Path,
    tenant: &str,
    ap_invoice_id: &str,
    xml_path: &str,
) -> Result<()> {
    let conn = Connection::open(db_path).with_context(|| {
        format!(
            "open tenant DuckDB at {} for ap_invoice nav_xml_path UPDATE",
            db_path.display()
        )
    })?;
    conn.execute_batch("PRAGMA disable_checkpoint_on_shutdown;")
        .context("ADR-0098 R3 (finding C): disable implicit close-checkpoint on residual opener")?;
    ensure_schema(&conn).context("ensure ap_invoice schema (nav_xml_path UPDATE)")?;
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format updated_at as Rfc3339 (nav_xml_path UPDATE)")?;
    let affected = conn.execute(
        "UPDATE ap_invoice
            SET nav_xml_path = ?, updated_at = ?
          WHERE tenant_id = ? AND id = ?",
        params![xml_path, &now, tenant, ap_invoice_id],
    )?;
    if affected == 0 {
        return Err(anyhow!(
            "set_nav_xml_path matched 0 rows for tenant={} ap_invoice_id={} \
             (caller passed an unknown id — schema or wire drift?)",
            tenant,
            ap_invoice_id
        ));
    }
    Ok(())
}

fn row_to_incoming(row: &duckdb::Row<'_>) -> Result<IncomingInvoice> {
    Ok(IncomingInvoice {
        id: row.get(0)?,
        supplier_tax_number: row.get(1)?,
        supplier_name: row.get(2)?,
        supplier_address: row.get(3)?,
        nav_invoice_number: row.get(4)?,
        issue_date: row.get(5)?,
        delivery_date: row.get(6)?,
        payment_deadline: row.get(7)?,
        total_net_minor: row.get(8)?,
        total_vat_minor: row.get(9)?,
        total_gross_minor: row.get(10)?,
        currency: row.get(11)?,
        local_status: row.get(12)?,
        irrelevant_reason: row.get(13)?,
        nav_xml_path: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
    })
}

// ──────────────────────────────────────────────────────────────────────
// Status transitions.
// ──────────────────────────────────────────────────────────────────────

/// Closed graph of allowed transitions. `Paid → Irrelevant` and
/// `Irrelevant → Paid` are NOT allowed in v1 — operator must clear to
/// `Outstanding` first. CLAUDE.md rule 7 (surface conflicts, don't
/// average them): two-step path keeps the audit chain explicit.
fn transition_allowed(from: IncomingInvoiceStatus, to: IncomingInvoiceStatus) -> bool {
    use IncomingInvoiceStatus::*;
    matches!(
        (from, to),
        (Outstanding, Paid)
            | (Outstanding, Irrelevant)
            | (Paid, Outstanding)
            | (Irrelevant, Outstanding)
    )
}

/// Apply a status change to an existing AP-side invoice row. Writes
/// the status-changed audit entry under one DuckDB transaction
/// alongside the column update. The chain is verified + the mirror is
/// synced post-commit, mirroring [`crate::mark_invoice_paid::mark_paid`].
pub fn change_status(
    db: &aberp_db::HandleArc,
    tenant: TenantId,
    binary_hash: audit_ledger::BinaryHash,
    operator_login: &str,
    ap_invoice_id: &str,
    to_status: IncomingInvoiceStatus,
    reason: Option<String>,
) -> std::result::Result<StatusChangeOutcome, StatusChangeError> {
    let trimmed_reason = reason
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if matches!(to_status, IncomingInvoiceStatus::Irrelevant) && trimmed_reason.is_none() {
        return Err(StatusChangeError::ReasonRequiredForIrrelevant);
    }

    // ADR-0098 R7 — route the status-change UPDATE + audit append through the
    // ONE shared Handle writer (`db.write()`), not an independent
    // `Connection::open` + `Ledger::open`/`sync_mirror` on the path. This is the
    // same swap-orphan / stale-head re-fork class the backfill seam carried: a
    // separate instance reads a stale head and can rewrite the mirror from its
    // own view. The `WriteGuard` drop runs the lockstep `sync_mirror`
    // post-commit hook; chain verification reuses a shared READ clone.
    let mut guard = db
        .write()
        .map_err(|e| anyhow!("shared writer for ap_invoice status change (ADR-0098 R7): {e}"))?;
    ensure_schema(&guard).context("ensure ap_invoice schema (status change)")?;
    audit_ledger::ensure_schema(&guard).context("ensure audit-ledger schema (status change)")?;

    let current = read_current_status(&guard, tenant.as_str(), ap_invoice_id)?;
    let from_status = match current {
        Some(s) => s,
        None => return Err(StatusChangeError::NotFound),
    };

    let from_parsed = IncomingInvoiceStatus::from_storage_str(&from_status)
        .context("decode ap_invoice.local_status read from DB")?;
    if !transition_allowed(from_parsed, to_status) {
        return Err(StatusChangeError::InvalidTransition {
            from: from_status,
            to: to_status.as_str().to_string(),
        });
    }

    // No-op short-circuit: if from == to, return success without
    // writing anything. The route layer can echo the unchanged row.
    if from_parsed == to_status {
        // Get the verify count for a coherent echo.
        // Release the writer BEFORE taking a read clone — `read()` locks the
        // same process-wide writer mutex, so holding `guard` would deadlock.
        drop(guard);
        let verify_conn = db.read().map_err(|e| {
            anyhow!("shared read to count entries for status no-op (ADR-0098 R7): {e}")
        })?;
        let ledger = Ledger::from_connection(verify_conn, tenant, binary_hash);
        let verified = ledger
            .verify_chain()
            .context("verify chain (status no-op)")?;
        return Ok(StatusChangeOutcome {
            id: ap_invoice_id.to_string(),
            from_status: from_parsed.as_str().to_string(),
            to_status: to_status.as_str().to_string(),
            reason: trimmed_reason,
            entries_verified: verified,
        });
    }

    let idempotency_key = IdempotencyKey::new();
    let session_id = Ulid::new().to_string();
    let actor = Actor::from_local_cli(session_id, operator_login);
    let ledger_meta = LedgerMeta::new(tenant.clone(), binary_hash);

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format updated_at as Rfc3339")?;

    let tx = guard
        .transaction()
        .context("begin DuckDB transaction (ap_invoice status change)")?;
    // Update local_status + irrelevant_reason atomically.
    let irrelevant_col_value: Option<&str> =
        if matches!(to_status, IncomingInvoiceStatus::Irrelevant) {
            trimmed_reason.as_deref()
        } else {
            None
        };
    tx.execute(
        "UPDATE ap_invoice
            SET local_status = ?, irrelevant_reason = ?, updated_at = ?
          WHERE tenant_id = ? AND id = ?",
        params![
            to_status.as_str(),
            irrelevant_col_value,
            &now,
            tenant.as_str(),
            ap_invoice_id,
        ],
    )
    .context("UPDATE ap_invoice (status change)")?;

    let payload = IncomingInvoiceStatusChangedPayload {
        ap_invoice_id: ap_invoice_id.to_string(),
        idempotency_key: idempotency_key.to_canonical_string(),
        from_status: from_parsed.as_str().to_string(),
        to_status: to_status.as_str().to_string(),
        reason: trimmed_reason.clone(),
    };
    audit_ledger::append_in_tx(
        &tx,
        &ledger_meta,
        EventKind::IncomingInvoiceStatusChanged,
        payload.to_bytes(),
        actor,
        Some(idempotency_key.to_canonical_string()),
    )
    .map_err(|e| anyhow!("audit_ledger::append_in_tx IncomingInvoiceStatusChanged: {e}"))?;
    tx.commit()
        .context("commit DuckDB transaction (ap_invoice status change)")?;

    // `guard` drops here -> the Handle's post-commit hook fires the lockstep
    // `sync_mirror` on the SHARED instance (coherent with the committed txn).
    // Chain verification reuses a read clone — never a second independent
    // `Ledger::open` (the duckdb#23046 replay / stale-head locus).
    drop(guard);
    let verify_conn = db.read().map_err(|e| {
        anyhow!("shared read to verify chain after ap_invoice status change (ADR-0098 R7): {e}")
    })?;
    let ledger = Ledger::from_connection(verify_conn, tenant, binary_hash);
    let verified = ledger
        .verify_chain()
        .context("audit-ledger chain verification failed AFTER ap_invoice status change")?;

    Ok(StatusChangeOutcome {
        id: ap_invoice_id.to_string(),
        from_status: from_parsed.as_str().to_string(),
        to_status: to_status.as_str().to_string(),
        reason: trimmed_reason,
        entries_verified: verified,
    })
}

fn read_current_status(
    conn: &Connection,
    tenant: &str,
    ap_invoice_id: &str,
) -> Result<Option<String>> {
    let mut stmt =
        conn.prepare("SELECT local_status FROM ap_invoice WHERE tenant_id = ? AND id = ? LIMIT 1")?;
    let mut rows = stmt.query(params![tenant, ap_invoice_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(row.get(0)?))
    } else {
        Ok(None)
    }
}

// ──────────────────────────────────────────────────────────────────────
// Helpers.
// ──────────────────────────────────────────────────────────────────────

/// Strict YYYY-MM-DD validator. Same posture as
/// `mark_invoice_paid::is_canonical_iso_date` — silent acceptance of
/// non-canonical date strings would lock the wrong shape into the
/// audit ledger forever.
pub(crate) fn is_canonical_iso_date(s: &str) -> bool {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    time::Date::parse(s, &format).is_ok()
}

// ADR-0098 R3 (finding C) — test shim. `ingest_incoming_invoice` now takes the
// shared `aberp_db::HandleArc` (production routes ap_sync + the manual route
// through it). Unit tests still express fixtures as a `&Path`; this shim builds
// a throwaway Handle over that path and calls the migrated fn, so each test's
// ingest exercises the real db.write() seam. Concurrency tests that need a
// SHARED writer build one Handle and clone the Arc across threads directly.
#[cfg(test)]
pub(crate) fn ingest_incoming_invoice_via_handle_for_test(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: audit_ledger::BinaryHash,
    operator_login: &str,
    ap_artifacts_dir: &Path,
    input: IngestionInput,
) -> std::result::Result<IngestOutcome, IngestError> {
    let db = aberp_db::Handle::open_default(db_path, tenant.clone())
        .map_err(|e| IngestError::Other(anyhow!("open shared Handle for test ingest: {e}")))?;
    ingest_incoming_invoice(
        &db,
        tenant,
        binary_hash,
        operator_login,
        ap_artifacts_dir,
        input,
    )
}

// ADR-0098 R7 — test shim mirroring `ingest_incoming_invoice_via_handle_for_test`.
// `change_status` now takes the shared `aberp_db::HandleArc` (production routes it
// through `state.db`). Unit tests express fixtures as a `&Path`; this shim opens a
// throwaway Handle over that path and calls the migrated fn, so each test's status
// change exercises the real `db.write()` seam (sequential open/close per call — the
// coherent reopen pattern, never two overlapping instances).
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub(crate) fn change_status_via_handle_for_test(
    db_path: &Path,
    tenant: TenantId,
    binary_hash: audit_ledger::BinaryHash,
    operator_login: &str,
    ap_invoice_id: &str,
    to_status: IncomingInvoiceStatus,
    reason: Option<String>,
) -> std::result::Result<StatusChangeOutcome, StatusChangeError> {
    let db = aberp_db::Handle::open_default(db_path, tenant.clone())
        .map_err(|e| anyhow!("open shared Handle for test status change: {e}"))?;
    change_status(
        &db,
        tenant,
        binary_hash,
        operator_login,
        ap_invoice_id,
        to_status,
        reason,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aberp_audit_ledger::BinaryHash;

    /// S410 / [[no-sql-specific]] — the
    /// `CHECK (local_status IN ('Outstanding','Paid','Irrelevant'))` DDL
    /// constraint was dropped; this pins the read-side rejection that
    /// replaced it. (The `currency` CHECK's replacement is pinned by
    /// `validate_rejects_unknown_currency` below.)
    #[test]
    fn incoming_status_from_storage_str_rejects_out_of_vocab() {
        assert!(IncomingInvoiceStatus::from_storage_str("Outstanding").is_ok());
        assert!(IncomingInvoiceStatus::from_storage_str("Paid").is_ok());
        assert!(IncomingInvoiceStatus::from_storage_str("Irrelevant").is_ok());
        // The dropped CHECK's job, now in code:
        assert!(IncomingInvoiceStatus::from_storage_str("outstanding").is_err());
        assert!(IncomingInvoiceStatus::from_storage_str("Settled").is_err());
        assert!(IncomingInvoiceStatus::from_storage_str("").is_err());
    }

    /// Per-test tempdir under the system temp root. Mirrors the
    /// pattern in `apps/aberp/tests/seller_banks_round_trip.rs` —
    /// avoids the `tempfile` dev-dep so the surface stays tight per
    /// CLAUDE.md rule 2. Each call returns a fresh unique directory;
    /// callers are responsible for cleanup at drop (deliberately
    /// best-effort — leaked tempdirs land under `/tmp` and the OS
    /// reaps them).
    struct ScopedTempDir(std::path::PathBuf);

    impl ScopedTempDir {
        fn new(label: &str) -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path =
                std::env::temp_dir().join(format!("aberp-s177-ap-{label}-{pid}-{nanos}-{seq}"));
            std::fs::create_dir_all(&path).expect("create scoped tempdir");
            Self(path)
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for ScopedTempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture_tenant() -> TenantId {
        TenantId::new("t1".to_string()).unwrap()
    }

    fn fixture_binary_hash() -> BinaryHash {
        BinaryHash::from_bytes([0u8; 32])
    }

    fn fixture_input() -> IngestionInput {
        IngestionInput {
            supplier_tax_number: "12345678".into(),
            supplier_name: "Supplier Kft.".into(),
            supplier_address: Some("1051 Budapest, Példa utca 1.".into()),
            nav_invoice_number: "SUP-2026/000001".into(),
            issue_date: "2026-05-30".into(),
            delivery_date: Some("2026-05-30".into()),
            payment_deadline: Some("2026-06-29".into()),
            total_net_minor: 100_000,
            total_vat_minor: 27_000,
            total_gross_minor: 127_000,
            currency: "HUF".into(),
            nav_xml: None,
        }
    }

    /// Closed-vocab round-trip — adding a variant requires updating
    /// both as_str + from_storage_str + this hand-listed array.
    #[test]
    fn incoming_invoice_status_round_trip_for_every_variant() {
        let variants = [
            IncomingInvoiceStatus::Outstanding,
            IncomingInvoiceStatus::Paid,
            IncomingInvoiceStatus::Irrelevant,
        ];
        for v in variants {
            let s = v.as_str();
            let parsed =
                IncomingInvoiceStatus::from_storage_str(s).unwrap_or_else(|e| panic!("{s} -> {e}"));
            assert_eq!(parsed, v);
        }
    }

    /// Unknown storage strings loud-fail per CLAUDE.md rule 12.
    #[test]
    fn incoming_invoice_status_rejects_unknown() {
        assert!(IncomingInvoiceStatus::from_storage_str("").is_err());
        assert!(IncomingInvoiceStatus::from_storage_str("paid").is_err());
        assert!(IncomingInvoiceStatus::from_storage_str("Cancelled").is_err());
    }

    /// Closed-graph transitions per the module docs.
    #[test]
    fn transition_allowed_matches_closed_graph() {
        use IncomingInvoiceStatus::*;
        // Allowed.
        assert!(transition_allowed(Outstanding, Paid));
        assert!(transition_allowed(Outstanding, Irrelevant));
        assert!(transition_allowed(Paid, Outstanding));
        assert!(transition_allowed(Irrelevant, Outstanding));
        // Forbidden.
        assert!(!transition_allowed(Paid, Irrelevant));
        assert!(!transition_allowed(Irrelevant, Paid));
        // No-op cases — `change_status` short-circuits these without
        // a write, but `transition_allowed` itself answers "is this
        // legal" — same-status moves are NOT in the allowed graph
        // (the no-op short-circuit happens before the check).
        assert!(!transition_allowed(Outstanding, Outstanding));
        assert!(!transition_allowed(Paid, Paid));
        assert!(!transition_allowed(Irrelevant, Irrelevant));
    }

    /// `IncomingInvoiceId` prefixed-ULID round-trip.
    #[test]
    fn incoming_invoice_id_round_trip() {
        let id = IncomingInvoiceId::new();
        let s = id.to_prefixed_string();
        assert!(s.starts_with("apinv_"));
        let parsed = IncomingInvoiceId::parse_prefixed(&s).unwrap();
        assert_eq!(parsed, id);
        assert!(IncomingInvoiceId::parse_prefixed("inv_X").is_err());
        assert!(IncomingInvoiceId::parse_prefixed("apinv_").is_err());
        assert!(IncomingInvoiceId::parse_prefixed("apinv_not-a-ulid").is_err());
    }

    /// Validation rejects empty supplier_tax_number.
    #[test]
    fn validate_rejects_empty_supplier_tax() {
        let mut input = fixture_input();
        input.supplier_tax_number = "   ".into();
        let result = validate_ingestion_input(&input);
        assert!(matches!(result, Err(IngestError::InvalidInput(_))));
    }

    /// Validation rejects malformed issue_date.
    #[test]
    fn validate_rejects_malformed_issue_date() {
        let mut input = fixture_input();
        input.issue_date = "2026/05/30".into();
        let result = validate_ingestion_input(&input);
        assert!(matches!(result, Err(IngestError::InvalidInput(_))));
    }

    /// Validation rejects currency outside the closed vocab.
    #[test]
    fn validate_rejects_unknown_currency() {
        let mut input = fixture_input();
        input.currency = "USD".into();
        let result = validate_ingestion_input(&input);
        assert!(matches!(result, Err(IngestError::InvalidInput(_))));
    }

    /// Ingestion is idempotent — re-ingesting the same supplier's
    /// same invoice returns AlreadyExists, NOT a duplicate row.
    #[test]
    fn ingestion_is_idempotent_on_supplier_and_invoice_number() {
        let dir = ScopedTempDir::new("idem");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        let first = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .expect("first ingest");
        let first_id = match first {
            IngestOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        };

        let second = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .expect("second ingest");
        match second {
            IngestOutcome::AlreadyExists { id } => assert_eq!(id, first_id),
            other => panic!("expected AlreadyExists, got {other:?}"),
        }

        let rows = list_incoming(&db_path, tenant.as_str(), None, 100, 0).unwrap();
        assert_eq!(rows.len(), 1, "duplicate insert was suppressed by UNIQUE");
        assert_eq!(rows[0].local_status, "Outstanding");
    }

    /// Two distinct suppliers OR two distinct invoice numbers can
    /// coexist — the uniqueness key is the pair.
    #[test]
    fn ingestion_allows_distinct_supplier_or_invoice_number() {
        let dir = ScopedTempDir::new("distinct");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .unwrap();

        let mut second = fixture_input();
        second.nav_invoice_number = "SUP-2026/000002".into();
        ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            second,
        )
        .unwrap();

        let mut third = fixture_input();
        third.supplier_tax_number = "87654321".into();
        ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            third,
        )
        .unwrap();

        let rows = list_incoming(&db_path, tenant.as_str(), None, 100, 0).unwrap();
        assert_eq!(rows.len(), 3);
    }

    /// Ingestion with raw NAV XML writes the artifact file and
    /// stamps the SHA-256 hex onto the audit payload.
    #[test]
    fn ingestion_persists_nav_xml_artifact_and_hash() {
        let dir = ScopedTempDir::new("xml-artifact");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();
        let xml_bytes = b"<InvoiceData/>".to_vec();
        let expected_hash = {
            let mut h = Sha256::new();
            h.update(&xml_bytes);
            hex::encode(h.finalize())
        };

        let mut input = fixture_input();
        input.nav_xml = Some(xml_bytes.clone());
        let outcome = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            input,
        )
        .expect("ingest with XML");
        let id = match outcome {
            IngestOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        };

        // The artifact file exists with the supplied bytes.
        let artifact_path = artifacts_dir.join(format!("{}.xml", id));
        let on_disk = std::fs::read(&artifact_path).expect("read artifact");
        assert_eq!(on_disk, xml_bytes);

        // The audit entry carries the hash.
        let ledger = Ledger::open(&db_path, tenant, bh).unwrap();
        let entries = ledger.entries().unwrap();
        let ingested = entries
            .iter()
            .find(|e| e.kind == EventKind::IncomingInvoiceIngested)
            .expect("IncomingInvoiceIngested entry");
        let payload: IncomingInvoiceIngestedPayload =
            serde_json::from_slice(&ingested.payload).unwrap();
        assert_eq!(
            payload.nav_xml_sha256.as_deref(),
            Some(expected_hash.as_str())
        );
    }

    /// Marking `Outstanding → Paid` updates the column and writes a
    /// status-changed audit entry.
    #[test]
    fn mark_paid_transitions_and_audits() {
        let dir = ScopedTempDir::new("mark-paid");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        let outcome = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .unwrap();
        let id = match outcome {
            IngestOutcome::Created { id } => id,
            other => panic!("{other:?}"),
        };

        let result = change_status_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &id,
            IncomingInvoiceStatus::Paid,
            None,
        )
        .expect("mark paid");
        assert_eq!(result.from_status, "Outstanding");
        assert_eq!(result.to_status, "Paid");

        let row = get_incoming(&db_path, tenant.as_str(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(row.local_status, "Paid");
        assert_eq!(row.irrelevant_reason, None);

        let ledger = Ledger::open(&db_path, tenant, bh).unwrap();
        let entries = ledger.entries().unwrap();
        let count = entries
            .iter()
            .filter(|e| e.kind == EventKind::IncomingInvoiceStatusChanged)
            .count();
        assert_eq!(count, 1);
    }

    /// Marking `Outstanding → Irrelevant` REQUIRES a reason.
    #[test]
    fn mark_irrelevant_requires_reason() {
        let dir = ScopedTempDir::new("mark-irrelevant");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        let outcome = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .unwrap();
        let id = match outcome {
            IngestOutcome::Created { id } => id,
            other => panic!("{other:?}"),
        };

        // Missing reason → 400-shaped error.
        let result = change_status_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &id,
            IncomingInvoiceStatus::Irrelevant,
            None,
        );
        assert!(matches!(
            result,
            Err(StatusChangeError::ReasonRequiredForIrrelevant)
        ));

        // Whitespace-only reason → same error.
        let result_ws = change_status_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &id,
            IncomingInvoiceStatus::Irrelevant,
            Some("    ".into()),
        );
        assert!(matches!(
            result_ws,
            Err(StatusChangeError::ReasonRequiredForIrrelevant)
        ));

        // Non-empty reason → succeeds.
        let result_ok = change_status_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &id,
            IncomingInvoiceStatus::Irrelevant,
            Some("duplicate of SUP-2026/000099".into()),
        )
        .expect("mark irrelevant with reason");
        assert_eq!(result_ok.to_status, "Irrelevant");
        let row = get_incoming(&db_path, tenant.as_str(), &id)
            .unwrap()
            .unwrap();
        assert_eq!(row.local_status, "Irrelevant");
        assert_eq!(
            row.irrelevant_reason.as_deref(),
            Some("duplicate of SUP-2026/000099")
        );
    }

    /// `Paid → Irrelevant` is NOT in the closed graph — route layer
    /// rejects with 400 via `InvalidTransition`.
    #[test]
    fn paid_to_irrelevant_is_rejected() {
        let dir = ScopedTempDir::new("paid-to-irrelevant");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        let outcome = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .unwrap();
        let id = match outcome {
            IngestOutcome::Created { id } => id,
            other => panic!("{other:?}"),
        };

        change_status_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &id,
            IncomingInvoiceStatus::Paid,
            None,
        )
        .unwrap();

        let result = change_status_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &id,
            IncomingInvoiceStatus::Irrelevant,
            Some("oops, irrelevant".into()),
        );
        assert!(matches!(
            result,
            Err(StatusChangeError::InvalidTransition { .. })
        ));
    }

    /// Unknown id → NotFound.
    #[test]
    fn unknown_id_returns_not_found() {
        let dir = ScopedTempDir::new("unknown-id");
        let db_path = dir.path().join("tenant.duckdb");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        // ensure schema before the lookup.
        {
            let conn = Connection::open(&db_path).unwrap();
            ensure_schema(&conn).unwrap();
            audit_ledger::ensure_schema(&conn).unwrap();
        }
        let result = change_status_via_handle_for_test(
            &db_path,
            tenant,
            bh,
            "operator",
            "apinv_01HRQXYZABCDEFGHJKMNPQRST",
            IncomingInvoiceStatus::Paid,
            None,
        );
        assert!(matches!(result, Err(StatusChangeError::NotFound)));
    }

    /// Schema migration is idempotent — calling `ensure_schema`
    /// twice on the same connection MUST NOT error.
    #[test]
    fn ensure_schema_is_idempotent() {
        let dir = ScopedTempDir::new("schema-idem");
        let db_path = dir.path().join("tenant.duckdb");
        let conn = Connection::open(&db_path).unwrap();
        ensure_schema(&conn).expect("first ensure");
        ensure_schema(&conn).expect("second ensure (idempotent)");
        // Sanity: the table exists and is empty.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM ap_invoice", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    /// S186 — the message-pattern discriminator catches the actual
    /// wording DuckDB emits for a UNIQUE-constraint violation, and
    /// does NOT match unrelated errors. Pinning the contract here so
    /// a future duckdb-rs upgrade that re-words the message surfaces
    /// at test time, not as a silent regression to "500 on every
    /// race-loser" production behaviour.
    #[test]
    fn is_duplicate_key_violation_matches_duckdb_unique_message() {
        let dir = ScopedTempDir::new("dup-key");
        let db_path = dir.path().join("tenant.duckdb");
        let conn = Connection::open(&db_path).unwrap();
        ensure_schema(&conn).unwrap();
        // First insert succeeds.
        conn.execute(
            "INSERT INTO ap_invoice (
                id, tenant_id, supplier_tax_number, supplier_name, supplier_address,
                nav_invoice_number, issue_date, delivery_date, payment_deadline,
                total_net_minor, total_vat_minor, total_gross_minor, currency,
                local_status, irrelevant_reason, nav_xml_path, created_at, updated_at
             ) VALUES ('apinv_x', 't1', '12345678', 'S', NULL, 'N',
                       '2026-05-30', NULL, NULL,
                       0, 0, 0, 'HUF',
                       'Outstanding', NULL, NULL, '2026-05-30T00:00:00Z',
                       '2026-05-30T00:00:00Z');",
            [],
        )
        .unwrap();
        // Second insert with same (tenant, supplier_tax, invoice_number)
        // (but different id) MUST trip the UNIQUE constraint.
        let err = conn
            .execute(
                "INSERT INTO ap_invoice (
                id, tenant_id, supplier_tax_number, supplier_name, supplier_address,
                nav_invoice_number, issue_date, delivery_date, payment_deadline,
                total_net_minor, total_vat_minor, total_gross_minor, currency,
                local_status, irrelevant_reason, nav_xml_path, created_at, updated_at
             ) VALUES ('apinv_y', 't1', '12345678', 'S', NULL, 'N',
                       '2026-05-30', NULL, NULL,
                       0, 0, 0, 'HUF',
                       'Outstanding', NULL, NULL, '2026-05-30T00:00:00Z',
                       '2026-05-30T00:00:00Z');",
                [],
            )
            .expect_err("second insert must violate UNIQUE");
        assert!(
            is_duplicate_key_violation(&err),
            "duckdb UNIQUE-violation message must match the helper: got {err:?}"
        );

        // Negative: a non-constraint error (e.g., syntax error) must
        // NOT match. Using a malformed SQL fragment so the failure
        // mode is unambiguous.
        let other = conn
            .execute("THIS IS NOT VALID SQL", [])
            .expect_err("syntax error");
        assert!(
            !is_duplicate_key_violation(&other),
            "non-constraint error must NOT match the helper: got {other:?}"
        );
    }

    /// S410 / [[no-sql-specific]] — the APP LAYER is the authoritative
    /// dedup gate, NOT the DB's `UNIQUE`. This test asserts that
    /// `ingest_incoming_invoice` dedups a repeated
    /// `(tenant, supplier_tax, nav_invoice_number)` to exactly one row
    /// **even though** the raw DB `UNIQUE` provably does not fire across
    /// two `Connection::open` handles in the same process.
    ///
    /// Part 1 establishes the rationale (the DB cannot be trusted as the
    /// gate): two manual connections both INSERT the same key and BOTH
    /// succeed. Part 2 asserts the behaviour that replaced reliance on
    /// that constraint: the real ingest path (`find_existing_id` probe
    /// under [`INGEST_SERIALIZER`]) returns `AlreadyExists` and leaves a
    /// single row — engine-independent, exactly what
    /// [[no-sql-specific]] requires.
    ///
    /// Pre-S410 this test merely *documented the quirk* (asserted the
    /// raw double-INSERT succeeds). It now asserts the app-layer gate,
    /// so it fails if dedup ever silently starts depending on the engine.
    #[test]
    fn app_layer_dedup_is_authoritative_even_though_db_unique_does_not_fire() {
        // ── Part 1: the DB UNIQUE does NOT fire cross-connection. ──
        // (This is *why* the app layer must be authoritative; it is a
        // demonstration of the engine's limitation, not the dedup gate.)
        let raw_dir = ScopedTempDir::new("dup-key-cross-conn-raw");
        let raw_db = raw_dir.path().join("tenant.duckdb");
        {
            let conn = Connection::open(&raw_db).unwrap();
            ensure_schema(&conn).unwrap();
        }
        let conn1 = Connection::open(&raw_db).unwrap();
        let conn2 = Connection::open(&raw_db).unwrap();
        let insert_sql = "INSERT INTO ap_invoice (
                id, tenant_id, supplier_tax_number, supplier_name, supplier_address,
                nav_invoice_number, issue_date, delivery_date, payment_deadline,
                total_net_minor, total_vat_minor, total_gross_minor, currency,
                local_status, irrelevant_reason, nav_xml_path, created_at, updated_at
             ) VALUES (?, 't1', '12345678', 'S', NULL, 'N',
                       '2026-05-30', NULL, NULL,
                       0, 0, 0, 'HUF',
                       'Outstanding', NULL, NULL, '2026-05-30T00:00:00Z',
                       '2026-05-30T00:00:00Z');";
        conn1
            .execute(insert_sql, duckdb::params!["apinv_first"])
            .expect("first raw insert must succeed");
        let second_raw = conn2.execute(insert_sql, duckdb::params!["apinv_second"]);
        assert!(
            second_raw.is_ok(),
            "precondition: DuckDB UNIQUE does not fire cross-connection, so the \
             app layer must be the gate; if this flips, the app-layer assertion \
             below is what still guarantees correctness"
        );

        // ── Part 2: the app-layer ingest path IS authoritative. ──
        let dir = ScopedTempDir::new("dup-key-app-layer");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        let first = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "op",
            &artifacts_dir,
            fixture_input(),
        )
        .expect("first ingest");
        let first_id = match first {
            IngestOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        };

        // A SECOND ingest of the SAME key (a fresh `Connection::open`
        // inside the call — the very scenario Part 1 proves the DB does
        // not guard) must dedup via the probe, NOT insert a duplicate.
        let second = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "op",
            &artifacts_dir,
            fixture_input(),
        )
        .expect("second ingest");
        match second {
            IngestOutcome::AlreadyExists { id } => assert_eq!(
                id, first_id,
                "app-layer dedup must echo the surviving row id"
            ),
            other => panic!("expected AlreadyExists (app-layer gate), got {other:?}"),
        }

        let rows = list_incoming(&db_path, tenant.as_str(), None, 100, 0).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "app-layer probe must keep exactly one row despite the DB UNIQUE not firing"
        );
    }

    /// S186 — `ingest_incoming_invoice` is robust under concurrent
    /// calls with the SAME `(tenant, supplier_tax, nav_invoice_number)`
    /// triple. The [`INGEST_SERIALIZER`] mutex serializes the
    /// critical section across threads in the process; the contract
    /// pinned here is the operator-visible outcome:
    ///
    ///   - Contract A: no caller errors (pre-S186 a concurrent
    ///     second caller hit an audit-chain "tamper detected"
    ///     verification error from racing audit-ledger writers,
    ///     surfacing as 500).
    ///   - Contract B: exactly ONE row exists in the table — the
    ///     serializer turns concurrent ingests into a winner +
    ///     `AlreadyExists` echoes.
    ///   - Contract C: every caller's returned id equals the
    ///     surviving row's id — no caller ever points an operator
    ///     at a row that does not exist.
    #[test]
    fn concurrent_ingest_holds_no_error_one_row_id_consistent() {
        let dir = ScopedTempDir::new("concurrent-ingest");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        // Pre-create schema so the threads don't race on
        // CREATE TABLE / boot-time setup (which would mask the
        // race-of-interest with a different race).
        {
            let conn = Connection::open(&db_path).unwrap();
            ensure_schema(&conn).unwrap();
            audit_ledger::ensure_schema(&conn).unwrap();
        }

        // ADR-0098 R3 (finding C) — ONE shared Handle cloned (Arc) across the
        // four threads: the migrated ingest routes through db.write(), so the
        // race-of-interest is now the shared serialized writer + INGEST_SERIALIZER
        // dedup gate (not four independent Connection::open handles). The
        // process-wide INGEST_SERIALIZER still guarantees at-most-one row.
        let handle = aberp_db::Handle::open_default(&db_path, tenant.clone())
            .expect("open shared Handle for concurrency test");
        // Four threads to up the race-trigger probability across
        // schedulings.
        let mut handles = Vec::new();
        for i in 0..4 {
            let db = handle.clone();
            let art = artifacts_dir.clone();
            let t = tenant.clone();
            let operator = format!("operator-{i}");
            handles.push(std::thread::spawn(move || {
                ingest_incoming_invoice(&db, t, bh, &operator, &art, fixture_input())
            }));
        }
        let outcomes: Vec<std::result::Result<IngestOutcome, IngestError>> = handles
            .into_iter()
            .map(|h| h.join().expect("thread panicked"))
            .collect();

        // Contract A: no caller errors.
        for (i, r) in outcomes.iter().enumerate() {
            assert!(
                r.is_ok(),
                "thread {i} must not error under concurrent ingest: {:?}",
                r.as_ref().err()
            );
        }

        // Contract B: exactly one row exists.
        let rows = list_incoming(&db_path, tenant.as_str(), None, 100, 0).unwrap();
        assert_eq!(
            rows.len(),
            1,
            "exactly one ap_invoice row must exist after concurrent ingest \
             (race-recovery + UNIQUE constraint)"
        );
        let canonical_id = rows[0].id.clone();

        // Contract C: every caller's returned id matches the
        // surviving row.
        for (i, r) in outcomes.iter().enumerate() {
            let id = match r.as_ref().unwrap() {
                IngestOutcome::Created { id } => id,
                IngestOutcome::AlreadyExists { id } => id,
            };
            assert_eq!(
                id, &canonical_id,
                "thread {i}'s outcome id must equal the surviving row id"
            );
        }
    }

    /// list_incoming filters by status.
    #[test]
    fn list_filters_by_status() {
        let dir = ScopedTempDir::new("list-filter");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        let outcome_a = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .unwrap();
        let id_a = match outcome_a {
            IngestOutcome::Created { id } => id,
            other => panic!("{other:?}"),
        };
        let mut second = fixture_input();
        second.nav_invoice_number = "SUP-2026/000002".into();
        ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            second,
        )
        .unwrap();

        change_status_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &id_a,
            IncomingInvoiceStatus::Paid,
            None,
        )
        .unwrap();

        let outstanding = list_incoming(
            &db_path,
            tenant.as_str(),
            Some(IncomingInvoiceStatus::Outstanding),
            100,
            0,
        )
        .unwrap();
        assert_eq!(outstanding.len(), 1);
        assert_eq!(outstanding[0].local_status, "Outstanding");

        let paid = list_incoming(
            &db_path,
            tenant.as_str(),
            Some(IncomingInvoiceStatus::Paid),
            100,
            0,
        )
        .unwrap();
        assert_eq!(paid.len(), 1);
        assert_eq!(paid[0].local_status, "Paid");
    }

    /// S197 — fresh ingest carries NULL `nav_xml_path` (the S178 daemon
    /// ingests digest-first); the S197 follow-on `queryInvoiceData`
    /// fetch reads via [`get_nav_xml_path`] to decide whether to fetch,
    /// then writes the on-disk path via [`set_nav_xml_path`].
    #[test]
    fn nav_xml_path_helpers_round_trip() {
        let dir = ScopedTempDir::new("nav-xml-path");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        let outcome = ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            fixture_input(),
        )
        .unwrap();
        let id = match outcome {
            IngestOutcome::Created { id } => id,
            other => panic!("expected Created, got {other:?}"),
        };

        // Fresh ingest with `nav_xml: None` writes NULL.
        let before = get_nav_xml_path(&db_path, tenant.as_str(), &id).unwrap();
        assert_eq!(before, None);

        // Set a path; readback matches.
        set_nav_xml_path(&db_path, tenant.as_str(), &id, "/tmp/example.xml").unwrap();
        let after = get_nav_xml_path(&db_path, tenant.as_str(), &id).unwrap();
        assert_eq!(after.as_deref(), Some("/tmp/example.xml"));
    }

    /// S197 — `set_nav_xml_path` against an unknown id loud-fails per
    /// CLAUDE.md rule 12. A silent 0-row UPDATE would mask wire / schema
    /// drift on the follow-on-fetch path.
    #[test]
    fn set_nav_xml_path_loud_fails_on_unknown_id() {
        let dir = ScopedTempDir::new("nav-xml-path-unknown");
        let db_path = dir.path().join("tenant.duckdb");
        let tenant = fixture_tenant();
        // Pre-create schema (no row).
        {
            let conn = Connection::open(&db_path).unwrap();
            ensure_schema(&conn).unwrap();
        }
        let err = set_nav_xml_path(
            &db_path,
            tenant.as_str(),
            "apinv_01HRQXYZABCDEFGHJKMNPQRST",
            "/tmp/x.xml",
        )
        .expect_err("must loud-fail on unknown id");
        assert!(format!("{err:#}").contains("matched 0 rows"));
    }

    /// PR-214 / S216 — `list_incoming` MUST return rows whose
    /// `nav_xml_path` column is NULL. The S197 daemon writes that
    /// column lazily (one queryInvoiceData fetch per row); a digest-
    /// only row is the lifecycle-natural state for every freshly-
    /// ingested entry. Pins against a future contributor who adds a
    /// `WHERE nav_xml_path IS NOT NULL` gate (which would silently
    /// hide every row until its XML fetch completes — a class of
    /// regression that hit prod's PR-214 / S216 brief). This is the
    /// non-bug pin for what the PR-214 brief termed "Symptom A".
    #[test]
    fn list_incoming_returns_rows_with_null_nav_xml_path() {
        let dir = ScopedTempDir::new("null-xml-path");
        let db_path = dir.path().join("tenant.duckdb");
        let artifacts_dir = dir.path().join("ap-artifacts");
        let tenant = fixture_tenant();
        let bh = fixture_binary_hash();

        // Ingest two rows: neither carries nav_xml (so nav_xml_path
        // stays NULL on both).
        let mut input_a = fixture_input();
        input_a.nav_invoice_number = "SUP-2026/A".into();
        ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            input_a,
        )
        .expect("ingest A");
        let mut input_b = fixture_input();
        input_b.nav_invoice_number = "SUP-2026/B".into();
        ingest_incoming_invoice_via_handle_for_test(
            &db_path,
            tenant.clone(),
            bh,
            "operator",
            &artifacts_dir,
            input_b,
        )
        .expect("ingest B");

        // The list must return BOTH rows even though their
        // nav_xml_path is NULL — the SPA's IncomingInvoiceList renders
        // them with an "XML pending" affordance, NOT a hidden gate.
        let rows = list_incoming(&db_path, tenant.as_str(), None, 100, 0).unwrap();
        assert_eq!(rows.len(), 2, "both NULL-xml-path rows must surface");
        for row in &rows {
            assert!(
                row.nav_xml_path.is_none(),
                "fixture row must keep NULL nav_xml_path; got: {:?}",
                row.nav_xml_path
            );
        }
    }
}
