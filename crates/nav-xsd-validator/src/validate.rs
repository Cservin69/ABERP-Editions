//! The structural walker that turns an `<InvoiceData>` byte slice into
//! either `Ok(())` or a typed [`NavXsdValidationError`].
//!
//! # Design notes
//!
//! The walker is event-driven (quick-xml `Reader::read_event`). It does
//! NOT build an in-memory tree — the v3.0 InvoiceData payload is small
//! and the validation rules are local (parent-child ordering and
//! cardinality), so a streaming state machine keeps the allocator out
//! of the hot path and makes the rule set explicit.
//!
//! # Allowlist source of truth
//!
//! Every element the validator accepts is named here as a `&'static
//! str`. If `apps/aberp/src/nav_xml.rs` writes an element name not
//! listed here, the validator rejects it loudly per ADR-0022 §"What
//! 'invariant check' covers." Conversely, if an element is named here
//! but the emitter never writes it (and no NAV-side response would
//! either), it is dead allowlist surface and a future cleanup PR
//! should prune it — but the bias is toward over-accepting on the
//! validator side so we never silently reject something NAV's own
//! parser would accept.
//!
//! # ADR-0022 conformance check
//!
//! The unit test `error_variants_have_distinct_display` at the bottom
//! of this file walks every [`NavXsdValidationError`] variant and
//! asserts pairwise-distinct `Display`. ADR-0022's adversarial-review
//! bullet 4 names this requirement.
//!
//! # Why `Reader::from_str` instead of `read_event_into`
//!
//! `Reader::from_str(s)` returns a `Reader<&[u8]>` whose `read_event`
//! returns `Event<'a>` borrowing from the SOURCE string — no per-call
//! buffer is needed and no buffer parameter has to plumb through
//! recursive walkers. The alternative (`read_event_into(&mut buf)`)
//! returns events borrowed from the supplied buffer, which means each
//! walk function would have to take and re-borrow the same `Vec<u8>`
//! across loop iterations — exactly the lifetime nightmare we ran
//! into the first time around.

use quick_xml::events::Event;
use quick_xml::name::QName;
use quick_xml::Reader;

use crate::error::NavXsdValidationError;
use crate::{NAV_NS_ANNUL, NAV_NS_DATA};

/// Validate that `xml` is a v3.0 `<InvoiceData>` payload structurally
/// acceptable to NAV. Returns `Ok(())` on success; on any divergence
/// returns a typed [`NavXsdValidationError`].
///
/// The call is single-pass over the input bytes; no in-memory tree is
/// built. The input must be valid UTF-8 (NAV v3.0 InvoiceData is
/// UTF-8 by spec).
pub fn validate_invoice_data(xml: &[u8]) -> Result<(), NavXsdValidationError> {
    let s = std::str::from_utf8(xml).map_err(|e| NavXsdValidationError::MalformedXml {
        position: e.valid_up_to(),
        message: format!("input is not valid UTF-8: {e}"),
    })?;
    let mut reader = Reader::from_str(s);
    reader.config_mut().trim_text(true);

    // Walk to the root element tag (skip XML declaration and any
    // whitespace-only events).
    //
    // `Event::Start` matches `<Foo>...</Foo>`; `Event::Empty` matches
    // the self-closing `<Foo/>`. Both produce a `BytesStart` carrying
    // the element name + attributes, so the validator must accept
    // either at the root position. The two negative-path tests
    // `wrong_root_element_is_loud_fail` and
    // `wrong_root_namespace_is_loud_fail` exercise the self-closing
    // form deliberately — they want to assert the root-shape check
    // fires regardless of whether the operator wrote a self-closing
    // or a paired tag.
    let root = loop {
        match read_event(&mut reader)? {
            Event::Start(e) | Event::Empty(e) => break e,
            Event::Eof => {
                return Err(NavXsdValidationError::MalformedXml {
                    position: reader.buffer_position() as usize,
                    message: "document ended before any element".into(),
                });
            }
            _ => continue,
        }
    };

    // Root must be <InvoiceData> at the NAV v3.0 data namespace.
    let root_local = local_name_of(root.name());
    if root_local != "InvoiceData" {
        return Err(NavXsdValidationError::UnexpectedRoot {
            actual: root_local.to_string(),
        });
    }
    let xmlns = extract_xmlns(&root)?;
    if xmlns.as_deref() != Some(NAV_NS_DATA) {
        return Err(NavXsdValidationError::UnexpectedRootNamespace {
            expected: NAV_NS_DATA,
            actual: xmlns,
        });
    }

    walk_invoice_data(&mut reader)
}

// ── Per-parent walkers ───────────────────────────────────────────────

fn walk_invoice_data(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "InvoiceData";
    const ALLOWED: &[&str] = &[
        "invoiceNumber",
        "invoiceIssueDate",
        "completenessIndicator",
        "invoiceMain",
    ];
    // PR-76 — `completenessIndicator` was pre-PR-76 listed in ALLOWED but
    // NOT ORDERED_REQUIRED. NAV-test rejected invoice 17
    // (`inv_01KSM8SRH3X2WQ2TPBHGF8QQBX`) with
    // `SCHEMA_VIOLATION: One of '{...:completenessIndicator}' is
    // expected` because the emitter never wrote it. The validator missed
    // the rule for the same reason (rule absence ≠ optional element).
    // The element is now required between `<invoiceIssueDate>` and
    // `<invoiceMain>`, matching the NAV v3.0 InvoiceData XSD.
    const ORDERED_REQUIRED: &[&str] = &[
        "invoiceNumber",
        "invoiceIssueDate",
        "completenessIndicator",
        "invoiceMain",
    ];

    let mut seen_in_order: Vec<&'static str> = Vec::new();

    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "invoiceNumber" => {
                        let _ = collect_text(reader, "invoiceNumber")?;
                    }
                    "invoiceIssueDate" => {
                        let text = collect_text(reader, "invoiceIssueDate")?;
                        ensure_date_shape("invoiceIssueDate", &text)?;
                    }
                    "completenessIndicator" => {
                        let _ = collect_text(reader, "completenessIndicator")?;
                    }
                    "invoiceMain" => {
                        walk_invoice_main(reader)?;
                    }
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen_in_order.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen_in_order)?;
                return Ok(());
            }
            Event::Eof => {
                return Err(NavXsdValidationError::MalformedXml {
                    position: reader.buffer_position() as usize,
                    message: "document ended inside <InvoiceData>".into(),
                });
            }
            _ => {}
        }
    }
}

fn walk_invoice_main(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "invoiceMain";
    const ALLOWED: &[&str] = &["invoice"];
    expect_single_child_then_close(reader, PARENT, ALLOWED, walk_invoice)
}

fn walk_invoice(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "invoice";
    // `invoiceReference` is optional and present only on STORNO / MODIFY
    // chain invoices (ADR-0023). NAV v3.0 schema positions it BEFORE
    // `invoiceHead`; the ABERP emitter (`apps/aberp/src/nav_xml.rs`
    // ::render_storno_data) writes it there. The validator does NOT
    // enforce that position because the projection-based ordered-
    // required check only cares about the position of REQUIRED
    // children — surfaced explicitly here so a future tightening is a
    // deliberate decision, not a silent regression.
    const ALLOWED: &[&str] = &[
        "invoiceReference",
        "invoiceHead",
        "invoiceLines",
        "invoiceSummary",
    ];
    const ORDERED_REQUIRED: &[&str] = &["invoiceHead", "invoiceLines", "invoiceSummary"];

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "invoiceReference" => walk_invoice_reference(reader)?,
                    "invoiceHead" => walk_invoice_head(reader)?,
                    "invoiceLines" => walk_invoice_lines(reader)?,
                    "invoiceSummary" => walk_invoice_summary(reader)?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in("invoice", reader)),
            _ => {}
        }
    }
}

/// `<invoiceReference>` chain-link block — PR-10, ADR-0023; PR-11,
/// ADR-0024. Present only on STORNO / MODIFY chain invoices. The
/// `InvoiceReferenceType` has EXACTLY three children
/// (`originalInvoiceNumber`, `modifyWithoutMaster`, `modificationIndex`),
/// identical on both STORNO and MODIFY bodies. The ABERP emitter writes
/// `modifyWithoutMaster=false` always (the migrated-from-Billingo `true`
/// path is deferred per ADR-0023 §4); the validator does not constrain
/// the value here.
///
/// S381/F1 — the PR-11 `modificationIssueDate` allowance was REMOVED.
/// That element existed only in NAV v2.0; v3.0's `InvoiceReferenceType`
/// has no such child, so a body carrying it is schema-invalid and must
/// be rejected (`UnexpectedElement`) rather than allowlisted. STORNO and
/// MODIFY bodies are now byte-identical here; the wire operation is
/// declared on the SOAP envelope (derived from the audit ledger by
/// `submission_queue::operation_for_invoice`), not from the body shape.
fn walk_invoice_reference(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "invoiceReference";
    // S381/F1 — exactly the three v3.0 `InvoiceReferenceType` children;
    // `modificationIssueDate` (v2.0-only) is intentionally absent so a
    // body still carrying it fails as `UnexpectedElement`.
    const ALLOWED: &[&str] = &[
        "originalInvoiceNumber",
        "modifyWithoutMaster",
        "modificationIndex",
    ];
    const ORDERED_REQUIRED: &[&str] = &[
        "originalInvoiceNumber",
        "modifyWithoutMaster",
        "modificationIndex",
    ];

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "originalInvoiceNumber" => {
                        let _ = collect_text(reader, "originalInvoiceNumber")?;
                    }
                    "modifyWithoutMaster" => {
                        let _ = collect_text(reader, "modifyWithoutMaster")?;
                    }
                    "modificationIndex" => {
                        let _ = collect_text(reader, "modificationIndex")?;
                    }
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_invoice_head(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "invoiceHead";
    const ALLOWED: &[&str] = &["supplierInfo", "customerInfo", "invoiceDetail"];
    const ORDERED_REQUIRED: &[&str] = &["supplierInfo", "customerInfo", "invoiceDetail"];

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "supplierInfo" => walk_supplier_info(reader)?,
                    "customerInfo" => walk_customer_info(reader)?,
                    "invoiceDetail" => walk_invoice_detail(reader)?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_supplier_info(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "supplierInfo";
    const ALLOWED: &[&str] = &["supplierTaxNumber", "supplierName", "supplierAddress"];
    const ORDERED_REQUIRED: &[&str] = ALLOWED;

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "supplierTaxNumber" => walk_supplier_tax_number(reader)?,
                    "supplierName" => {
                        let _ = collect_text(reader, canonical)?;
                    }
                    "supplierAddress" => walk_address("supplierAddress", reader)?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

/// PR-50 / session-70 — walk the structured `<supplierTaxNumber>`
/// children per NAV `Online Számla` v3.0. Mirror of
/// [`walk_customer_tax_number`] with the three Hungarian-specific
/// supplier sub-elements (taxpayerId + vatCode + countyCode all
/// required for a domestic supplier). The pre-PR-50 validator
/// accepted a flat string here — that shape ships clean past this
/// gate but NAV's submit endpoint loud-fails it, so the gate now
/// matches the wire reality.
///
/// PR-66 / session-87 — additionally asserts the `common:` namespace
/// prefix on each of the three structured children. NAV v3.0 places
/// these in the `base` namespace (declared as `xmlns:common` at the
/// `<InvoiceData>` root); a bare-prefix child would inherit the
/// default `data` namespace which is semantically wrong. The
/// emitter (`nav_xml::common_element`) always writes the prefix, so
/// a violation here means an upstream regression dropped it. Loud-
/// fail per CLAUDE.md rule 12.
fn walk_supplier_tax_number(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "supplierTaxNumber";
    const ALLOWED: &[&str] = &["taxpayerId", "vatCode", "countyCode"];
    const ORDERED_REQUIRED: &[&str] = ALLOWED;
    walk_structured_tax_number(reader, PARENT, ALLOWED, ORDERED_REQUIRED)
}

fn walk_customer_info(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "customerInfo";
    // PR-77 / session-101 — `customerAddress` added. NAV v3.0
    // CustomerInfoType permits it at the optional position AFTER
    // `<customerName>`; the business rule below promotes it to
    // required when `customerVatStatus` is non-PRIVATE_PERSON.
    const ALLOWED: &[&str] = &[
        "customerVatStatus",
        "customerVatData",
        "customerName",
        "customerAddress",
    ];
    // PR-97 / ADR-0048 (Ervin override 2 / GDPR) — `customerVatStatus`
    // is the ONLY unconditionally-required child. `customerName` +
    // `customerAddress` are conditional: REQUIRED when status !=
    // PRIVATE_PERSON (the PR-77 `CUSTOMER_DATA_EXPECTED` positive half),
    // FORBIDDEN under PRIVATE_PERSON. `customerVatData` is likewise
    // conditionally required for non-PRIVATE_PERSON and forbidden under
    // PRIVATE_PERSON.
    //
    // Session-154 (ADR-0048 amendment 2026-05-29) resolved the PR-97
    // open question. The NAV-test endpoint did NOT promote
    // `<customerName>` back to required under PRIVATE_PERSON — it
    // FORBADE it: invoice 31 (2026-05-29) ABORTED with business rule
    // `CUSTOMER_DATA_NOT_EXPECTED` ("Magánszemély vevő adatai nem adhatók
    // meg.") because the wire body carried `<customerName>` +
    // `<customerAddress>` under PRIVATE_PERSON. §169's buyer-name/address
    // mandate governs the printed PDF (unchanged), NOT the NAV wire. The
    // post-walk branch below enforces both the positive (non-PP requires)
    // and negative (PP forbids) halves.
    const ORDERED_REQUIRED: &[&str] = &["customerVatStatus"];

    // PR-77 / session-101 — capture the customerVatStatus text so the
    // post-walk business-rule check can decide whether `customerVatData`
    // and `customerAddress` are required. NAV's `CUSTOMER_DATA_EXPECTED`
    // business rule fires when status is non-PRIVATE_PERSON (DOMESTIC /
    // OTHER) and either of those two elements is missing — invoice 18
    // ABORTED on exactly this trap door before PR-77.
    let mut customer_vat_status: Option<String> = None;
    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "customerVatStatus" => {
                        let text = collect_text(reader, canonical)?;
                        customer_vat_status = Some(text);
                    }
                    "customerVatData" => walk_customer_vat_data(reader)?,
                    "customerName" => {
                        let _ = collect_text(reader, canonical)?;
                    }
                    "customerAddress" => walk_address("customerAddress", reader)?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                // PR-77 / session-101 — NAV business-rule
                // `CUSTOMER_DATA_EXPECTED`. When customerVatStatus is
                // anything OTHER than `PRIVATE_PERSON`, both
                // `customerVatData` AND `customerAddress` are required.
                // The rule fires the validator's
                // `MissingRequiredChild` loud-fail so the gap surfaces
                // at the ADR-0022 invariant check between render and
                // disk write — instead of as the NAV-test ABORTED
                // status that bit invoice 18 (`5E9KWQSOX3L9EC30`,
                // pointer `customerInfo/customerVatStatus` =
                // `DOMESTIC`).
                let status = customer_vat_status.as_deref().unwrap_or("");
                let is_business_buyer = !status.eq_ignore_ascii_case("PRIVATE_PERSON");
                if is_business_buyer {
                    if !seen.contains(&"customerVatData") {
                        return Err(NavXsdValidationError::MissingRequiredChild {
                            parent: PARENT,
                            expected: "customerVatData",
                        });
                    }
                    if !seen.contains(&"customerAddress") {
                        return Err(NavXsdValidationError::MissingRequiredChild {
                            parent: PARENT,
                            expected: "customerAddress",
                        });
                    }
                } else {
                    // PR-97 / ADR-0048 §5 — symmetric NEGATIVE half of
                    // `CUSTOMER_DATA_EXPECTED`. PRIVATE_PERSON buyers
                    // MUST NOT carry `<customerVatData>`; NAV rejects
                    // its presence at the business-rule layer. The
                    // positive half above is PR-77's hold; this branch
                    // closes the trap door on the negative half so a
                    // future emit drift (PrivatePerson body that still
                    // carries a stray `<customerVatData>`) loud-fails
                    // at the ADR-0022 invariant check.
                    if seen.contains(&"customerVatData") {
                        return Err(NavXsdValidationError::ForbiddenChildUnderStatus {
                            parent: PARENT,
                            element: "customerVatData",
                            status: "PRIVATE_PERSON",
                        });
                    }
                    // Session-154 (ADR-0048 amendment 2026-05-29) —
                    // NAV business rule `CUSTOMER_DATA_NOT_EXPECTED`
                    // ("Magánszemély vevő adatai nem adhatók meg.")
                    // ABORTED Ervin's invoice 31 (2026-05-29) because the
                    // wire body carried `<customerName>` + `<customerAddress>`
                    // under PRIVATE_PERSON. Sessions 148/150 made the emit
                    // unconditional for §169 (a *printed-invoice* obligation
                    // — the PDF still renders both). This branch closes the
                    // wire-side trap door: a PrivatePerson body carrying
                    // either field loud-fails at the ADR-0022 invariant
                    // check before it can reach NAV.
                    if seen.contains(&"customerName") {
                        return Err(NavXsdValidationError::ForbiddenChildUnderStatus {
                            parent: PARENT,
                            element: "customerName",
                            status: "PRIVATE_PERSON",
                        });
                    }
                    if seen.contains(&"customerAddress") {
                        return Err(NavXsdValidationError::ForbiddenChildUnderStatus {
                            parent: PARENT,
                            element: "customerAddress",
                            status: "PRIVATE_PERSON",
                        });
                    }
                }
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_customer_vat_data(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "customerVatData";
    const ALLOWED: &[&str] = &["customerTaxNumber"];
    expect_single_child_then_close(reader, PARENT, ALLOWED, walk_customer_tax_number)
}

fn walk_customer_tax_number(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "customerTaxNumber";
    const ALLOWED: &[&str] = &["taxpayerId", "vatCode", "countyCode"];
    const ORDERED_REQUIRED: &[&str] = ALLOWED;
    walk_structured_tax_number(reader, PARENT, ALLOWED, ORDERED_REQUIRED)
}

/// PR-66 / session-87 — shared walker for `<supplierTaxNumber>` and
/// `<customerTaxNumber>`. Factored out so the `common:` prefix
/// assertion (the PR-66 tightening) is written once and stays
/// symmetric between the two sides. The flat shape (no children, a
/// dashed-string text body) is rejected by `MissingRequiredChild` —
/// the load-bearing PR-50 invariant. The wrong-prefix shape is
/// rejected by `WrongChildNamespacePrefix` — the load-bearing PR-66
/// invariant.
///
/// Kept private (no `pub`) — the two call sites above are the only
/// allowed parents; a third structured-tax-number parent would be a
/// schema-level decision and would land its own walker.
fn walk_structured_tax_number(
    reader: &mut Reader<&[u8]>,
    parent: &'static str,
    allowed: &'static [&'static str],
    ordered_required: &'static [&'static str],
) -> Result<(), NavXsdValidationError> {
    const EXPECTED_PREFIX: &str = "common";
    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                // Capture the raw prefix BEFORE local-name canonicalization
                // so the loud-fail name names exactly what was written
                // on the wire (per CLAUDE.md rule 12 — the operator-
                // actionable message must point at the dropped prefix,
                // not at a generic "wrong element").
                let raw = e.name();
                let actual_prefix = prefix_of(raw).unwrap_or("").to_string();
                let local = local_name_of(raw).to_string();
                let canonical = canonicalize(allowed, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent,
                        element: local.clone(),
                    }
                })?;
                if actual_prefix != EXPECTED_PREFIX {
                    return Err(NavXsdValidationError::WrongChildNamespacePrefix {
                        parent,
                        element: canonical,
                        expected_prefix: EXPECTED_PREFIX,
                        actual_prefix,
                    });
                }
                let _ = collect_text(reader, canonical)?;
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(parent, ordered_required, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(parent, reader)),
            _ => {}
        }
    }
}
fn walk_address(
    address_tag: &'static str,
    reader: &mut Reader<&[u8]>,
) -> Result<(), NavXsdValidationError> {
    const ALLOWED: &[&str] = &["simpleAddress", "detailedAddress"];
    expect_single_child_then_close(reader, address_tag, ALLOWED, walk_address_child)
}

fn walk_address_child(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    // We don't enforce the inner leaf set strictly — NAV v3.0 has many
    // optional fields and they vary by simpleAddress vs detailedAddress.
    // We accept-and-walk-to-close so unknown leaves do not blow up;
    // the SUPPLIER and CUSTOMER addresses are emitter-controlled bytes
    // and a future PR that adds new address leaves will land them with
    // matching test fixtures.
    skip_to_matching_end(reader)
}

fn walk_invoice_detail(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "invoiceDetail";
    const ALLOWED: &[&str] = &[
        "invoiceCategory",
        "invoiceDeliveryDate",
        "currencyCode",
        "exchangeRate",
        "paymentMethod",
        "paymentDate",
        "invoiceAppearance",
    ];
    const ORDERED_REQUIRED: &[&str] = &[
        "invoiceCategory",
        "currencyCode",
        "exchangeRate",
        "invoiceAppearance",
    ];

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                let text = collect_text(reader, canonical)?;
                if canonical == "invoiceDeliveryDate" || canonical == "paymentDate" {
                    ensure_date_shape(canonical, &text)?;
                }
                if canonical == "exchangeRate" {
                    ensure_numeric_amount(canonical, &text)?;
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_invoice_lines(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "invoiceLines";
    const ALLOWED: &[&str] = &["mergedItemIndicator", "line"];

    let mut line_count: u32 = 0;
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "mergedItemIndicator" => {
                        let _ = collect_text(reader, canonical)?;
                    }
                    "line" => {
                        walk_line(reader)?;
                        line_count += 1;
                    }
                    other => unreachable!("canonicalized unknown element {other}"),
                }
            }
            Event::End(_) => {
                if line_count == 0 {
                    return Err(NavXsdValidationError::NoInvoiceLines);
                }
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_line(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "line";
    // `lineModificationReference` is optional — present only on chain
    // bodies (storno / modification, i.e. those carrying
    // `<invoiceReference>` at the head per ADR-0049 §NAV emit). It is
    // NOT in ORDERED_REQUIRED because plain new-invoice lines must
    // continue to validate without it; NAV's `LineType` positions it as
    // the second element (after `<lineNumber>`), and the emitter writes
    // it there. The projection-based ordered-required check does not
    // enforce that position — same posture as `<invoiceReference>`'s own
    // position within `<invoice>` (A40).
    const ALLOWED: &[&str] = &[
        "lineNumber",
        "lineModificationReference",
        "lineExpressionIndicator",
        "lineDescription",
        "quantity",
        "unitOfMeasure",
        // S159 — `<unitOfMeasureOwn>` (free-text unit) is permitted by
        // NAV's LineType ONLY when `<unitOfMeasure>` is `OWN`. The
        // post-walk pair-rule below enforces that conditional; the
        // allowlist just admits the element.
        "unitOfMeasureOwn",
        "unitPrice",
        "lineAmountsNormal",
    ];
    const ORDERED_REQUIRED: &[&str] = &[
        "lineNumber",
        "lineDescription",
        "quantity",
        "unitOfMeasure",
        "unitPrice",
        "lineAmountsNormal",
    ];

    // S159 — capture the `<unitOfMeasure>` text so the post-walk pair-rule
    // can decide whether `<unitOfMeasureOwn>` is required (status `OWN`),
    // forbidden (any other token), or absent-and-fine.
    let mut unit_of_measure: Option<String> = None;
    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "lineNumber" => {
                        let text = collect_text(reader, canonical)?;
                        ensure_numeric_amount(canonical, &text)?;
                    }
                    "lineExpressionIndicator" => {
                        let _ = collect_text(reader, canonical)?;
                    }
                    "lineDescription" => {
                        let _ = collect_text(reader, canonical)?;
                    }
                    "quantity" => {
                        let text = collect_text(reader, canonical)?;
                        ensure_numeric_amount(canonical, &text)?;
                    }
                    "unitOfMeasure" => {
                        unit_of_measure = Some(collect_text(reader, canonical)?);
                    }
                    "unitOfMeasureOwn" => {
                        let _ = collect_text(reader, canonical)?;
                    }
                    "unitPrice" => {
                        let text = collect_text(reader, canonical)?;
                        ensure_numeric_amount(canonical, &text)?;
                    }
                    "lineAmountsNormal" => walk_line_amounts_normal(reader)?,
                    "lineModificationReference" => walk_line_modification_reference(reader)?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                // S159 — `<unitOfMeasure>OWN</...>` ↔ `<unitOfMeasureOwn>`
                // pair rule. NAV's LineType permits the free-text element
                // ONLY alongside the `OWN` token. `check_ordered_required`
                // already guaranteed `<unitOfMeasure>` is present.
                let is_own = unit_of_measure
                    .as_deref()
                    .is_some_and(|u| u.eq_ignore_ascii_case("OWN"));
                let has_own_text = seen.contains(&"unitOfMeasureOwn");
                if is_own && !has_own_text {
                    return Err(NavXsdValidationError::MissingRequiredChild {
                        parent: PARENT,
                        expected: "unitOfMeasureOwn",
                    });
                }
                if !is_own && has_own_text {
                    return Err(NavXsdValidationError::ForbiddenUnitOfMeasureOwn {
                        unit_of_measure: unit_of_measure.unwrap_or_default(),
                    });
                }
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

/// `<lineModificationReference>` chain-line block — ADR-0049 §NAV emit.
/// Present on every `<line>` of a storno / modification body (those
/// carrying `<invoiceReference>`). NAV rejects a chain body whose lines
/// omit it with business rule `LINE_MODIFICATION_EXPECTED`. Both children
/// are required by NAV's `LineModificationReferenceType`:
///   - `<lineNumberReference>` — the line's position on the ORIGINAL
///     invoice (positive int).
///   - `<lineOperation>` — `LINE_OPERATION` enum (`CREATE` | `MODIFY`).
///     The validator does NOT constrain the enum value (same posture as
///     `<modifyWithoutMaster>` / `<annulmentCode>` — the closed-set lives
///     at the emitter's `CHAIN_LINE_OPERATION` const, not the XSD walk).
fn walk_line_modification_reference(
    reader: &mut Reader<&[u8]>,
) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "lineModificationReference";
    const ALLOWED: &[&str] = &["lineNumberReference", "lineOperation"];
    const ORDERED_REQUIRED: &[&str] = ALLOWED;

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                if canonical == "lineNumberReference" {
                    let text = collect_text(reader, canonical)?;
                    ensure_numeric_amount(canonical, &text)?;
                } else {
                    let _ = collect_text(reader, canonical)?;
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_line_amounts_normal(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "lineAmountsNormal";
    const ALLOWED: &[&str] = &[
        "lineNetAmountData",
        "lineVatRate",
        "lineVatData",
        "lineGrossAmountData",
    ];
    const ORDERED_REQUIRED: &[&str] = ALLOWED;

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "lineNetAmountData" => {
                        walk_amount_pair(canonical, &["lineNetAmount", "lineNetAmountHUF"], reader)?
                    }
                    "lineVatRate" => walk_line_vat_rate(reader)?,
                    "lineVatData" => {
                        walk_amount_pair(canonical, &["lineVatAmount", "lineVatAmountHUF"], reader)?
                    }
                    "lineGrossAmountData" => walk_amount_pair(
                        canonical,
                        &["lineGrossAmountNormal", "lineGrossAmountNormalHUF"],
                        reader,
                    )?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_line_vat_rate(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    walk_vat_rate_choice("lineVatRate", reader)
}

/// Walk <vatRate> inside summaryByVatRate (same shape as lineVatRate).
fn walk_vat_rate(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    walk_vat_rate_choice("vatRate", reader)
}

/// ADR-0101 — the shared `lineVatRate` / `vatRate` choice-group walker.
///
/// NAV's `LineVatRateType` is an XSD choice; the emitter renders exactly
/// one member. This validator models the members ABERP now emits:
/// `vatPercentage` (numeric-checked), `vatExemption` / `vatOutOfScope`
/// (interior `case` + `reason` now validated, not skipped —
/// [`walk_vat_detail`]), and `vatDomesticReverseCharge` (boolean leaf,
/// text-consumed). `vatContent` stays in the allowlist (ADR-0022 loose
/// model) but is skipped. At least one member must appear.
///
/// Note (matches pre-ADR-0101 posture): this does not enforce STRICT
/// exactly-one — a malformed body with two choice members is not rejected
/// here. NAV's server-side schema is the backstop for that; the emitter
/// only ever writes one member (see `nav_xml::write_vat_rate_choice`).
fn walk_vat_rate_choice(
    parent: &'static str,
    reader: &mut Reader<&[u8]>,
) -> Result<(), NavXsdValidationError> {
    const ALLOWED: &[&str] = &[
        "vatPercentage",
        "vatContent",
        "vatExemption",
        "vatOutOfScope",
        "vatDomesticReverseCharge",
    ];
    let mut any_seen = false;
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "vatPercentage" => {
                        let text = collect_text(reader, canonical)?;
                        ensure_numeric_amount(canonical, &text)?;
                    }
                    "vatExemption" | "vatOutOfScope" => walk_vat_detail(canonical, reader)?,
                    // vatContent (loose model) + vatDomesticReverseCharge
                    // (boolean leaf) — consume without deep validation.
                    _ => skip_to_matching_end(reader)?,
                }
                any_seen = true;
            }
            Event::End(_) => {
                if !any_seen {
                    return Err(NavXsdValidationError::MissingRequiredChild {
                        parent,
                        expected: "vatPercentage",
                    });
                }
                return Ok(());
            }
            Event::Eof => return Err(eof_in(parent, reader)),
            _ => {}
        }
    }
}

/// ADR-0101 — walk the interior of `<vatExemption>` / `<vatOutOfScope>`.
/// NAV's `DetailedReasonType` requires BOTH `<case>` (a code-list token)
/// and `<reason>` (non-blank free text), `case` before `reason`. Modeling
/// them (rather than `skip_to_matching_end` as before ADR-0101) makes a
/// malformed exemption — e.g. one missing `<case>` — a local
/// `MissingRequiredChild` instead of slipping past to a NAV-side rejection.
fn walk_vat_detail(
    parent: &'static str,
    reader: &mut Reader<&[u8]>,
) -> Result<(), NavXsdValidationError> {
    const ALLOWED: &[&str] = &["case", "reason"];
    const ORDERED_REQUIRED: &[&str] = &["case", "reason"];
    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent,
                        element: local.clone(),
                    }
                })?;
                // Consume the text (also rejects a nested element inside
                // <case>/<reason> via collect_text's Start arm).
                let _ = collect_text(reader, canonical)?;
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(parent, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(parent, reader)),
            _ => {}
        }
    }
}
fn walk_amount_pair(
    parent: &'static str,
    expected: &'static [&'static str],
    reader: &mut Reader<&[u8]>,
) -> Result<(), NavXsdValidationError> {
    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(expected, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent,
                        element: local.clone(),
                    }
                })?;
                let text = collect_text(reader, canonical)?;
                ensure_numeric_amount(canonical, &text)?;
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(parent, expected, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(parent, reader)),
            _ => {}
        }
    }
}

fn walk_invoice_summary(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "invoiceSummary";
    const ALLOWED: &[&str] = &["summaryNormal", "summaryGrossData", "summarySimplified"];
    const ORDERED_REQUIRED: &[&str] = &["summaryNormal", "summaryGrossData"];

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "summaryNormal" => walk_summary_normal(reader)?,
                    "summaryGrossData" => walk_summary_gross_data(reader)?,
                    "summarySimplified" => skip_to_matching_end(reader)?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_summary_normal(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "summaryNormal";
    const ALLOWED: &[&str] = &[
        "summaryByVatRate",
        "invoiceNetAmount",
        "invoiceNetAmountHUF",
        "invoiceVatAmount",
        "invoiceVatAmountHUF",
    ];
    const ORDERED_REQUIRED: &[&str] = &[
        "summaryByVatRate",
        "invoiceNetAmount",
        "invoiceNetAmountHUF",
        "invoiceVatAmount",
        "invoiceVatAmountHUF",
    ];

    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "summaryByVatRate" => walk_summary_by_vat_rate(reader)?,
                    "invoiceNetAmount"
                    | "invoiceNetAmountHUF"
                    | "invoiceVatAmount"
                    | "invoiceVatAmountHUF" => {
                        let text = collect_text(reader, canonical)?;
                        ensure_numeric_amount(canonical, &text)?;
                    }
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen)?;
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}

fn walk_summary_by_vat_rate(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "summaryByVatRate";
    const ALLOWED: &[&str] = &[
        "vatRateNetData",
        "vatRateVatData",
        "vatRateGrossData",
        "vatRate",
    ];
    let mut seen: Vec<&'static str> = Vec::new();
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                match canonical {
                    "vatRateNetData" => walk_amount_pair(
                        canonical,
                        &["vatRateNetAmount", "vatRateNetAmountHUF"],
                        reader,
                    )?,
                    "vatRateVatData" => walk_amount_pair(
                        canonical,
                        &["vatRateVatAmount", "vatRateVatAmountHUF"],
                        reader,
                    )?,
                    "vatRateGrossData" => walk_amount_pair(
                        canonical,
                        &["vatRateGrossAmount", "vatRateGrossAmountHUF"],
                        reader,
                    )?,
                    "vatRate" => walk_vat_rate(reader)?,
                    other => unreachable!("canonicalized unknown element {other}"),
                }
                seen.push(canonical);
            }
            Event::End(_) => {
                if !seen.contains(&"vatRateGrossData") {
                    return Err(NavXsdValidationError::MissingRequiredChild {
                        parent: PARENT,
                        expected: "vatRateGrossData",
                    });
                }
                return Ok(());
            }
            Event::Eof => return Err(eof_in(PARENT, reader)),
            _ => {}
        }
    }
}
fn walk_summary_gross_data(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    walk_amount_pair(
        "summaryGrossData",
        &["invoiceGrossAmount", "invoiceGrossAmountHUF"],
        reader,
    )
}

// ── PR-13 / ADR-0026 §4 — `<InvoiceAnnulment>` validator ────────────
//
// Separate public entry point from `validate_invoice_data`. Same
// streaming-walker shape; different allowlist (four required
// children, all text leaves).
//
// Allowlist source-of-truth discipline mirrors the
// `<InvoiceData>` side (ADR-0022 / module header).

/// Validate that `xml` is a v3.0 `<InvoiceAnnulment>` payload
/// structurally acceptable to NAV (ADR-0026 §4; closes session 16
/// handoff F30). Returns `Ok(())` on success; on any divergence
/// returns a typed [`NavXsdValidationError`].
///
/// Allowlist (exhaustive for v3.0):
///
/// - Root: `<InvoiceAnnulment>` at namespace
///   [`crate::NAV_NS_ANNUL`].
/// - Required children, in document order:
///   1. `<annulmentReference>` — text content (the base invoice's
///      NAV-facing number).
///   2. `<annulmentTimestamp>` — text content (ISO 8601 UTC
///      `YYYY-MM-DDTHH:MM:SSZ` per ADR-0025 §4; the NAV-compressed
///      `YYYYMMDDhhmmss` form is the named-trigger amendment surface
///      per ADR-0025 §"Open questions").
///   3. `<annulmentCode>` — text content (one of the four wire-form
///      codes per ADR-0025 §"Surfaced conflict 2"; the validator
///      does NOT enforce the closed-set — the CLI clap-ValueEnum is
///      the loud-fail boundary per ADR-0026 §4).
///   4. `<annulmentReason>` — text content (free-form operator
///      reason).
///
/// The walker does NOT date-shape-check `<annulmentTimestamp>`
/// against ISO 8601 (NAV's v3.0 declares it as `xs:dateTime` rather
/// than `xs:date`; the emitter pins the shape via
/// `OffsetDateTime::now_utc()` formatting, and the validator
/// accepts whatever well-formed text the operator's hand-edit could
/// produce). Same posture as `<lineDescription>` text content in
/// [`validate_invoice_data`].
///
/// The call is single-pass over the input bytes; no in-memory tree
/// is built. The input must be valid UTF-8 (NAV v3.0
/// `<InvoiceAnnulment>` is UTF-8 by spec).
pub fn validate_annulment_data(xml: &[u8]) -> Result<(), NavXsdValidationError> {
    let s = std::str::from_utf8(xml).map_err(|e| NavXsdValidationError::MalformedXml {
        position: e.valid_up_to(),
        message: format!("input is not valid UTF-8: {e}"),
    })?;
    let mut reader = Reader::from_str(s);
    reader.config_mut().trim_text(true);

    // Walk to the root element tag. Same skip-decl-and-whitespace
    // posture as `validate_invoice_data`.
    let root = loop {
        match read_event(&mut reader)? {
            Event::Start(e) | Event::Empty(e) => break e,
            Event::Eof => {
                return Err(NavXsdValidationError::MalformedXml {
                    position: reader.buffer_position() as usize,
                    message: "document ended before any element".into(),
                });
            }
            _ => continue,
        }
    };

    // Root must be <InvoiceAnnulment> at the NAV v3.0 annul namespace.
    let root_local = local_name_of(root.name());
    if root_local != "InvoiceAnnulment" {
        return Err(NavXsdValidationError::UnexpectedRoot {
            actual: root_local.to_string(),
        });
    }
    let xmlns = extract_xmlns(&root)?;
    if xmlns.as_deref() != Some(NAV_NS_ANNUL) {
        return Err(NavXsdValidationError::UnexpectedRootNamespace {
            expected: NAV_NS_ANNUL,
            actual: xmlns,
        });
    }

    walk_invoice_annulment(&mut reader)
}

fn walk_invoice_annulment(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    const PARENT: &str = "InvoiceAnnulment";
    const ALLOWED: &[&str] = &[
        "annulmentReference",
        "annulmentTimestamp",
        "annulmentCode",
        "annulmentReason",
    ];
    // All four are required per ADR-0026 §4 and ADR-0025 §4.
    const ORDERED_REQUIRED: &[&str] = ALLOWED;

    let mut seen_in_order: Vec<&'static str> = Vec::new();

    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(ALLOWED, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent: PARENT,
                        element: local.clone(),
                    }
                })?;
                // All four allowed elements are text-leaf children
                // (no nested structure). collect_text walks to the
                // matching </tag> and returns the accumulated text;
                // we discard the value here — the validator's job is
                // schema-shape conformance, not value semantics. The
                // four-code closed-set check on <annulmentCode>
                // lives at the CLI's clap-ValueEnum boundary
                // (ADR-0025 §3 / ADR-0026 §4).
                let _ = collect_text(reader, canonical)?;
                seen_in_order.push(canonical);
            }
            Event::End(_) => {
                check_ordered_required(PARENT, ORDERED_REQUIRED, &seen_in_order)?;
                return Ok(());
            }
            Event::Eof => {
                return Err(NavXsdValidationError::MalformedXml {
                    position: reader.buffer_position() as usize,
                    message: "document ended inside <InvoiceAnnulment>".into(),
                });
            }
            _ => {}
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────────

fn expect_single_child_then_close<F>(
    reader: &mut Reader<&[u8]>,
    parent: &'static str,
    allowed: &'static [&'static str],
    mut inner: F,
) -> Result<(), NavXsdValidationError>
where
    F: FnMut(&mut Reader<&[u8]>) -> Result<(), NavXsdValidationError>,
{
    let mut count: u32 = 0;
    let mut child_canonical: Option<&'static str> = None;
    loop {
        match read_event(reader)? {
            Event::Start(e) => {
                let local = local_name_of(e.name()).to_string();
                let canonical = canonicalize(allowed, &local).ok_or_else(|| {
                    NavXsdValidationError::UnexpectedElement {
                        parent,
                        element: local.clone(),
                    }
                })?;
                count += 1;
                if count > 1 {
                    return Err(NavXsdValidationError::CardinalityExceeded {
                        parent,
                        element: child_canonical.unwrap_or(canonical),
                        max: 1,
                        actual: count,
                    });
                }
                child_canonical = Some(canonical);
                inner(reader)?;
            }
            Event::End(_) => {
                if count == 0 {
                    return Err(NavXsdValidationError::MissingRequiredChild {
                        parent,
                        expected: allowed[0],
                    });
                }
                return Ok(());
            }
            Event::Eof => return Err(eof_in(parent, reader)),
            _ => {}
        }
    }
}

fn skip_to_matching_end(reader: &mut Reader<&[u8]>) -> Result<(), NavXsdValidationError> {
    let mut depth: i32 = 1;
    while depth > 0 {
        match read_event(reader)? {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth -= 1,
            Event::Empty(_) => { /* depth unchanged */ }
            Event::Eof => {
                return Err(NavXsdValidationError::MalformedXml {
                    position: reader.buffer_position() as usize,
                    message: "EOF while skipping nested content".into(),
                });
            }
            _ => {}
        }
    }
    Ok(())
}

fn collect_text(
    reader: &mut Reader<&[u8]>,
    field: &'static str,
) -> Result<String, NavXsdValidationError> {
    let mut text = String::new();
    loop {
        match read_event(reader)? {
            Event::Text(t) => {
                let unescaped = t
                    .unescape()
                    .map_err(|e| NavXsdValidationError::MalformedXml {
                        position: reader.buffer_position() as usize,
                        message: format!("text unescape in <{field}>: {e}"),
                    })?
                    .into_owned();
                text.push_str(&unescaped);
            }
            Event::End(_) => return Ok(text),
            Event::Start(e) => {
                let inner = local_name_of(e.name()).to_string();
                return Err(NavXsdValidationError::UnexpectedElement {
                    parent: field,
                    element: inner,
                });
            }
            Event::Eof => return Err(eof_in(field, reader)),
            _ => {}
        }
    }
}

fn read_event<'a>(reader: &mut Reader<&'a [u8]>) -> Result<Event<'a>, NavXsdValidationError> {
    // Explicit match (not `.map_err(|e| reader.buffer_position()...)`)
    // because the closure form re-borrows `reader` while the original
    // `read_event` borrow is still active — that fails the borrow
    // check even though logically the closure only runs after
    // read_event has finished.
    match reader.read_event() {
        Ok(ev) => Ok(ev),
        Err(e) => {
            let pos = reader.buffer_position() as usize;
            Err(NavXsdValidationError::MalformedXml {
                position: pos,
                message: e.to_string(),
            })
        }
    }
}

fn eof_in(parent: &'static str, reader: &Reader<&[u8]>) -> NavXsdValidationError {
    NavXsdValidationError::MalformedXml {
        position: reader.buffer_position() as usize,
        message: format!("EOF inside <{parent}>"),
    }
}

/// Local-name slice of a `QName`. The returned `&str` borrows from the
/// same source as the input `QName<'a>` — the `from_str` source string.
///
/// `QName<'a>` is `pub struct QName<'a>(pub &'a [u8])` in quick-xml
/// 0.36; we destructure the public inner field directly. `as_ref()`
/// would return a slice borrowing from the local `name` value, which
/// is the lifetime mistake we ran into the first time.
fn local_name_of<'a>(name: QName<'a>) -> &'a str {
    let raw: &'a [u8] = name.0;
    let local = match raw.iter().rposition(|&b| b == b':') {
        Some(i) => &raw[i + 1..],
        None => raw,
    };
    std::str::from_utf8(local).unwrap_or("")
}

/// PR-66 / session-87 — namespace-prefix slice of a `QName`. Returns
/// `Some("common")` for `<common:taxpayerId>`, `None` for bare
/// `<taxpayerId>`. Used by `walk_structured_tax_number` to enforce
/// the `common:` prefix on supplier/customer tax-number children
/// (NAV v3.0 base namespace).
///
/// Mirrors `local_name_of`'s lifetime posture — the returned `&str`
/// borrows from the same `QName<'a>` source. `rposition` matches
/// `local_name_of`'s rightmost-colon split so the prefix and local
/// name partition the same byte slice consistently.
fn prefix_of<'a>(name: QName<'a>) -> Option<&'a str> {
    let raw: &'a [u8] = name.0;
    let idx = raw.iter().position(|&b| b == b':')?;
    std::str::from_utf8(&raw[..idx]).ok()
}

fn canonicalize(allowed: &'static [&'static str], local: &str) -> Option<&'static str> {
    allowed.iter().copied().find(|a| *a == local)
}

fn extract_xmlns(
    start: &quick_xml::events::BytesStart,
) -> Result<Option<String>, NavXsdValidationError> {
    for attr in start.attributes() {
        let attr = attr.map_err(|e| NavXsdValidationError::MalformedXml {
            position: 0,
            message: format!("attribute parse: {e}"),
        })?;
        if attr.key.as_ref() == b"xmlns" {
            let v = attr
                .unescape_value()
                .map_err(|e| NavXsdValidationError::MalformedXml {
                    position: 0,
                    message: format!("xmlns unescape: {e}"),
                })?
                .into_owned();
            return Ok(Some(v));
        }
    }
    Ok(None)
}

fn ensure_date_shape(field: &'static str, text: &str) -> Result<(), NavXsdValidationError> {
    let bytes = text.as_bytes();
    let ok = bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[..4].iter().all(|b| b.is_ascii_digit())
        && bytes[5..7].iter().all(|b| b.is_ascii_digit())
        && bytes[8..10].iter().all(|b| b.is_ascii_digit());
    if !ok {
        return Err(NavXsdValidationError::MalformedDate {
            field,
            actual: text.to_string(),
        });
    }
    Ok(())
}

fn ensure_numeric_amount(field: &'static str, text: &str) -> Result<(), NavXsdValidationError> {
    if text.is_empty() {
        return Err(NavXsdValidationError::NonNumericAmount {
            field,
            actual: text.to_string(),
        });
    }
    // NAV v3.0 storno line/summary amounts are NEGATIVE per the schema
    // (PR-10, ADR-0023). Accept a single optional leading `-`; reject
    // `--`, a trailing `-`, or any other sign-like glyph. A bare `-`
    // (no digits) is also rejected because the digits loop would see
    // an empty remainder. CLAUDE.md rule 12 — fail loud on garbage.
    let mut bytes = text.bytes();
    let mut decimal_seen = false;
    let mut digit_seen = false;
    let first = bytes.next().expect("empty case handled above");
    match first {
        b'-' => {} // legal leading sign
        b'0'..=b'9' => digit_seen = true,
        b'.' => decimal_seen = true,
        _ => {
            return Err(NavXsdValidationError::NonNumericAmount {
                field,
                actual: text.to_string(),
            });
        }
    }
    for b in bytes {
        match b {
            b'0'..=b'9' => digit_seen = true,
            b'.' if !decimal_seen => decimal_seen = true,
            _ => {
                return Err(NavXsdValidationError::NonNumericAmount {
                    field,
                    actual: text.to_string(),
                });
            }
        }
    }
    if !digit_seen {
        return Err(NavXsdValidationError::NonNumericAmount {
            field,
            actual: text.to_string(),
        });
    }
    Ok(())
}

fn check_ordered_required(
    parent: &'static str,
    required: &'static [&'static str],
    seen_in_order: &[&'static str],
) -> Result<(), NavXsdValidationError> {
    for needed in required {
        if !seen_in_order.contains(needed) {
            return Err(NavXsdValidationError::MissingRequiredChild {
                parent,
                expected: needed,
            });
        }
    }
    let projection: Vec<&'static str> = seen_in_order
        .iter()
        .copied()
        .filter(|s| required.contains(s))
        .collect();
    for (i, r) in required.iter().enumerate() {
        match projection.get(i) {
            Some(p) if p == r => continue,
            Some(p) => {
                return Err(NavXsdValidationError::ChildOrderViolation {
                    parent,
                    expected_before: r,
                    actually_appeared_first: (*p).to_string(),
                });
            }
            None => {
                return Err(NavXsdValidationError::MissingRequiredChild {
                    parent,
                    expected: r,
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The minimum valid InvoiceData that the v3.0 allowlist accepts.
    /// Updated whenever the allowlist or emitter changes; this fixture
    /// is the validator's golden positive example.
    const MIN_VALID: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<InvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data" xmlns:common="http://schemas.nav.gov.hu/OSA/3.0/base">
  <invoiceNumber>INV-default/00001</invoiceNumber>
  <invoiceIssueDate>2026-05-20</invoiceIssueDate>
  <completenessIndicator>false</completenessIndicator>
  <invoiceMain>
    <invoice>
      <invoiceHead>
        <supplierInfo>
          <supplierTaxNumber>
            <common:taxpayerId>12345678</common:taxpayerId>
            <common:vatCode>1</common:vatCode>
            <common:countyCode>42</common:countyCode>
          </supplierTaxNumber>
          <supplierName>ABERP Supplier Kft.</supplierName>
          <supplierAddress>
            <simpleAddress>
              <countryCode>HU</countryCode>
              <postalCode>1011</postalCode>
              <city>Budapest</city>
              <additionalAddressDetail>Fő utca 1.</additionalAddressDetail>
            </simpleAddress>
          </supplierAddress>
        </supplierInfo>
        <customerInfo>
          <customerVatStatus>DOMESTIC</customerVatStatus>
          <customerVatData>
            <customerTaxNumber>
              <common:taxpayerId>87654321</common:taxpayerId>
              <common:vatCode>1</common:vatCode>
              <common:countyCode>42</common:countyCode>
            </customerTaxNumber>
          </customerVatData>
          <customerName>Test Customer Zrt.</customerName>
          <customerAddress>
            <common:simpleAddress>
              <common:countryCode>HU</common:countryCode>
              <common:postalCode>1052</common:postalCode>
              <common:city>Budapest</common:city>
              <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>
            </common:simpleAddress>
          </customerAddress>
        </customerInfo>
        <invoiceDetail>
          <invoiceCategory>NORMAL</invoiceCategory>
          <invoiceDeliveryDate>2026-05-20</invoiceDeliveryDate>
          <currencyCode>HUF</currencyCode>
          <exchangeRate>1</exchangeRate>
          <paymentMethod>TRANSFER</paymentMethod>
          <paymentDate>2026-05-20</paymentDate>
          <invoiceAppearance>ELECTRONIC</invoiceAppearance>
        </invoiceDetail>
      </invoiceHead>
      <invoiceLines>
        <mergedItemIndicator>false</mergedItemIndicator>
        <line>
          <lineNumber>1</lineNumber>
          <lineExpressionIndicator>false</lineExpressionIndicator>
          <lineDescription>Test widget</lineDescription>
          <quantity>2</quantity>
          <unitOfMeasure>PIECE</unitOfMeasure>
          <unitPrice>1000</unitPrice>
          <lineAmountsNormal>
            <lineNetAmountData>
              <lineNetAmount>2000</lineNetAmount>
              <lineNetAmountHUF>2000</lineNetAmountHUF>
            </lineNetAmountData>
            <lineVatRate>
              <vatPercentage>0.27</vatPercentage>
            </lineVatRate>
            <lineVatData>
              <lineVatAmount>540</lineVatAmount>
              <lineVatAmountHUF>540</lineVatAmountHUF>
            </lineVatData>
            <lineGrossAmountData>
              <lineGrossAmountNormal>2540</lineGrossAmountNormal>
              <lineGrossAmountNormalHUF>2540</lineGrossAmountNormalHUF>
            </lineGrossAmountData>
          </lineAmountsNormal>
        </line>
      </invoiceLines>
      <invoiceSummary>
        <summaryNormal>
          <summaryByVatRate>
            <vatRate>
              <vatPercentage>0.27</vatPercentage>
            </vatRate>
            <vatRateNetData>
              <vatRateNetAmount>2000</vatRateNetAmount>
              <vatRateNetAmountHUF>2000</vatRateNetAmountHUF>
            </vatRateNetData>
            <vatRateVatData>
              <vatRateVatAmount>540</vatRateVatAmount>
              <vatRateVatAmountHUF>540</vatRateVatAmountHUF>
            </vatRateVatData>
            <vatRateGrossData>
              <vatRateGrossAmount>2540</vatRateGrossAmount>
              <vatRateGrossAmountHUF>2540</vatRateGrossAmountHUF>
            </vatRateGrossData>
          </summaryByVatRate>
          <invoiceNetAmount>2000</invoiceNetAmount>
          <invoiceNetAmountHUF>2000</invoiceNetAmountHUF>
          <invoiceVatAmount>540</invoiceVatAmount>
          <invoiceVatAmountHUF>540</invoiceVatAmountHUF>
        </summaryNormal>
        <summaryGrossData>
          <invoiceGrossAmount>2540</invoiceGrossAmount>
          <invoiceGrossAmountHUF>2540</invoiceGrossAmountHUF>
        </summaryGrossData>
      </invoiceSummary>
    </invoice>
  </invoiceMain>
</InvoiceData>"#;

    #[test]
    fn minimum_valid_invoice_data_validates() {
        validate_invoice_data(MIN_VALID.as_bytes())
            .expect("the hand-rolled v3.0 minimum example must validate");
    }

    #[test]
    fn empty_input_is_loud_fail() {
        let err = validate_invoice_data(b"").unwrap_err();
        match err {
            NavXsdValidationError::MalformedXml { .. } => {}
            other => panic!("expected MalformedXml for empty input, got {other:?}"),
        }
    }

    #[test]
    fn wrong_root_element_is_loud_fail() {
        let xml = r#"<?xml version="1.0"?><NotInvoiceData xmlns="http://schemas.nav.gov.hu/OSA/3.0/data"/>"#;
        let err = validate_invoice_data(xml.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedRoot { actual } => {
                assert_eq!(actual, "NotInvoiceData");
            }
            other => panic!("expected UnexpectedRoot, got {other:?}"),
        }
    }

    #[test]
    fn wrong_root_namespace_is_loud_fail() {
        let xml = r#"<?xml version="1.0"?><InvoiceData xmlns="http://example.com/wrong"/>"#;
        let err = validate_invoice_data(xml.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedRootNamespace { expected, actual } => {
                assert_eq!(expected, NAV_NS_DATA);
                assert_eq!(actual.as_deref(), Some("http://example.com/wrong"));
            }
            other => panic!("expected UnexpectedRootNamespace, got {other:?}"),
        }
    }

    #[test]
    fn missing_invoice_number_is_loud_fail() {
        let bad = MIN_VALID.replace("<invoiceNumber>INV-default/00001</invoiceNumber>", "");
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "InvoiceData");
                assert_eq!(expected, "invoiceNumber");
            }
            other => panic!("expected MissingRequiredChild invoiceNumber, got {other:?}"),
        }
    }

    /// PR-76 — `<completenessIndicator>` is REQUIRED per NAV v3.0
    /// InvoiceData XSD. Pre-PR-76 the emitter omitted it and the
    /// validator tolerated the absence; NAV-test rejected invoice 17
    /// with `SCHEMA_VIOLATION` naming this element as the expected
    /// next child of `InvoiceData`. The new pin asserts the validator
    /// loud-fails on the missing element so a future regression
    /// surfaces locally (CLAUDE.md rule 12) instead of as an
    /// `InvoiceAckStatus::ABORTED` after a live submission.
    #[test]
    fn missing_completeness_indicator_is_loud_fail() {
        let bad = MIN_VALID.replace("<completenessIndicator>false</completenessIndicator>", "");
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "InvoiceData");
                assert_eq!(expected, "completenessIndicator");
            }
            other => panic!("expected MissingRequiredChild completenessIndicator, got {other:?}"),
        }
    }

    #[test]
    fn unexpected_element_is_loud_fail() {
        let bad = MIN_VALID.replace(
            "<invoiceMain>",
            "<unknownElement>x</unknownElement><invoiceMain>",
        );
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedElement { parent, element } => {
                assert_eq!(parent, "InvoiceData");
                assert_eq!(element, "unknownElement");
            }
            other => panic!("expected UnexpectedElement unknownElement, got {other:?}"),
        }
    }

    #[test]
    fn malformed_invoice_issue_date_is_loud_fail() {
        let bad = MIN_VALID.replace(
            "<invoiceIssueDate>2026-05-20</invoiceIssueDate>",
            "<invoiceIssueDate>2026/05/20</invoiceIssueDate>",
        );
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::MalformedDate { field, actual } => {
                assert_eq!(field, "invoiceIssueDate");
                assert_eq!(actual, "2026/05/20");
            }
            other => panic!("expected MalformedDate, got {other:?}"),
        }
    }

    #[test]
    fn non_numeric_amount_is_loud_fail() {
        let bad = MIN_VALID.replace("<unitPrice>1000</unitPrice>", "<unitPrice>1k</unitPrice>");
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::NonNumericAmount { field, actual } => {
                assert_eq!(field, "unitPrice");
                assert_eq!(actual, "1k");
            }
            other => panic!("expected NonNumericAmount, got {other:?}"),
        }
    }

    /// PR-77 / session-101 — NAV business-rule `CUSTOMER_DATA_EXPECTED`
    /// trap door: `customerVatStatus=DOMESTIC` paired with a missing
    /// `<customerVatData>` block is a loud-fail. The pre-PR-77
    /// validator marked customerVatData as optional ("PRIVATE_PERSON
    /// has no tax data"); but the NAV BUSINESS rule promotes it to
    /// required under any non-PRIVATE_PERSON status. The MIN_VALID
    /// fixture's status is `DOMESTIC`, so stripping the block exercises
    /// the conditional branch.
    #[test]
    fn missing_customer_vat_data_under_domestic_is_loud_fail() {
        let needle_open = "<customerVatData>";
        let needle_close = "</customerVatData>";
        let open = MIN_VALID
            .find(needle_open)
            .expect("MIN_VALID contains <customerVatData>");
        let close = MIN_VALID
            .find(needle_close)
            .expect("MIN_VALID contains </customerVatData>")
            + needle_close.len();
        let bad = format!("{}{}", &MIN_VALID[..open], &MIN_VALID[close..]);
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "customerInfo");
                assert_eq!(expected, "customerVatData");
            }
            other => panic!("expected MissingRequiredChild customerVatData, got {other:?}"),
        }
    }

    /// PR-77 / session-101 — NAV business-rule `CUSTOMER_DATA_EXPECTED`,
    /// the OTHER half of the trap door: a DOMESTIC customerVatStatus
    /// without `<customerAddress>` is a loud-fail. This is the actual
    /// rule NAV-test fired against invoice 18 (transaction
    /// `5E9KWQSOX3L9EC30`); the pre-PR-77 emitter wrote
    /// `customerVatData` correctly but omitted `customerAddress`, and
    /// the pre-PR-77 validator had no rule against the omission. The
    /// new pin asserts the strengthened validator catches the gap
    /// locally so a regression cannot reach NAV-test again.
    #[test]
    fn missing_customer_address_under_domestic_is_loud_fail() {
        let needle_open = "<customerAddress>";
        let needle_close = "</customerAddress>";
        let open = MIN_VALID
            .find(needle_open)
            .expect("MIN_VALID contains <customerAddress>");
        let close = MIN_VALID
            .find(needle_close)
            .expect("MIN_VALID contains </customerAddress>")
            + needle_close.len();
        let bad = format!("{}{}", &MIN_VALID[..open], &MIN_VALID[close..]);
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "customerInfo");
                assert_eq!(expected, "customerAddress");
            }
            other => panic!("expected MissingRequiredChild customerAddress, got {other:?}"),
        }
    }

    /// PR-77 / session-101, amended Session-154 — sanity-check the OTHER
    /// side of the conditional branch: a `PRIVATE_PERSON` customerVatStatus
    /// legitimately carries NONE of `<customerVatData>`, `<customerName>`,
    /// `<customerAddress>` (the natural-person buyer's data is intentionally
    /// absent — NAV's `CUSTOMER_DATA_NOT_EXPECTED` rule forbids all three).
    /// The validator's conditional-required logic must NOT fire on this
    /// bare shape. This pin guards against a regression that promotes any
    /// of the three to unconditionally required.
    #[test]
    fn private_person_status_accepts_missing_customer_vat_data_and_address() {
        // Replace the customerInfo block wholesale: PRIVATE_PERSON +
        // status only — no vatData, no name, no address.
        let original = "<customerInfo>\n          <customerVatStatus>DOMESTIC</customerVatStatus>\n          <customerVatData>\n            <customerTaxNumber>\n              <common:taxpayerId>87654321</common:taxpayerId>\n              <common:vatCode>1</common:vatCode>\n              <common:countyCode>42</common:countyCode>\n            </customerTaxNumber>\n          </customerVatData>\n          <customerName>Test Customer Zrt.</customerName>\n          <customerAddress>\n            <common:simpleAddress>\n              <common:countryCode>HU</common:countryCode>\n              <common:postalCode>1052</common:postalCode>\n              <common:city>Budapest</common:city>\n              <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>\n            </common:simpleAddress>\n          </customerAddress>\n        </customerInfo>";
        let replacement = "<customerInfo>\n          <customerVatStatus>PRIVATE_PERSON</customerVatStatus>\n        </customerInfo>";
        assert!(
            MIN_VALID.contains(original),
            "MIN_VALID must contain the full customerInfo block this pin replaces — \
             fixture drift detected"
        );
        let mutated = MIN_VALID.replace(original, replacement);
        validate_invoice_data(mutated.as_bytes())
            .expect("PRIVATE_PERSON must accept a bare customerInfo (no vatData/name/address)");
    }

    /// PR-97 / ADR-0048 §5 — symmetric NEGATIVE half of
    /// `CUSTOMER_DATA_EXPECTED`. PRIVATE_PERSON buyers MUST NOT carry
    /// a `<customerVatData>` block; NAV's business-rule layer rejects
    /// the combination server-side. The validator now models that
    /// rule locally so the trap door cannot reopen on either a future
    /// emit regression or a SPA-bypassing CLI body.
    #[test]
    fn private_person_status_forbids_customer_vat_data_in_input() {
        // Build a PRIVATE_PERSON customerInfo body that ILLEGALLY
        // carries `<customerVatData>` — the validator's new
        // ForbiddenChildUnderStatus rule must fire.
        let original = "<customerInfo>\n          <customerVatStatus>DOMESTIC</customerVatStatus>\n          <customerVatData>\n            <customerTaxNumber>\n              <common:taxpayerId>87654321</common:taxpayerId>\n              <common:vatCode>1</common:vatCode>\n              <common:countyCode>42</common:countyCode>\n            </customerTaxNumber>\n          </customerVatData>\n          <customerName>Test Customer Zrt.</customerName>\n          <customerAddress>\n            <common:simpleAddress>\n              <common:countryCode>HU</common:countryCode>\n              <common:postalCode>1052</common:postalCode>\n              <common:city>Budapest</common:city>\n              <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>\n            </common:simpleAddress>\n          </customerAddress>\n        </customerInfo>";
        // PRIVATE_PERSON + customerVatData present (the forbidden
        // combination). `<customerName>` is universally required.
        let replacement = "<customerInfo>\n          <customerVatStatus>PRIVATE_PERSON</customerVatStatus>\n          <customerVatData>\n            <customerTaxNumber>\n              <common:taxpayerId>87654321</common:taxpayerId>\n              <common:vatCode>1</common:vatCode>\n              <common:countyCode>42</common:countyCode>\n            </customerTaxNumber>\n          </customerVatData>\n          <customerName>Test Private Buyer</customerName>\n        </customerInfo>";
        assert!(
            MIN_VALID.contains(original),
            "MIN_VALID must contain the full customerInfo block this pin replaces"
        );
        let mutated = MIN_VALID.replace(original, replacement);
        let err = validate_invoice_data(mutated.as_bytes())
            .expect_err("PRIVATE_PERSON + customerVatData must loud-fail");
        match err {
            NavXsdValidationError::ForbiddenChildUnderStatus {
                parent,
                element,
                status,
            } => {
                assert_eq!(parent, "customerInfo");
                assert_eq!(element, "customerVatData");
                assert_eq!(status, "PRIVATE_PERSON");
            }
            other => panic!(
                "expected ForbiddenChildUnderStatus(customerVatData under PRIVATE_PERSON), \
                 got {other:?}"
            ),
        }
    }

    /// Session-154 (ADR-0048 amendment 2026-05-29) — defense-in-depth for
    /// NAV business rule `CUSTOMER_DATA_NOT_EXPECTED`. A PRIVATE_PERSON
    /// body carrying `<customerName>` ABORTED Ervin's invoice 31
    /// (2026-05-29) server-side; the validator now loud-fails it locally,
    /// before the wire, with `ForbiddenChildUnderStatus`.
    #[test]
    fn private_person_status_forbids_customer_name_in_input() {
        let original = "<customerInfo>\n          <customerVatStatus>DOMESTIC</customerVatStatus>\n          <customerVatData>\n            <customerTaxNumber>\n              <common:taxpayerId>87654321</common:taxpayerId>\n              <common:vatCode>1</common:vatCode>\n              <common:countyCode>42</common:countyCode>\n            </customerTaxNumber>\n          </customerVatData>\n          <customerName>Test Customer Zrt.</customerName>\n          <customerAddress>\n            <common:simpleAddress>\n              <common:countryCode>HU</common:countryCode>\n              <common:postalCode>1052</common:postalCode>\n              <common:city>Budapest</common:city>\n              <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>\n            </common:simpleAddress>\n          </customerAddress>\n        </customerInfo>";
        // PRIVATE_PERSON + customerName present (forbidden combination).
        let replacement = "<customerInfo>\n          <customerVatStatus>PRIVATE_PERSON</customerVatStatus>\n          <customerName>Test Private Buyer</customerName>\n        </customerInfo>";
        assert!(
            MIN_VALID.contains(original),
            "MIN_VALID must contain the full customerInfo block this pin replaces"
        );
        let mutated = MIN_VALID.replace(original, replacement);
        let err = validate_invoice_data(mutated.as_bytes())
            .expect_err("PRIVATE_PERSON + customerName must loud-fail");
        match err {
            NavXsdValidationError::ForbiddenChildUnderStatus {
                parent,
                element,
                status,
            } => {
                assert_eq!(parent, "customerInfo");
                assert_eq!(element, "customerName");
                assert_eq!(status, "PRIVATE_PERSON");
            }
            other => panic!(
                "expected ForbiddenChildUnderStatus(customerName under PRIVATE_PERSON), \
                 got {other:?}"
            ),
        }
    }

    /// Session-154 (ADR-0048 amendment 2026-05-29) — defense-in-depth for
    /// NAV business rule `CUSTOMER_DATA_NOT_EXPECTED`. A PRIVATE_PERSON
    /// body carrying `<customerAddress>` is rejected locally with
    /// `ForbiddenChildUnderStatus` before it can reach the NAV wire.
    #[test]
    fn private_person_status_forbids_customer_address_in_input() {
        let original = "<customerInfo>\n          <customerVatStatus>DOMESTIC</customerVatStatus>\n          <customerVatData>\n            <customerTaxNumber>\n              <common:taxpayerId>87654321</common:taxpayerId>\n              <common:vatCode>1</common:vatCode>\n              <common:countyCode>42</common:countyCode>\n            </customerTaxNumber>\n          </customerVatData>\n          <customerName>Test Customer Zrt.</customerName>\n          <customerAddress>\n            <common:simpleAddress>\n              <common:countryCode>HU</common:countryCode>\n              <common:postalCode>1052</common:postalCode>\n              <common:city>Budapest</common:city>\n              <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>\n            </common:simpleAddress>\n          </customerAddress>\n        </customerInfo>";
        // PRIVATE_PERSON + customerAddress present (forbidden combination),
        // no customerName so the name rule does not pre-empt this one.
        let replacement = "<customerInfo>\n          <customerVatStatus>PRIVATE_PERSON</customerVatStatus>\n          <customerAddress>\n            <common:simpleAddress>\n              <common:countryCode>HU</common:countryCode>\n              <common:postalCode>1052</common:postalCode>\n              <common:city>Budapest</common:city>\n              <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>\n            </common:simpleAddress>\n          </customerAddress>\n        </customerInfo>";
        assert!(
            MIN_VALID.contains(original),
            "MIN_VALID must contain the full customerInfo block this pin replaces"
        );
        let mutated = MIN_VALID.replace(original, replacement);
        let err = validate_invoice_data(mutated.as_bytes())
            .expect_err("PRIVATE_PERSON + customerAddress must loud-fail");
        match err {
            NavXsdValidationError::ForbiddenChildUnderStatus {
                parent,
                element,
                status,
            } => {
                assert_eq!(parent, "customerInfo");
                assert_eq!(element, "customerAddress");
                assert_eq!(status, "PRIVATE_PERSON");
            }
            other => panic!(
                "expected ForbiddenChildUnderStatus(customerAddress under PRIVATE_PERSON), \
                 got {other:?}"
            ),
        }
    }

    /// PR-97 / ADR-0048 §5 — guard the PR-77 hold against accidental
    /// collapse. DOMESTIC buyers still REQUIRE `<customerVatData>` AND
    /// `<customerAddress>`; pinned here so a future merge that
    /// rewrites the customer-info walker cannot silently weaken the
    /// positive half of the rule.
    #[test]
    fn domestic_status_still_requires_vat_data_and_address() {
        // Strip BOTH `<customerVatData>` and `<customerAddress>` from
        // the DOMESTIC fixture; expect MissingRequiredChild (the
        // PR-77 positive half).
        let original = "<customerVatData>\n            <customerTaxNumber>\n              <common:taxpayerId>87654321</common:taxpayerId>\n              <common:vatCode>1</common:vatCode>\n              <common:countyCode>42</common:countyCode>\n            </customerTaxNumber>\n          </customerVatData>\n          <customerName>Test Customer Zrt.</customerName>\n          <customerAddress>\n            <common:simpleAddress>\n              <common:countryCode>HU</common:countryCode>\n              <common:postalCode>1052</common:postalCode>\n              <common:city>Budapest</common:city>\n              <common:additionalAddressDetail>Váci utca 19.</common:additionalAddressDetail>\n            </common:simpleAddress>\n          </customerAddress>\n        ";
        let replacement = "<customerName>Test Customer Zrt.</customerName>\n        ";
        assert!(
            MIN_VALID.contains(original),
            "MIN_VALID must contain the customerVatData+customerAddress block"
        );
        let mutated = MIN_VALID.replace(original, replacement);
        let err = validate_invoice_data(mutated.as_bytes())
            .expect_err("DOMESTIC + missing customerVatData must loud-fail");
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "customerInfo");
                assert!(
                    expected == "customerVatData" || expected == "customerAddress",
                    "expected one of the two required children to fire first; got `{expected}`"
                );
            }
            other => panic!("expected MissingRequiredChild for DOMESTIC, got {other:?}"),
        }
    }

    /// PR-10 / ADR-0023: NAV v3.0 storno convention is negative line
    /// and summary amounts. The validator must accept a leading `-`
    /// followed by digits.
    #[test]
    fn negative_amount_is_accepted_for_storno() {
        assert!(ensure_numeric_amount("unitPrice", "-1000").is_ok());
        assert!(ensure_numeric_amount("lineNetAmount", "-2000").is_ok());
        assert!(ensure_numeric_amount("vatPercentage", "-0.27").is_ok());
    }

    /// CLAUDE.md rule 12: a leading `-` with no digits, a doubled
    /// minus, a trailing minus, or a stray `+` are all garbage. The
    /// negative-acceptance must not become a free-pass for sign-shaped
    /// nonsense.
    #[test]
    fn bare_minus_and_doubled_signs_are_loud_fail() {
        assert!(ensure_numeric_amount("unitPrice", "-").is_err());
        assert!(ensure_numeric_amount("unitPrice", "--1000").is_err());
        assert!(ensure_numeric_amount("unitPrice", "1000-").is_err());
        assert!(ensure_numeric_amount("unitPrice", "+1000").is_err());
        assert!(ensure_numeric_amount("unitPrice", "-.").is_err());
    }

    #[test]
    fn no_lines_is_loud_fail() {
        let line_open = "        <line>";
        let line_close = "        </line>";
        let pre = MIN_VALID
            .find(line_open)
            .expect("MIN_VALID contains a <line>");
        let post = MIN_VALID
            .find(line_close)
            .expect("MIN_VALID contains </line>")
            + line_close.len();
        let bad = format!("{}{}", &MIN_VALID[..pre], &MIN_VALID[post..]);
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::NoInvoiceLines => {}
            other => panic!("expected NoInvoiceLines, got {other:?}"),
        }
    }

    // ── PR-13 / ADR-0026 §4 — `<InvoiceAnnulment>` validator tests ──
    //
    // Same positive + negative shape as the `<InvoiceData>` tests
    // above. F30 closure surface: validator accepts a well-formed
    // body and loud-fails on the named divergences.

    /// The minimum valid `<InvoiceAnnulment>` per ADR-0026 §4.
    /// Updated whenever the allowlist or the emitter shifts; this
    /// fixture is the annulment validator's golden positive example.
    const MIN_VALID_ANNULMENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<InvoiceAnnulment xmlns="http://schemas.nav.gov.hu/OSA/3.0/annul" xmlns:common="http://schemas.nav.gov.hu/OSA/3.0/base">
  <annulmentReference>INV-default/00007</annulmentReference>
  <annulmentTimestamp>2026-05-21T12:00:00Z</annulmentTimestamp>
  <annulmentCode>ERRATIC_DATA</annulmentCode>
  <annulmentReason>test invoice accidentally sent to production</annulmentReason>
</InvoiceAnnulment>"#;

    #[test]
    fn minimum_valid_annulment_data_validates() {
        validate_annulment_data(MIN_VALID_ANNULMENT.as_bytes())
            .expect("the hand-rolled v3.0 minimum annulment example must validate");
    }

    #[test]
    fn empty_annulment_input_is_loud_fail() {
        let err = validate_annulment_data(b"").unwrap_err();
        match err {
            NavXsdValidationError::MalformedXml { .. } => {}
            other => panic!("expected MalformedXml for empty annulment input, got {other:?}"),
        }
    }

    #[test]
    fn wrong_annulment_root_element_is_loud_fail() {
        let xml = r#"<?xml version="1.0"?><NotAnnulment xmlns="http://schemas.nav.gov.hu/OSA/3.0/annul"/>"#;
        let err = validate_annulment_data(xml.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedRoot { actual } => {
                assert_eq!(actual, "NotAnnulment");
            }
            other => panic!("expected UnexpectedRoot, got {other:?}"),
        }
    }

    /// ADR-0026 §4: the annul namespace differs from the data
    /// namespace. A body with the InvoiceData namespace under an
    /// InvoiceAnnulment root must loud-fail — exactly the shape
    /// CLAUDE.md rule 12 names.
    #[test]
    fn wrong_annulment_root_namespace_is_loud_fail() {
        let xml = r#"<?xml version="1.0"?><InvoiceAnnulment xmlns="http://schemas.nav.gov.hu/OSA/3.0/data"/>"#;
        let err = validate_annulment_data(xml.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedRootNamespace { expected, actual } => {
                assert_eq!(expected, NAV_NS_ANNUL);
                assert_eq!(
                    actual.as_deref(),
                    Some("http://schemas.nav.gov.hu/OSA/3.0/data")
                );
            }
            other => panic!("expected UnexpectedRootNamespace, got {other:?}"),
        }
    }

    /// Missing `<annulmentCode>` — the most likely emitter-regression
    /// shape per ADR-0025 §"Adversarial review #2" (a refactor
    /// accidentally dropping the code child). Pinning this branch is
    /// the validator's load-bearing emitter-regression catch surface;
    /// CLAUDE.md rule 9 (test intent, not just behaviour).
    #[test]
    fn missing_annulment_code_is_loud_fail() {
        let bad = MIN_VALID_ANNULMENT.replace("<annulmentCode>ERRATIC_DATA</annulmentCode>", "");
        let err = validate_annulment_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "InvoiceAnnulment");
                assert_eq!(expected, "annulmentCode");
            }
            other => panic!("expected MissingRequiredChild annulmentCode, got {other:?}"),
        }
    }

    #[test]
    fn missing_annulment_reference_is_loud_fail() {
        let bad = MIN_VALID_ANNULMENT.replace(
            "<annulmentReference>INV-default/00007</annulmentReference>",
            "",
        );
        let err = validate_annulment_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "InvoiceAnnulment");
                assert_eq!(expected, "annulmentReference");
            }
            other => panic!("expected MissingRequiredChild annulmentReference, got {other:?}"),
        }
    }

    /// An out-of-order child set fires `ChildOrderViolation`. PR-13
    /// pins the document-order requirement at the validator layer;
    /// a future emitter regression that swaps `<annulmentCode>` and
    /// `<annulmentReason>` would fail this test loud at CI time.
    #[test]
    fn annulment_child_order_violation_is_loud_fail() {
        // Swap <annulmentCode> and <annulmentReason> so reason
        // precedes code — out-of-order per ORDERED_REQUIRED.
        let bad = MIN_VALID_ANNULMENT
            .replace(
                "<annulmentCode>ERRATIC_DATA</annulmentCode>\n  <annulmentReason>test invoice accidentally sent to production</annulmentReason>",
                "<annulmentReason>test invoice accidentally sent to production</annulmentReason>\n  <annulmentCode>ERRATIC_DATA</annulmentCode>",
            );
        let err = validate_annulment_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::ChildOrderViolation {
                parent,
                expected_before,
                actually_appeared_first,
            } => {
                assert_eq!(parent, "InvoiceAnnulment");
                assert_eq!(expected_before, "annulmentCode");
                assert_eq!(actually_appeared_first, "annulmentReason");
            }
            other => panic!("expected ChildOrderViolation, got {other:?}"),
        }
    }

    /// An element not in the four-child allowlist (e.g. a stray
    /// `<modificationIndex>` from a refactor that conflates
    /// annulment with chain operations) fires `UnexpectedElement`.
    #[test]
    fn unexpected_element_in_annulment_is_loud_fail() {
        let bad = MIN_VALID_ANNULMENT.replace(
            "<annulmentReason>",
            "<modificationIndex>1</modificationIndex><annulmentReason>",
        );
        let err = validate_annulment_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedElement { parent, element } => {
                assert_eq!(parent, "InvoiceAnnulment");
                assert_eq!(element, "modificationIndex");
            }
            other => panic!("expected UnexpectedElement modificationIndex, got {other:?}"),
        }
    }

    /// ADR-0026 §4 explicitly does NOT enforce the four-code closed
    /// set at the validator layer — the CLI's clap ValueEnum is the
    /// loud-fail boundary for unknown codes. The validator accepts
    /// any text content in `<annulmentCode>` (a future testbed-
    /// driven amendment may tighten this; out of scope for PR-13).
    #[test]
    fn annulment_code_text_is_not_enum_checked_by_validator() {
        // An off-allowlist value like `SOMETHING_ELSE` must still
        // validate at the schema-shape layer — the closed-set check
        // lives at the CLI boundary.
        let body = MIN_VALID_ANNULMENT.replace(
            "<annulmentCode>ERRATIC_DATA</annulmentCode>",
            "<annulmentCode>SOMETHING_ELSE</annulmentCode>",
        );
        validate_annulment_data(body.as_bytes())
            .expect("validator must not enforce annulmentCode enumeration");
    }

    /// PR-13 / F30 closure pin. The two `validate_*` functions
    /// share the same error type but operate on disjoint root
    /// elements — an InvoiceData body must NOT validate against
    /// `validate_annulment_data`. ADR-0026 §"Adversarial review #4"
    /// surfaced this directly; this test is the load-bearing
    /// type-confusion guardrail.
    #[test]
    fn validate_annulment_data_rejects_invoice_data_body() {
        // Use the canonical InvoiceData fixture above — passing it
        // to validate_annulment_data must surface UnexpectedRoot.
        let err = validate_annulment_data(MIN_VALID.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedRoot { actual } => {
                assert_eq!(actual, "InvoiceData");
            }
            other => panic!(
                "expected UnexpectedRoot when InvoiceData is fed to validate_annulment_data, got {other:?}"
            ),
        }
    }

    /// Symmetric reverse pin: an InvoiceAnnulment body must NOT
    /// validate against `validate_invoice_data`. Defence-in-depth
    /// on the type-confusion concern above.
    #[test]
    fn validate_invoice_data_rejects_annulment_body() {
        let err = validate_invoice_data(MIN_VALID_ANNULMENT.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::UnexpectedRoot { actual } => {
                assert_eq!(actual, "InvoiceAnnulment");
            }
            other => panic!(
                "expected UnexpectedRoot when InvoiceAnnulment is fed to validate_invoice_data, got {other:?}"
            ),
        }
    }

    /// PR-66 / session-87 — the supplier tax-number's three structured
    /// children must carry the `common:` namespace prefix. A bare-
    /// prefix child (the regression class session 87 closes on the
    /// emit side) must loud-fail with the new variant, naming the
    /// dropped prefix so the operator's next step is to fix the
    /// emitter rather than relax the validator.
    #[test]
    fn supplier_tax_number_bare_prefix_child_is_loud_fail() {
        let bad = MIN_VALID.replace(
            "<common:taxpayerId>12345678</common:taxpayerId>",
            "<taxpayerId>12345678</taxpayerId>",
        );
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::WrongChildNamespacePrefix {
                parent,
                element,
                expected_prefix,
                actual_prefix,
            } => {
                assert_eq!(parent, "supplierTaxNumber");
                assert_eq!(element, "taxpayerId");
                assert_eq!(expected_prefix, "common");
                assert_eq!(actual_prefix, "");
            }
            other => panic!(
                "expected WrongChildNamespacePrefix for bare-prefix supplier taxpayerId, got {other:?}"
            ),
        }
    }

    /// PR-66 / session-87 — symmetric pin on the customer side. The
    /// emit code in `nav_xml::write_customer` is a mirror of
    /// `write_supplier`; the validator must enforce the prefix on
    /// both sides so a regression on either side surfaces here.
    #[test]
    fn customer_tax_number_bare_prefix_child_is_loud_fail() {
        let bad = MIN_VALID.replace(
            "<common:taxpayerId>87654321</common:taxpayerId>",
            "<taxpayerId>87654321</taxpayerId>",
        );
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::WrongChildNamespacePrefix {
                parent,
                element,
                expected_prefix,
                actual_prefix,
            } => {
                assert_eq!(parent, "customerTaxNumber");
                assert_eq!(element, "taxpayerId");
                assert_eq!(expected_prefix, "common");
                assert_eq!(actual_prefix, "");
            }
            other => panic!(
                "expected WrongChildNamespacePrefix for bare-prefix customer taxpayerId, got {other:?}"
            ),
        }
    }

    /// PR-66 / session-87 — a non-`common` prefix (e.g. a typo
    /// `base:taxpayerId` from a future contributor confusing the
    /// declared `xmlns:common="...base"` binding) must also fail
    /// loud. The `actual_prefix` field surfaces exactly what was on
    /// the wire so the error message points at the typo.
    #[test]
    fn supplier_tax_number_wrong_prefix_is_loud_fail() {
        let bad = MIN_VALID.replace(
            "<common:vatCode>1</common:vatCode>",
            "<base:vatCode>1</base:vatCode>",
        );
        let err = validate_invoice_data(bad.as_bytes()).unwrap_err();
        match err {
            NavXsdValidationError::WrongChildNamespacePrefix {
                parent,
                element,
                expected_prefix,
                actual_prefix,
            } => {
                assert_eq!(parent, "supplierTaxNumber");
                assert_eq!(element, "vatCode");
                assert_eq!(expected_prefix, "common");
                assert_eq!(actual_prefix, "base");
            }
            other => panic!(
                "expected WrongChildNamespacePrefix for wrong-prefix supplier vatCode, got {other:?}"
            ),
        }
    }

    /// S159 — positive case: `<unitOfMeasure>OWN</...>` paired with
    /// `<unitOfMeasureOwn>{label}</...>` (the fuel-measure `liter@15C`)
    /// validates. This is the wire shape the emitter produces for a
    /// `ProductUnit::Own` line; the validator must accept it.
    #[test]
    fn unit_of_measure_own_with_free_text_validates() {
        let original = "<unitOfMeasure>PIECE</unitOfMeasure>";
        let replacement =
            "<unitOfMeasure>OWN</unitOfMeasure>\n          <unitOfMeasureOwn>liter@15C</unitOfMeasureOwn>";
        assert!(
            MIN_VALID.contains(original),
            "MIN_VALID must contain the PIECE unitOfMeasure this pin replaces — fixture drift"
        );
        let mutated = MIN_VALID.replace(original, replacement);
        validate_invoice_data(mutated.as_bytes())
            .expect("OWN + unitOfMeasureOwn is a valid NAV LineType shape");
    }

    /// S159 — POSITIVE half of the pair rule: `<unitOfMeasure>OWN</...>`
    /// REQUIRES the free-text `<unitOfMeasureOwn>` sibling. An `OWN`
    /// token with no free-text element loud-fails as a missing child so
    /// the gap surfaces at the ADR-0022 invariant check, not as a
    /// NAV-side rejection.
    #[test]
    fn unit_of_measure_own_without_free_text_is_rejected() {
        let original = "<unitOfMeasure>PIECE</unitOfMeasure>";
        let replacement = "<unitOfMeasure>OWN</unitOfMeasure>";
        let mutated = MIN_VALID.replace(original, replacement);
        let err = validate_invoice_data(mutated.as_bytes())
            .expect_err("OWN without <unitOfMeasureOwn> must loud-fail");
        match err {
            NavXsdValidationError::MissingRequiredChild { parent, expected } => {
                assert_eq!(parent, "line");
                assert_eq!(expected, "unitOfMeasureOwn");
            }
            other => panic!("expected MissingRequiredChild unitOfMeasureOwn, got {other:?}"),
        }
    }

    /// S159 — NEGATIVE half of the pair rule: `<unitOfMeasureOwn>` is
    /// FORBIDDEN when `<unitOfMeasure>` is a closed-vocab token (here
    /// `PIECE`). NAV's LineType permits the free-text element only
    /// alongside `OWN`.
    #[test]
    fn unit_of_measure_free_text_without_own_is_rejected() {
        let original = "<unitOfMeasure>PIECE</unitOfMeasure>";
        let replacement =
            "<unitOfMeasure>PIECE</unitOfMeasure>\n          <unitOfMeasureOwn>liter@15C</unitOfMeasureOwn>";
        let mutated = MIN_VALID.replace(original, replacement);
        let err = validate_invoice_data(mutated.as_bytes())
            .expect_err("PIECE + <unitOfMeasureOwn> must loud-fail");
        match err {
            NavXsdValidationError::ForbiddenUnitOfMeasureOwn { unit_of_measure } => {
                assert_eq!(unit_of_measure, "PIECE");
            }
            other => panic!("expected ForbiddenUnitOfMeasureOwn, got {other:?}"),
        }
    }

    #[test]
    fn error_variants_have_distinct_display() {
        let v = vec![
            NavXsdValidationError::MalformedXml {
                position: 0,
                message: "x".into(),
            },
            NavXsdValidationError::UnexpectedRoot { actual: "x".into() },
            NavXsdValidationError::UnexpectedRootNamespace {
                expected: "x",
                actual: None,
            },
            NavXsdValidationError::MissingRequiredChild {
                parent: "x",
                expected: "y",
            },
            NavXsdValidationError::UnexpectedElement {
                parent: "x",
                element: "y".into(),
            },
            NavXsdValidationError::ChildOrderViolation {
                parent: "x",
                expected_before: "y",
                actually_appeared_first: "z".into(),
            },
            NavXsdValidationError::CardinalityExceeded {
                parent: "x",
                element: "y",
                max: 1,
                actual: 2,
            },
            NavXsdValidationError::MalformedDate {
                field: "x",
                actual: "y".into(),
            },
            NavXsdValidationError::NonNumericAmount {
                field: "x",
                actual: "y".into(),
            },
            NavXsdValidationError::NoInvoiceLines,
            NavXsdValidationError::WrongChildNamespacePrefix {
                parent: "x",
                element: "y",
                expected_prefix: "common",
                actual_prefix: "z".into(),
            },
            NavXsdValidationError::ForbiddenChildUnderStatus {
                parent: "x",
                element: "y",
                status: "PRIVATE_PERSON",
            },
            NavXsdValidationError::ForbiddenUnitOfMeasureOwn {
                unit_of_measure: "PIECE".into(),
            },
        ];

        for i in 0..v.len() {
            for j in (i + 1)..v.len() {
                let a = format!("{}", v[i]);
                let b = format!("{}", v[j]);
                assert_ne!(
                    a, b,
                    "variants at index {i} and {j} render to the same Display; \
                     ADR-0022 §Adversarial review bullet 4 requires pairwise-distinct"
                );
            }
        }
    }
}
