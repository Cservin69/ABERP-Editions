//! Typed validation errors per ADR-0022 §"thiserror discipline."
//!
//! Each variant names one failure class the validator can surface.
//! Variants are pairwise-distinct in their `Display` text so a future
//! merge that accidentally collapses two cases into one produces a
//! conflict at the variant declaration AND at the
//! `error_variants_have_distinct_display` test in `validate.rs`.

use thiserror::Error;

/// Failure classes the hand-rolled v3.0 validator can surface.
///
/// Variant order is deliberate — most-likely failures first
/// (root / namespace), then structural (missing / unexpected element),
/// then field shape (numeric / date). A future variant added by an
/// emitter extension should slot in by failure class, not at the end.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum NavXsdValidationError {
    /// The XML failed to parse at the byte level (truncated, malformed
    /// tag, encoding mismatch). Carries the underlying quick-xml error
    /// description; the byte position is included by quick-xml's own
    /// formatting.
    #[error("malformed XML at byte {position}: {message}")]
    MalformedXml { position: usize, message: String },

    /// The root element is not `<InvoiceData>` — wrong document type
    /// entirely.
    #[error("root element must be <InvoiceData>, got <{actual}>")]
    UnexpectedRoot { actual: String },

    /// The root element's `xmlns` does not match NAV v3.0 data namespace.
    /// Loud-fail because a wrong namespace means we are validating
    /// something that is not a NAV v3.0 InvoiceData regardless of how
    /// the element names look.
    #[error("root namespace must be {expected}, got {actual:?}")]
    UnexpectedRootNamespace {
        expected: &'static str,
        actual: Option<String>,
    },

    /// A required child element is missing inside `parent`.
    /// `expected` is the missing element's local name.
    #[error("missing required child <{expected}> inside <{parent}>")]
    MissingRequiredChild {
        parent: &'static str,
        expected: &'static str,
    },

    /// An element appeared that the v3.0 allowlist for this parent
    /// does not recognise. NAV's schema rejects unknown elements; the
    /// validator does so first.
    #[error("unexpected element <{element}> inside <{parent}> (not in NAV v3.0 allowlist)")]
    UnexpectedElement {
        parent: &'static str,
        element: String,
    },

    /// Child elements appear in the wrong order for v3.0. NAV uses
    /// `xs:sequence` extensively; out-of-order children break the
    /// schema. `expected_before` is the element that should precede
    /// `actually_appeared_first`.
    #[error(
        "child order violation inside <{parent}>: <{expected_before}> must precede <{actually_appeared_first}>"
    )]
    ChildOrderViolation {
        parent: &'static str,
        expected_before: &'static str,
        actually_appeared_first: String,
    },

    /// A required cardinality `1..=N` was violated by either zero
    /// occurrences (caught as `MissingRequiredChild`) or more than the
    /// max. This variant fires for the "more than max" case — e.g.,
    /// `<invoiceSummary>` appears twice.
    #[error(
        "cardinality violation: <{element}> may appear at most {max} time(s) inside <{parent}>, saw {actual}"
    )]
    CardinalityExceeded {
        parent: &'static str,
        element: &'static str,
        max: u32,
        actual: u32,
    },

    /// `<invoiceIssueDate>` (or another `xs:date`-shaped field) does
    /// not match `YYYY-MM-DD` ASCII. NAV v3.0 narrows `xs:date` to this
    /// shape; surfacing here keeps the failure off the wire.
    #[error("malformed date in <{field}>: expected YYYY-MM-DD, got {actual:?}")]
    MalformedDate { field: &'static str, actual: String },

    /// A numeric-amount field's text is not pure ASCII digits with at
    /// most one optional decimal point. NAV v3.0 rejects scientific
    /// notation, signs, locale separators.
    #[error("non-numeric content in <{field}>: {actual:?}")]
    NonNumericAmount { field: &'static str, actual: String },

    /// `<invoiceLines>` has zero `<line>` children. NAV v3.0 requires
    /// at least one. Distinct from `MissingRequiredChild` because the
    /// schema-level cardinality here is `1..N`, not `1`.
    #[error("<invoiceLines> requires at least one <line>, found none")]
    NoInvoiceLines,

    /// PR-66 / session-87 — a structured child of `<supplierTaxNumber>`
    /// or `<customerTaxNumber>` carried the wrong (or missing)
    /// namespace prefix. NAV v3.0 places `taxpayerId` / `vatCode` /
    /// `countyCode` in the `base` namespace; the canonical wire shape
    /// is the `common:` prefix bound to that namespace at the root
    /// (see `apps/aberp/src/nav_xml.rs::common_element`).
    ///
    /// Distinct from `UnexpectedElement` because the LOCAL name IS in
    /// the allowlist — only the prefix is wrong. Pre-PR-66 the
    /// validator was prefix-blind (via `local_name_of`) so a future
    /// emit regression that dropped the prefix would slip past the
    /// invariant check and surface only as a NAV-side rejection.
    /// Session 87 surfaced the symmetric regression in
    /// `parse_supplier_tax_number_from_xml`'s substring scan; this
    /// variant closes the emit-side mirror so the v3.0 invariant
    /// check is now load-bearing against both directions.
    ///
    /// `actual_prefix` is the empty string when the child was written
    /// bare (no prefix at all — would inherit the default `data`
    /// namespace from the root, which is semantically wrong for these
    /// elements).
    #[error(
        "namespace-prefix mismatch on <{actual_prefix}:{element}> inside <{parent}>: \
         NAV v3.0 requires the `{expected_prefix}:` prefix (base namespace)"
    )]
    WrongChildNamespacePrefix {
        parent: &'static str,
        element: &'static str,
        expected_prefix: &'static str,
        actual_prefix: String,
    },
}
