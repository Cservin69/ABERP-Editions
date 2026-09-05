//! The **Defense demo seed** — one coherent aerospace job, written into the
//! bundled `demo` tenant so the Defense screens have something to render.
//!
//! # Why this exists
//!
//! The pre-existing bundled seed (`serve::seed_demo_sample_data`, S434) writes
//! three partners and two products. That is a *Portable* convenience: it makes
//! a NAV-off international install look used. It says nothing about the
//! Defense line, so a fresh Defense tenant boots with every wall-TV counter at
//! zero and the pricing / PO / AVL / QA / work-order / inventory / NCR screens
//! empty. A demo then has nothing to walk.
//!
//! This module writes the *narrative* instead of a table dump: **one part
//! flows CAD → quote → PO → make → inspect → ship-gate**, and every screen in
//! the walkthrough is a different window onto that same job. The ids line up
//! on purpose — the heat lot on the stock row is the heat lot stamped into the
//! part UIDs, which is the heat lot the traceability report resolves, whose
//! grade names the quote that spawned the work order.
//!
//! # How it is loaded
//!
//! Exactly like the seed it complements: **code, run against the demo tenant's
//! own DuckDB, idempotent, and reachable only through a demo-scoped entry
//! point.** Nothing here lands in a prod path, a committed universe document,
//! or a config file a gate would induct over. The entry point is the
//! `aberp demo-seed` subcommand (see [`run`]); `run/run_defense_demo.sh` is a
//! one-command wrapper that seeds and then launches the desktop shell.
//!
//! # Guards ([[trust-code-not-operator]])
//!
//! 1. **Slug** — only [`tenant_registry::DEMO_SLUG`] is seedable. Not a
//!    prefix match, not an operator-supplied path: the DB path is *derived*
//!    from the slug through the edition-locked resolver (FOUNDATION §5), so
//!    this command physically cannot write into `defense`, `prod`, or the
//!    sibling edition's root.
//! 2. **Emptiness** — a tenant that already has partners is left alone and
//!    the command exits 0 (`already_seeded`). Re-running is free.
//! 3. **NAV** — the `demo` registry row is written by
//!    [`tenant_registry::TenantRegistry::add_demo`], which is NAV-**off**.
//!    That matters twice: the demo can never submit to real NAV even from a
//!    `--features production` (Defense) binary, and the boot path skips the
//!    keychain + §169 seller gate entirely, so the tenant boots straight to
//!    `Ready` instead of into the setup wizard.
//!
//! # Edition
//!
//! Every table this module writes exists in BOTH editions — the ADR-0199
//! edition gate covers the QC *report* layer, not the QC measurement surface,
//! and not AVL / purchasing / marking / dispatch. So the seed runs in a
//! Portable build too (which is what keeps it inside `cargo test`); it is the
//! *binary* that decides which screens then light up. The demo is designed
//! against the Defense build.
//!
//! # What it deliberately does NOT seed
//!
//! No issued invoice. Issuing one needs the whole pipeline (seller.toml
//! identity, MNB rates, gap-free numbering, NAV XML render) and burns a real
//! sequence number; the honest demonstration is the operator clicking Issue on
//! a seeded draft during the demo. Same call the S434 seed made, same reason.

use anyhow::{bail, Context, Result};
use duckdb::params;
use rust_decimal::Decimal;
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime};
use ulid::Ulid;

use aberp_audit_ledger::{Actor, BinaryHash, EventKind, LedgerMeta, TenantId};
use aberp_compliance::avl::{ApprovalCategory, ApprovedStatus};
use aberp_db::HandleArc;
use aberp_quote_engine::{
    Feature, FeatureGraph, FeatureType, HoleEndCondition, LocatedHole, StockForm, ToleranceSpec,
};

use crate::{
    avl_vendors, invoice_draft, material_inventory, part_marking, partners, products, purchasing,
    quality, quote_pricing_jobs as jobs, quote_pricing_pipeline, tenant_registry,
};

/// The operator login every seeded audit row is attributed to. Deliberately
/// not a human name: a forensic walk over the demo ledger must be able to tell
/// seeded rows from rows an operator made during the demo.
pub const DEMO_OPERATOR: &str = "demo-seed";

/// Display name for the seeded end customer — the aerospace prime the whole
/// narrative hangs off.
const CUSTOMER_DISPLAY: &str = "Meridian Aerostructures Kft.";

/// Material grades. All three are rows in the boot-seeded
/// `quoting_materials` catalogue, so the engine can actually price against
/// them (an off-catalogue grade is `QuoteError::MaterialNotInCatalogue`).
const GRADE_TITANIUM: &str = "Ti-6Al-4V";
const GRADE_ALUMINIUM: &str = "7075-T651";
const GRADE_STAINLESS: &str = "316";

/// Heat lots. `[A-Za-z0-9-]` only — `aberp_compliance::lot_heat` refuses
/// anything else.
const HEAT_LOT_TI: &str = "HT-2026-TI-88431";
const HEAT_LOT_AL: &str = "HT-2026-AL-55210";

/// What the seed wrote, for the operator-facing summary the CLI prints.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DemoSeedSummary {
    /// `true` when the tenant already carried data and nothing was written.
    pub already_seeded: bool,
    pub partners: usize,
    pub products: usize,
    pub avl_vendors: usize,
    pub material_balances: usize,
    pub pricing_jobs: usize,
    pub intake_rows: usize,
    pub inspection_plans: usize,
    pub work_orders: usize,
    pub part_marks: usize,
    pub qc_inspections: usize,
    pub purchase_orders: usize,
    pub ncrs: usize,
    pub dispatches: usize,
    pub invoice_drafts: usize,
}

/// CLI entry point for `aberp demo-seed`.
///
/// Registers the `demo` tenant NAV-off (idempotent), opens its edition-locked
/// database through the ONE shared [`aberp_db::Handle`] with every tenant
/// schema ensured, and seeds the narrative. Prints a human summary + the
/// launch command on success.
pub fn run(args: &crate::cli::DemoSeedArgs) -> Result<()> {
    let slug = args.tenant.trim();
    refuse_non_demo_slug(slug)?;

    // 1 — registry row FIRST, so the very first boot of this tenant already
    //     finds a NAV-off row. Without it `serve`'s registry self-heal
    //     (`ensure_boot_tenant_registered`) would add the slug NAV-*on*, and
    //     the second boot would demand NAV credentials from the OS keychain.
    ensure_demo_registered()?;

    let db_path = tenant_registry::tenant_db_path(slug)
        .context("resolve the demo tenant's edition-locked DB path")?;
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create demo tenant dir {}", parent.display()))?;
    }
    let tenant = TenantId::new(slug.to_string())
        .ok_or_else(|| anyhow::anyhow!("{slug:?} is not a valid tenant id"))?;

    // 2 — the shared Handle. `open_tenant_handle` runs `ensure_all_tenant_schemas`
    //     under the writer, so every table this module touches exists.
    let db = crate::serve::open_tenant_handle(&db_path, tenant.clone())
        .context("open the demo tenant's shared DuckDB handle")?;
    let binary_hash = crate::binary_hash::compute().context("compute binary hash for demo seed")?;

    let summary = seed(&db_path, &db, &tenant, binary_hash)?;

    // The guard drop already ran the lockstep mirror sync; ask for the
    // durable ack explicitly so a demo laptop that loses power between the
    // seed and the launch still boots into a seeded tenant.
    db.durable_ack()
        .map_err(|e| anyhow::anyhow!("durable ack after demo seed: {e}"))?;

    print_summary(&summary, &db_path);
    Ok(())
}

/// The slug guard. Extracted so it is unit-testable without a `$HOME`, and
/// so the refusal reads as one named rule rather than an `if` buried in the
/// command body.
///
/// An exact match, deliberately — not a `demo-` prefix. A prefix rule invites
/// `demo-defense` or `demo-prod`, and the whole point is that this command
/// has exactly one destination.
fn refuse_non_demo_slug(slug: &str) -> Result<()> {
    if slug != tenant_registry::DEMO_SLUG {
        bail!(
            "demo-seed refuses tenant {slug:?} — only the bundled {:?} tenant is seedable. \
             That is deliberate: the seed writes sample vendors, purchase orders and \
             non-conformances, and a real tenant must never receive them.",
            tenant_registry::DEMO_SLUG
        );
    }
    Ok(())
}

/// Append the `demo` row to `tenants.toml` if absent. NAV-off + state `Demo`
/// via the existing [`tenant_registry::TenantRegistry::add_demo`] — the same
/// call `serve::bootstrap_demo_tenant` makes on a fresh install, so a machine
/// that already ran that path is a no-op here.
fn ensure_demo_registered() -> Result<()> {
    let path = tenant_registry::registry_path()?;
    let mut reg = tenant_registry::TenantRegistry::read_from(&path)?;
    if reg.find(tenant_registry::DEMO_SLUG).is_some() {
        return Ok(());
    }
    reg.add_demo(OffsetDateTime::now_utc())
        .map_err(|e| anyhow::anyhow!("register the demo tenant: {e}"))?;
    reg.write_to(&path)?;
    Ok(())
}

fn print_summary(s: &DemoSeedSummary, db_path: &std::path::Path) {
    if s.already_seeded {
        println!(
            "demo tenant already carries data at {} — nothing written (the seed is idempotent).",
            db_path.display()
        );
        return;
    }
    println!("Seeded the Defense demo tenant at {}:", db_path.display());
    println!("  partners          {}", s.partners);
    println!("  products          {}", s.products);
    println!("  AVL vendors       {}", s.avl_vendors);
    println!("  purchase orders   {}", s.purchase_orders);
    println!("  material balances {}", s.material_balances);
    println!("  pricing jobs      {}", s.pricing_jobs);
    println!("  quote-intake rows {}", s.intake_rows);
    println!("  inspection plans  {}", s.inspection_plans);
    println!("  work orders       {}", s.work_orders);
    println!("  part UIDs minted  {}", s.part_marks);
    println!("  QC inspections    {}", s.qc_inspections);
    println!("  NCRs              {}", s.ncrs);
    println!("  dispatches        {}", s.dispatches);
    println!("  invoice drafts    {}", s.invoice_drafts);
    println!();
    println!("Launch the seeded demo:  ./run/run_defense_demo.sh");
}

// ── The narrative ───────────────────────────────────────────────────
//
// Ids minted by the earlier steps that later steps need. Threaded
// explicitly rather than re-queried, so the coupling between the acts of
// the story is visible in the type rather than implied by a WHERE clause.
#[derive(Debug, Default)]
struct Cast {
    customer_partner_id: String,
    titanium_vendor_id: String,
    aluminium_vendor_id: String,
    /// The vendor that is SUSPENDED on the AVL — the live refusal an
    /// operator can trigger on stage by trying to raise a PO against it.
    suspended_vendor_id: String,
    bracket_product_id: String,
    manifold_product_id: String,
    ti_bar_product_id: String,
    al_plate_product_id: String,
    /// The two priced quotes, in narrative order (bracket, manifold).
    bracket_quote_id: String,
    manifold_quote_id: String,
    /// The four inspection-plan ids for the bracket, drawing-balloon order.
    bracket_plan_ids: Vec<String>,
    /// (wo_id, wo_number) for the three work orders.
    wo_bracket_a: String,
    wo_manifold: String,
    wo_bracket_b: String,
}

/// Seed the whole narrative into an already-migrated demo tenant.
///
/// Idempotent by the partner table: a tenant that already has one partner is
/// left untouched. Split out of [`run`] so the integration test can drive it
/// against a scratch DB without a `tenants.toml`.
pub fn seed(
    db_path: &std::path::Path,
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
) -> Result<DemoSeedSummary> {
    {
        let conn = db
            .read()
            .map_err(|e| anyhow::anyhow!("read the demo tenant for the idempotency probe: {e}"))?;
        let existing = partners::list_partners(&conn, tenant.as_str(), None)
            .context("list demo partners for the idempotency probe")?;
        if !existing.is_empty() {
            return Ok(DemoSeedSummary {
                already_seeded: true,
                ..Default::default()
            });
        }
    }

    let mut s = DemoSeedSummary::default();
    let mut cast = Cast::default();

    ensure_quoting_catalogues(db, tenant, binary_hash)?;
    seed_master_data(db, tenant, &mut cast, &mut s)?;
    seed_avl(db, tenant, binary_hash, &mut cast, &mut s)?;
    seed_purchasing(db_path, db, tenant, binary_hash, &cast, &mut s)?;
    seed_material_stock(db_path, db, tenant, binary_hash, &cast, &mut s)?;
    seed_quotes(db, tenant, &mut cast, &mut s)?;
    seed_inspection_plans(db, tenant, &mut cast, &mut s)?;
    seed_work_orders(db, tenant, binary_hash, &mut cast, &mut s)?;
    seed_part_marks(db, tenant, binary_hash, &cast, &mut s)?;
    seed_qc_and_ncr(db_path, db, tenant, binary_hash, &cast, &mut s)?;
    seed_dispatch_and_drafts(db, tenant, binary_hash, &cast, &mut s)?;

    Ok(s)
}

/// Lay down the pricing catalogues the engine needs.
///
/// `serve::open_tenant_handle` runs `ensure_all_tenant_schemas`, which creates
/// these tables but does NOT seed them — the boot path calls the seeders
/// separately (`serve::run`'s quoting-migration blocks). `aberp demo-seed`
/// runs BEFORE this tenant has ever booted, so an unseeded
/// `quoting_materials` would make the very first quote fail with
/// `MaterialNotInCatalogue`. Every seeder here is the project's own
/// insert-if-absent one, so a later boot re-running them is a no-op and an
/// operator who edited a row is never overruled.
fn ensure_quoting_catalogues(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
) -> Result<()> {
    let t = tenant.as_str();
    let ledger_meta = meta(tenant, binary_hash);
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the quoting catalogues: {e}"))?;
    crate::quoting_materials::seed_if_empty(&mut guard, t)
        .context("seed the material catalogue for the demo tenant")?;
    crate::quoting_tunables::ensure_schema(&mut guard, t)
        .context("seed the quoting tunables for the demo tenant")?;
    crate::quoting_machine_rates::seed_machine_rates_if_absent(&guard, t)
        .context("seed the machine-rate catalogue for the demo tenant")?;
    crate::quoting_gear_processes::seed_gear_processes_if_absent(&guard, t)
        .context("seed the gear-process catalogue for the demo tenant")?;
    crate::quoting_tolerance_cost_rates::seed_tolerance_cost_rates_if_absent(
        &mut guard,
        &ledger_meta,
        t,
    )
    .context("seed the tolerance cost-rate catalogue for the demo tenant")?;
    // Complexity rules are operator-configured tunables with NO production
    // default (unlike the material / machine-rate / gear / tolerance
    // catalogues above, which every tenant boots with). So the boot path
    // leaves `quoting_complexity_rules` empty, and the demo's own
    // FeatureGraphs (pocket / hole / thread / undercut_5axis / surface / …)
    // would fail the real engine with `NoComplexityRuleForFeature`. Seed a
    // demo-credible full grid here — demo-tenant-only; the shared boot path
    // is untouched, so a real tenant still starts with an empty catalogue it
    // configures itself.
    seed_demo_complexity_rules(&mut guard, &ledger_meta, t)
        .context("seed the complexity-rule catalogue for the demo tenant")?;
    Ok(())
}

/// Insert-if-absent complexity rules covering every feature type the demo's
/// FeatureGraphs use, across all five size buckets, so the real pricing
/// engine has a rule for every `(feature_type, size_bucket)` it looks up.
/// One rule per `(feature_type, size_bucket)` with `count_min = 1` /
/// `count_max = NULL` covers every positive count. Values are demo-credible
/// (a 5-axis undercut costs far more than a drilled hole), not researched
/// production defaults — the point is a coherent, non-zero engine price, not
/// a shop's real rate card. Idempotent: a pre-existing `(ft, sb, count_min)`
/// is skipped, so a re-run (or a later boot) is a no-op.
fn seed_demo_complexity_rules(
    guard: &mut duckdb::Connection,
    ledger_meta: &LedgerMeta,
    tenant: &str,
) -> Result<()> {
    use crate::quoting_tunables::{
        create_complexity_rule, ComplexityRuleInputs, TunableWriteError,
    };
    // (feature_type db-string, base minutes at bucket M, setup-penalty minutes).
    const FEATURES: &[(&str, f64, f64)] = &[
        ("pocket", 6.0, 12.0),
        ("hole", 1.5, 4.0),
        ("slot", 3.0, 8.0),
        ("thread", 2.5, 6.0),
        ("undercut_5axis", 15.0, 35.0),
        ("thin_wall", 8.0, 18.0),
        ("surface", 4.0, 10.0),
        ("engraving", 2.0, 5.0),
    ];
    // Size scales the per-feature machining time; setup is size-independent.
    const BUCKETS: &[(&str, f64)] = &[
        ("XS", 0.5),
        ("S", 0.75),
        ("M", 1.0),
        ("L", 1.5),
        ("XL", 2.2),
    ];
    for &(ft, base_m, setup) in FEATURES {
        for &(sb, factor) in BUCKETS {
            let inputs = ComplexityRuleInputs {
                feature_type: ft.to_string(),
                size_bucket: sb.to_string(),
                count_min: 1,
                count_max: None,
                base_time_minutes: base_m * factor,
                multiplier: 1.0,
                setup_penalty_minutes: setup,
                notes: Some("demo seed catalogue".to_string()),
            };
            match create_complexity_rule(guard, ledger_meta, DEMO_OPERATOR, tenant, &inputs) {
                Ok(_) => {}
                // Already present (a re-run / prior boot) — insert-if-absent.
                Err(TunableWriteError::Conflict(_)) => {}
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "create demo complexity rule ({ft}, {sb}): {e:?}"
                    ))
                }
            }
        }
    }
    Ok(())
}

fn meta(tenant: &TenantId, binary_hash: BinaryHash) -> LedgerMeta {
    LedgerMeta::new(tenant.clone(), binary_hash)
}

fn actor() -> Actor {
    Actor::from_local_cli(Ulid::new().to_string(), DEMO_OPERATOR)
}

fn iso(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

/// Days ago, as an RFC-3339 instant. The seeded history spans ~5 weeks so the
/// "since" columns on every list read like a shop that has been running, not
/// like a bulk import stamped at one instant.
fn days_ago(n: i64) -> OffsetDateTime {
    OffsetDateTime::now_utc() - Duration::days(n)
}

// ── Act 1 · master data ─────────────────────────────────────────────

/// Partners, products and the BOM that ties a finished good to the bar stock
/// a work-order Release will consume.
fn seed_master_data(
    db: &HandleArc,
    tenant: &TenantId,
    cast: &mut Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let t = tenant.as_str();
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for demo master data: {e}"))?;

    let customer = |display: &str, legal: &str, ctype: partners::CustomerType| {
        partners::PartnerInputs {
            display_name: display.to_string(),
            legal_name: legal.to_string(),
            kind: partners::PartnerKind::Customer,
            // The demo tenant is the NAV-off international sandbox, so the HU
            // §169 ADÓSZÁM requirement a `Domestic` status carries does not
            // apply. Same call the S434 seed made, same reason.
            customer_vat_status: crate::nav_xml::CustomerVatStatus::PrivatePerson,
            customer_type: ctype,
            tax_number: None,
            eu_vat_number: None,
            address_street: Some("Repülőtéri út 12.".to_string()),
            address_postal_code: Some("1185".to_string()),
            address_city: Some("Budapest".to_string()),
            address_country: Some("HU".to_string()),
            bank_account: None,
            contact_email: Some("supplier.quality@meridian-aero.example".to_string()),
            contact_phone: None,
        }
    };
    let supplier =
        |display: &str, legal: &str, country: &str, email: &str| partners::PartnerInputs {
            display_name: display.to_string(),
            legal_name: legal.to_string(),
            kind: partners::PartnerKind::Supplier,
            customer_vat_status: crate::nav_xml::CustomerVatStatus::PrivatePerson,
            customer_type: partners::CustomerType::Unset,
            tax_number: None,
            eu_vat_number: None,
            address_street: None,
            address_postal_code: None,
            address_city: None,
            address_country: Some(country.to_string()),
            bank_account: None,
            contact_email: Some(email.to_string()),
            contact_phone: None,
        };

    let inputs = customer(
        CUSTOMER_DISPLAY,
        "Meridian Aerostructures Korlátolt Felelősségű Társaság",
        partners::CustomerType::Aerospace,
    );
    cast.customer_partner_id = partners::create_partner(&guard, t, &inputs)
        .context("seed the demo customer")?
        .id;
    s.partners += 1;

    for (display, legal, country, email, slot) in [
        (
            "TitaniumSource Europe GmbH",
            "TitaniumSource Europe GmbH",
            "DE",
            "orders@titaniumsource.example",
            0u8,
        ),
        (
            "Danube Metals Zrt.",
            "Danube Metals Zártkörűen Működő Részvénytársaság",
            "HU",
            "sales@danubemetals.example",
            1,
        ),
        (
            "Balaton Heat Treat Kft.",
            "Balaton Heat Treat Kft.",
            "HU",
            "info@balatonht.example",
            2,
        ),
    ] {
        let p = partners::create_partner(&guard, t, &supplier(display, legal, country, email))
            .with_context(|| format!("seed demo supplier {display}"))?;
        match slot {
            0 => cast.titanium_vendor_id = p.id,
            1 => cast.aluminium_vendor_id = p.id,
            // Balaton's only job is to be the SUSPENDED row on the AVL
            // screen: an operator can try to raise a PO against it, live,
            // and watch the gate refuse.
            _ => cast.suspended_vendor_id = p.id,
        }
        s.partners += 1;
    }
    let product = |name: &str, unit: &str, price_minor: i64| products::ProductInputs {
        name: name.to_string(),
        unit: products::ProductUnit::Own(unit.to_string()),
        currency: aberp_billing::Currency::Eur,
        unit_price_minor: price_minor,
    };
    cast.bracket_product_id = products::create_product(
        &guard,
        t,
        &product(
            "LG-BRKT-4412 landing-gear bracket (Ti-6Al-4V)",
            "pcs",
            48_500,
        ),
    )
    .context("seed the bracket product")?
    .id;
    cast.manifold_product_id = products::create_product(
        &guard,
        t,
        &product("HYD-MAN-2207 hydraulic manifold (7075-T651)", "pcs", 21_900),
    )
    .context("seed the manifold product")?
    .id;
    cast.ti_bar_product_id = products::create_product(
        &guard,
        t,
        &product("Ti-6Al-4V bar Ø60 × 3000 (raw stock)", "pcs", 39_000),
    )
    .context("seed the titanium bar stock product")?
    .id;
    cast.al_plate_product_id = products::create_product(
        &guard,
        t,
        &product("7075-T651 plate 60 × 200 × 1000 (raw stock)", "pcs", 12_400),
    )
    .context("seed the aluminium plate stock product")?
    .id;
    s.products += 4;

    // ADR-0199 — QC-report readiness for the aerospace customer.
    //  • Default the customer's QC-report template to AS9102 Rev C, so an
    //    AS9102 First Article (FAIR) drafts against this delivery with NO
    //    per-report template override (the house `aben_standard` default does
    //    not produce a FAIR, so without this the demo would need the operator
    //    to pick the template by hand).
    //  • Give the bracket a drawing number + revision, so the FAIR / CoC names
    //    the drawing it was inspected against instead of printing a blank.
    partners::set_qc_report_template(
        &guard,
        t,
        &cast.customer_partner_id,
        Some(aberp_qa::QcReportTemplate::As9102RevC),
    )
    .context("set the demo customer's default QC-report template")?;
    aberp_qa::ensure_schema(&guard)
        .map_err(|e| anyhow::anyhow!("ensure qa/qc schema for the drawing ref: {e}"))?;
    aberp_qa::record_drawing_ref(
        &mut guard,
        t,
        aberp_qa::NewPartDrawingRef {
            product_id: cast.bracket_product_id.clone(),
            drawing_number: "LG-BRKT-4412".to_string(),
            drawing_rev: "C".to_string(),
        },
        DEMO_OPERATOR,
        OffsetDateTime::now_utc(),
    )
    .context("seed the bracket drawing reference")?;

    // Re-order points. `products::create_product` pre-dates the ADR-0061
    // cache columns, so the SPA's own inventory form is what normally writes
    // these; the seed writes them the same way (a plain UPDATE on the
    // already-migrated columns) through the shared writer.
    for (id, min_stock, bin) in [
        (&cast.bracket_product_id, "0", "FG-A1"),
        (&cast.manifold_product_id, "0", "FG-A2"),
        (&cast.ti_bar_product_id, "8", "RAW-T3"),
        (&cast.al_plate_product_id, "4", "RAW-A1"),
    ] {
        guard
            .execute(
                "UPDATE products SET min_stock = CAST(? AS DECIMAL(18,6)), bin_location = ? \
                 WHERE tenant_id = ? AND id = ?",
                params![min_stock, bin, t, id],
            )
            .context("stamp min_stock / bin_location on a seeded product")?;
    }

    // The BOM. One bar per bracket, one plate per two manifolds — so a WO
    // Release emits real `bom_consumption` stock movements and the raw-stock
    // rows actually move on the inventory screen.
    {
        let tx = guard
            .transaction()
            .context("begin the demo BOM transaction")?;
        aberp_work_orders::replace_bom_for_product(
            &tx,
            t,
            &cast.bracket_product_id,
            &[aberp_work_orders::BomLineInput {
                component_id: cast.ti_bar_product_id.clone(),
                qty_per_unit: Decimal::new(1, 0),
            }],
        )
        .map_err(|e| anyhow::anyhow!("author the bracket BOM: {e}"))?;
        aberp_work_orders::replace_bom_for_product(
            &tx,
            t,
            &cast.manifold_product_id,
            &[aberp_work_orders::BomLineInput {
                component_id: cast.al_plate_product_id.clone(),
                qty_per_unit: Decimal::new(2, 1),
            }],
        )
        .map_err(|e| anyhow::anyhow!("author the manifold BOM: {e}"))?;
        tx.commit().context("commit the demo BOM transaction")?;
    }

    Ok(())
}

// ── Act 2 · the approved vendor list ────────────────────────────────

/// Three AVL rows, chosen so the screen shows all three things it can say:
/// an approved vendor in date, an approved vendor whose re-screening window
/// has **lapsed** (the overdue chip), and a **suspended** one that the PO
/// gate refuses.
fn seed_avl(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    cast: &mut Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let t = tenant.as_str();
    let rows: [(&str, ApprovedStatus, &[ApprovalCategory], i64, &str); 3] = [
        (
            cast.titanium_vendor_id.as_str(),
            ApprovedStatus::Approved,
            &[
                ApprovalCategory::Aerospace,
                ApprovalCategory::Defense,
                ApprovalCategory::Ear99,
            ],
            // Re-screening due in ~7 months — comfortably in date.
            -210,
            "AS9100D certificate on file (exp. 2028-03). Denied-party screen clear. \
             Mill certs 3.1 per EN 10204 on every heat.",
        ),
        (
            cast.aluminium_vendor_id.as_str(),
            ApprovedStatus::Conditional,
            &[ApprovalCategory::Aerospace, ApprovalCategory::General],
            // Lapsed 11 days ago — the overdue-re-screening branch.
            11,
            "Conditional: plate only, no forgings. Re-screening window has lapsed — \
             requalification audit outstanding.",
        ),
        (
            cast.suspended_vendor_id.as_str(),
            ApprovedStatus::Suspended,
            &[ApprovalCategory::Defense],
            30,
            "Suspended after the 2026-07 furnace-calibration finding. No new purchase \
             orders until the corrective action is verified.",
        ),
    ];

    for (partner_id, status, cats, until_days_ago, notes) in rows {
        let vendor = {
            let guard = db
                .write()
                .map_err(|e| anyhow::anyhow!("shared writer for AVL seed: {e}"))?;
            avl_vendors::create_vendor(
                &guard,
                t,
                &avl_vendors::VendorInputs {
                    partner_id: partner_id.to_string(),
                    approved_status: status.as_str().to_string(),
                    approval_categories: cats.iter().map(|c| c.as_str().to_string()).collect(),
                    approved_until_utc: Some(iso(days_ago(until_days_ago))),
                    screening_notes: notes.to_string(),
                },
                DEMO_OPERATOR,
            )
            .context("seed an AVL vendor row")?
        };
        // The audit append takes its own writer, so the guard above is
        // scoped tight (holding two would deadlock the single writer).
        avl_vendors::append_vendor_event_via_handle(
            db,
            tenant.clone(),
            binary_hash,
            DEMO_OPERATOR,
            EventKind::AvlVendorAdded,
            serde_json::to_vec(&serde_json::json!({
                "vendor_id": vendor.id,
                "partner_id": vendor.partner_id,
                "approved_status": vendor.approved_status,
                "approval_categories": vendor.approval_categories,
                "operator_user_id": DEMO_OPERATOR,
            }))
            .context("encode the AVL vendor-added payload")?,
        )
        .context("record the AVL vendor-added audit entry")?;
        s.avl_vendors += 1;
    }
    Ok(())
}

// ── Act 3 · procurement, and the delivery that failed inspection ────

/// Two purchase orders against approved vendors. The first is received clean;
/// the second's incoming inspection **fails**, which is the code path that
/// auto-creates an NCR — a bad delivery cannot be received without a quality
/// record ([[trust-code-not-operator]], ADR-0068 invariant 3).
fn seed_purchasing(
    db_path: &std::path::Path,
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    cast: &Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    // ── PO 1 — titanium bar, clean receipt.
    let po_ti = purchasing::create_po(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        purchasing::NewPo {
            vendor_partner_id: cast.titanium_vendor_id.clone(),
            currency: "EUR".to_string(),
            vat_rate_pct: 27,
            expected_delivery_utc: Some(iso(days_ago(21))),
            notes: "Ti-6Al-4V bar for the Meridian LG-BRKT-4412 batch. \
                    EN 10204 3.1 mill cert required with every heat."
                .to_string(),
            lines: vec![purchasing::NewPoLine {
                product_id: Some(cast.ti_bar_product_id.clone()),
                description: "Ti-6Al-4V (Grade 5) bar Ø60 × 3000 mm, AMS 4928".to_string(),
                quantity: 24,
                unit_price_minor: 39_000,
                expected_heat_lot_required: true,
            }],
        },
    )
    .map_err(|e| anyhow::anyhow!("seed the titanium purchase order: {e}"))?;
    s.purchase_orders += 1;

    purchasing::transition_po(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        &po_ti.po_id,
        purchasing::PoState::IssuedToVendor,
        Some(DEMO_OPERATOR),
    )
    .map_err(|e| anyhow::anyhow!("issue the titanium purchase order: {e}"))?;

    // The persisted lines ride back on the `create_po` return (with their
    // server-generated `pol_id`s). We must NOT re-read them through the shared
    // `db` Handle here: `create_po` wrote them via its own residual opener, and
    // a held Handle is a separate DuckDB instance that cannot see that write
    // until a checkpoint (proven: fresh-open sees the line, `db.read()` sees 0).
    let ti_lines = &po_ti.lines;
    purchasing::record_receipt(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        &po_ti.po_id,
        purchasing::NewReceipt {
            delivery_note_number: "TSE-DN-2026-4471".to_string(),
            lines: ti_lines
                .iter()
                .map(|l| purchasing::ReceiptLineInput {
                    pol_id: l.pol_id.clone(),
                    received_quantity: l.quantity,
                    inspection_pass: true,
                    inspection_notes: "Dimensional spot-check OK. 3.1 cert matches heat."
                        .to_string(),
                    heat_lot: Some(HEAT_LOT_TI.to_string()),
                })
                .collect(),
        },
    )
    .map_err(|e| anyhow::anyhow!("record the titanium delivery: {e}"))?;

    // ── PO 2 — aluminium plate, and the delivery that FAILS inspection.
    let po_al = purchasing::create_po(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        purchasing::NewPo {
            vendor_partner_id: cast.aluminium_vendor_id.clone(),
            currency: "EUR".to_string(),
            vat_rate_pct: 27,
            expected_delivery_utc: Some(iso(days_ago(9))),
            notes: "7075-T651 plate for the HYD-MAN-2207 batch.".to_string(),
            lines: vec![purchasing::NewPoLine {
                product_id: Some(cast.al_plate_product_id.clone()),
                description: "7075-T651 plate 60 × 200 × 1000 mm, AMS 4045".to_string(),
                quantity: 8,
                unit_price_minor: 12_400,
                expected_heat_lot_required: true,
            }],
        },
    )
    .map_err(|e| anyhow::anyhow!("seed the aluminium purchase order: {e}"))?;
    s.purchase_orders += 1;

    purchasing::transition_po(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        &po_al.po_id,
        purchasing::PoState::IssuedToVendor,
        Some(DEMO_OPERATOR),
    )
    .map_err(|e| anyhow::anyhow!("issue the aluminium purchase order: {e}"))?;

    // Same as the titanium delivery: use the lines returned in-process by
    // `create_po`, never a Handle read-back of this residual-opener write.
    let al_lines = &po_al.lines;
    purchasing::record_receipt(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        &po_al.po_id,
        purchasing::NewReceipt {
            delivery_note_number: "DM-DN-2026-0912".to_string(),
            lines: al_lines
                .iter()
                .map(|l| purchasing::ReceiptLineInput {
                    pol_id: l.pol_id.clone(),
                    // Six of eight plates delivered; the PO lands
                    // PartiallyReceived, which is the state the screen's
                    // middle case renders.
                    received_quantity: 6,
                    inspection_pass: false,
                    inspection_notes: "Two plates show surface pitting outside AMS 4045 \
                                       acceptance. Rolling direction not marked on any plate."
                        .to_string(),
                    heat_lot: Some(HEAT_LOT_AL.to_string()),
                })
                .collect(),
        },
    )
    .map_err(|e| anyhow::anyhow!("record the aluminium delivery: {e}"))?;
    // `record_receipt` auto-created the NCR for the failed line.
    s.ncrs += 1;

    Ok(())
}

// ── Act 4 · material on the shelf, with a heat lot and an MTR ───────

/// Material-side balances (`inventory_balances`, keyed by grade) plus the
/// product-side stock the BOM will consume (`stock_movements`, keyed by
/// product). Two grades get a heat lot bound to them with a real Mill Test
/// Report file on disk; the third deliberately does **not**, so the screen
/// shows what an un-certified grade looks like.
fn seed_material_stock(
    db_path: &std::path::Path,
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    cast: &Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let t = tenant.as_str();
    let now = iso(OffsetDateTime::now_utc());

    // The material-side balance rows. There is no production "goods-in"
    // writer for these — `commit_material_in_tx` only ever upserts at zeros
    // and then reserves against them — so the seed states the on-hand mass
    // directly, through the shared writer.
    {
        let guard = db
            .write()
            .map_err(|e| anyhow::anyhow!("shared writer for material balances: {e}"))?;
        for (grade, on_hand) in [
            (GRADE_TITANIUM, 186.4_f64),
            (GRADE_ALUMINIUM, 242.0),
            (GRADE_STAINLESS, 58.5),
        ] {
            guard
                .execute(
                    "INSERT INTO inventory_balances (
                        tenant_id, material_grade, on_hand_qty, reserved_qty,
                        committed_qty, consumed_qty, unit_of_measure, last_updated
                     ) VALUES (?, ?, ?, 0, 0, 0, ?, ?)
                     ON CONFLICT (tenant_id, material_grade) DO NOTHING",
                    params![t, grade, on_hand, material_inventory::DEFAULT_UOM, &now],
                )
                .with_context(|| format!("seed the {grade} material balance"))?;
            s.material_balances += 1;
        }
    }

    // Mill Test Reports. `validate_mtr_url` only checks the `file://` scheme,
    // so nothing stops a demo pointing at a file that does not exist — but a
    // traceability screen whose MTR link is a dead path is exactly the kind of
    // prop this project refuses. Write the documents.
    let mtr_dir = db_path
        .parent()
        .context("resolve the demo tenant home for the MTR documents")?
        .join("demo-mtr");
    std::fs::create_dir_all(&mtr_dir)
        .with_context(|| format!("create the demo MTR dir {}", mtr_dir.display()))?;

    for (grade, heat_lot, spec, supplier) in [
        (
            GRADE_TITANIUM,
            HEAT_LOT_TI,
            "AMS 4928 / ASTM B348 Gr.5",
            "TitaniumSource Europe GmbH",
        ),
        (
            GRADE_ALUMINIUM,
            HEAT_LOT_AL,
            "AMS 4045 / EN 573-3",
            "Danube Metals Zrt.",
        ),
    ] {
        let file = mtr_dir.join(format!("{heat_lot}.txt"));
        std::fs::write(
            &file,
            format!(
                "MILL TEST REPORT (EN 10204 type 3.1) — DEMO DOCUMENT, NOT A REAL CERTIFICATE\n\
                 ==========================================================================\n\
                 Heat / lot ....... {heat_lot}\n\
                 Material ......... {grade}\n\
                 Specification .... {spec}\n\
                 Supplier ......... {supplier}\n\
                 \n\
                 This file is written by `aberp demo-seed` so that the Mill Test Report\n\
                 link on the Inventory and Material Traceability screens resolves to a\n\
                 real document during a demo. It carries no chemistry and no mechanical\n\
                 results, because inventing them would make it look like evidence.\n"
            ),
        )
        .with_context(|| format!("write the demo MTR for {heat_lot}"))?;

        let assignment = {
            let guard = db
                .write()
                .map_err(|e| anyhow::anyhow!("shared writer for the heat-lot assignment: {e}"))?;
            material_inventory::assign_heat_lot(
                &guard,
                t,
                grade,
                heat_lot,
                &format!("file://{}", file.display()),
                DEMO_OPERATOR,
            )
            .with_context(|| format!("bind heat lot {heat_lot} to {grade}"))?
        };
        material_inventory::append_heat_lot_events(db, tenant.clone(), binary_hash, &assignment)
            .context("record the heat-lot audit trail")?;
    }

    // Product-side stock: the raw bar / plate the work-order Releases will
    // consume, through the real ADR-0061 movement writer so `stock_qty` is
    // rebuilt from `SUM(qty_delta)` and the movement history is genuine.
    let ledger_meta = meta(tenant, binary_hash);
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the goods-in movements: {e}"))?;
    let tx = guard
        .transaction()
        .context("begin the goods-in movement transaction")?;
    let ctx = aberp_inventory::RecordMovementContext {
        tenant: t,
        actor: aberp_inventory::ActorKind::SpaOperator {
            operator_login: DEMO_OPERATOR.to_string(),
        },
        ledger_meta: &ledger_meta,
        ledger_actor: actor(),
    };
    for (product_id, qty, note) in [
        (
            &cast.ti_bar_product_id,
            24_i64,
            "Goods-in against PO — Ti-6Al-4V bar, heat HT-2026-TI-88431.",
        ),
        (
            &cast.al_plate_product_id,
            6,
            "Goods-in against PO — 7075-T651 plate, heat HT-2026-AL-55210 (part delivery).",
        ),
    ] {
        aberp_inventory::record_movement(
            &tx,
            &ctx,
            aberp_inventory::RecordMovementInputs {
                product_id: product_id.clone(),
                qty_delta: Decimal::new(qty, 0),
                reason: aberp_inventory::MovementReason::Receipt,
                ref_kind: aberp_inventory::MovementRefKind::Manual,
                ref_id: None,
                notes: Some(note.to_string()),
                idempotency_key: format!("demo-seed:goods-in:{product_id}"),
            },
        )
        .map_err(|e| anyhow::anyhow!("record a goods-in stock movement: {e}"))?;
    }
    tx.commit()
        .context("commit the goods-in movement transaction")?;
    Ok(())
}

// ── Act 5 · CAD in, price out ───────────────────────────────────────

/// A part as the STEP extractor would have described it, plus the operator
/// decisions (stock form, tolerance, buyer) that move the price.
struct DemoPart {
    quote_id: &'static str,
    cad_filename: &'static str,
    contact_name: &'static str,
    contact_email: &'static str,
    company: &'static str,
    notes: &'static str,
    grade: &'static str,
    quantity: u32,
    graph: FeatureGraph,
    /// `(kind, od_mm, id_mm, length_mm)` — the ADR-0094 Gap 1 stock form.
    stock_form: Option<(&'static str, Option<f64>, Option<f64>, Option<f64>)>,
    /// `(band, spec)` — the ADR-0097 per-job tolerance.
    tolerance: (&'static str, ToleranceSpec),
    fetched_days_ago: i64,
}

/// Three machines, so the S427 capacity model has a real shop to schedule
/// against instead of falling back to one virtual machine.
fn seed_machines(guard: &duckdb::Connection, tenant: &str) -> Result<()> {
    for (name, family, env, hours) in [
        (
            "DMU 50 (5-axis)",
            "5-axis-mill",
            [500.0, 450.0, 400.0],
            14.0,
        ),
        (
            "NLX 2500 turn-mill",
            "turn-mill",
            [366.0, 366.0, 705.0],
            14.0,
        ),
        ("VF-2SS (3-axis)", "3-axis-mill", [762.0, 406.0, 508.0], 8.0),
    ] {
        crate::quoting_machines::create_machine(
            guard,
            tenant,
            &crate::quoting_machines::MachineInputs {
                name: name.to_string(),
                family: family.to_string(),
                max_envelope_xyz_mm: env,
                daily_hours_avail: hours,
                buffer_pct: 15.0,
                enabled: true,
            },
        )
        .with_context(|| format!("seed the {name} machine"))?;
    }
    Ok(())
}

/// The landing-gear bracket: a turned-and-milled titanium part with real
/// located holes, quoted at a precision band off round bar.
fn bracket_graph() -> FeatureGraph {
    FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        bounding_box_mm: [148.0, 62.0, 62.0],
        volume_mm3: 214_800.0,
        surface_area_mm2: 41_950.0,
        material_grade: GRADE_TITANIUM.to_string(),
        features: vec![
            Feature {
                feature_type: FeatureType::Pocket,
                count: 2,
                representative_size_mm: 34.0,
            },
            Feature {
                feature_type: FeatureType::Hole,
                count: 6,
                representative_size_mm: 12.0,
            },
            Feature {
                feature_type: FeatureType::Thread,
                count: 4,
                representative_size_mm: 10.0,
            },
            Feature {
                feature_type: FeatureType::Undercut5Axis,
                count: 1,
                representative_size_mm: 18.0,
            },
            Feature {
                feature_type: FeatureType::Surface,
                count: 3,
                representative_size_mm: 60.0,
            },
        ],
        requires_5_axis: true,
        thin_wall_present: true,
        stock_form: StockForm::RoundBar {
            diameter_mm: 62.0,
            length_mm: 152.0,
        },
        gears: Vec::new(),
        tolerance: ToleranceSpec::ItGrade { grade: 7 },
        critical_feature_tolerances: Vec::new(),
        located_holes: vec![
            LocatedHole {
                diameter_mm: 12.02,
                depth_mm: 62.0,
                axis_unit: [0.0, 0.0, -1.0],
                entry_point_mm: [24.0, 31.0, 62.0],
                end_condition: HoleEndCondition::Through,
                flat_bottom: false,
            },
            LocatedHole {
                diameter_mm: 12.02,
                depth_mm: 62.0,
                axis_unit: [0.0, 0.0, -1.0],
                entry_point_mm: [124.0, 31.0, 62.0],
                end_condition: HoleEndCondition::Through,
                flat_bottom: false,
            },
            LocatedHole {
                diameter_mm: 8.0,
                depth_mm: 22.0,
                axis_unit: [0.0, -1.0, 0.0],
                entry_point_mm: [74.0, 62.0, 31.0],
                end_condition: HoleEndCondition::Blind,
                flat_bottom: true,
            },
        ],
    }
}

/// The hydraulic manifold: a prismatic aluminium block, tight but not
/// precision, cut from plate.
fn manifold_graph() -> FeatureGraph {
    FeatureGraph {
        schema_version: FeatureGraph::SCHEMA_VERSION,
        bounding_box_mm: [96.0, 58.0, 44.0],
        volume_mm3: 168_300.0,
        surface_area_mm2: 26_400.0,
        material_grade: GRADE_ALUMINIUM.to_string(),
        features: vec![
            Feature {
                feature_type: FeatureType::Hole,
                count: 11,
                representative_size_mm: 9.0,
            },
            Feature {
                feature_type: FeatureType::Thread,
                count: 8,
                representative_size_mm: 12.0,
            },
            Feature {
                feature_type: FeatureType::Pocket,
                count: 1,
                representative_size_mm: 40.0,
            },
            Feature {
                feature_type: FeatureType::Surface,
                count: 2,
                representative_size_mm: 90.0,
            },
        ],
        requires_5_axis: false,
        thin_wall_present: false,
        stock_form: StockForm::RectangularBlock,
        gears: Vec::new(),
        tolerance: ToleranceSpec::GeneralClass {
            class: aberp_quote_engine::GeneralClass::Iso2768Medium,
        },
        critical_feature_tolerances: Vec::new(),
        located_holes: Vec::new(),
    }
}

/// Drive two quotes through the real pipeline states — Fetched → Pricing →
/// Rendering → PostingBack → Posted — pricing each with the **real engine**
/// off the tenant's own catalogues and rendering the **real customer PDF**.
/// Nothing here fabricates a number: the totals, the reasoning log and the
/// lead time are whatever the shipped code computes for these parts.
///
/// A third job is left `Failed` with a permanent verdict, because a pricing
/// queue that has never failed is not a pricing queue an operator recognises.
fn seed_quotes(
    db: &HandleArc,
    tenant: &TenantId,
    cast: &mut Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let t = tenant.as_str();
    let parts = [
        DemoPart {
            quote_id: "b1f6c0a2-4d7e-4c11-9a3f-0d2e5c8b7a41",
            cad_filename: "LG-BRKT-4412_rev-C.step",
            contact_name: "Anna Kovács",
            contact_email: "a.kovacs@meridian-aero.example",
            company: CUSTOMER_DISPLAY,
            notes: "Landing-gear bracket, AS9102 FAI required on first article. \
                    Ø12 H7 bores are key characteristics.",
            grade: GRADE_TITANIUM,
            quantity: 12,
            graph: bracket_graph(),
            stock_form: Some(("round_bar", Some(62.0), None, Some(152.0))),
            tolerance: ("precision", ToleranceSpec::ItGrade { grade: 7 }),
            fetched_days_ago: 34,
        },
        DemoPart {
            quote_id: "c93a71de-8b25-4f60-b7c4-1e8a4f2d6b09",
            cad_filename: "HYD-MAN-2207_rev-B.step",
            contact_name: "Anna Kovács",
            contact_email: "a.kovacs@meridian-aero.example",
            company: CUSTOMER_DISPLAY,
            notes: "Hydraulic manifold. Ports to ISO 6149-1; deburr all cross-drillings.",
            grade: GRADE_ALUMINIUM,
            quantity: 25,
            graph: manifold_graph(),
            stock_form: None,
            tolerance: (
                "tight",
                ToleranceSpec::GeneralClass {
                    class: aberp_quote_engine::GeneralClass::Iso2768Fine,
                },
            ),
            fetched_days_ago: 19,
        },
    ];

    let artifact_dir = db
        .db_path()
        .parent()
        .context("resolve the demo tenant home for the quote artifacts")?
        .join("demo-quote-artifacts");

    {
        let guard = db
            .write()
            .map_err(|e| anyhow::anyhow!("shared writer for the machine catalogue: {e}"))?;
        seed_machines(&guard, t)?;
    }

    for (i, part) in parts.iter().enumerate() {
        price_one_quote(db, tenant, part, &cast.customer_partner_id, &artifact_dir)?;
        if i == 0 {
            cast.bracket_quote_id = part.quote_id.to_string();
        } else {
            cast.manifold_quote_id = part.quote_id.to_string();
        }
        s.pricing_jobs += 1;
    }

    // The failed job. Written through the real enqueue-failure writer so its
    // `failure_kind` drives the same operator affordances (retry vs delete)
    // the daemon's own failures do.
    {
        let guard = db
            .write()
            .map_err(|e| anyhow::anyhow!("shared writer for the failed pricing job: {e}"))?;
        jobs::insert_failed_enqueue_job(
            &guard,
            "5e2b9c47-6a10-4d83-8f55-2c7b1a904e6d",
            t,
            "procurement@northfield-defence.example",
            "Peter Nagy",
            "Northfield Defence Systems Ltd.",
            GRADE_STAINLESS,
            40,
            "extract",
            "STEP file carries no solid body (assembly with suppressed parts) — \
             the extractor cannot compute a volume.",
            jobs::FailureKind::Permanent,
            days_ago(4),
        )
        .context("seed the failed pricing job")?;
        s.pricing_jobs += 1;
    }

    seed_intake_rows(db, tenant, &parts, s)?;
    Ok(())
}

/// Walk ONE part through the pipeline states, pricing with the real engine
/// and rendering the real PDF. Every write goes through the same
/// `quote_pricing_jobs` transition writers the daemon uses, so a demo that
/// clicks Retry or Re-price lands on exactly the code an operator's own
/// action would.
fn price_one_quote(
    db: &HandleArc,
    tenant: &TenantId,
    part: &DemoPart,
    buyer_partner_id: &str,
    artifact_dir: &std::path::Path,
) -> Result<()> {
    let t = tenant.as_str();
    let quote_id = part.quote_id;
    let fetched = days_ago(part.fetched_days_ago);
    let graph_json = serde_json::to_string(&part.graph).context("encode the demo FeatureGraph")?;
    // The same key the pipeline uses: blake3 over the canonical encoding.
    let graph_hash = blake3::hash(graph_json.as_bytes()).to_hex().to_string();
    let cad_dir = artifact_dir.join(quote_id);
    std::fs::create_dir_all(&cad_dir)
        .with_context(|| format!("create the demo artifact dir {}", cad_dir.display()))?;
    let cad_path = cad_dir.join(part.cad_filename);

    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the demo quote: {e}"))?;

    jobs::insert_fetched_job(
        &guard,
        quote_id,
        t,
        part.contact_email,
        part.contact_name,
        part.company,
        part.grade,
        part.quantity,
        part.cad_filename,
        &cad_path.to_string_lossy(),
        fetched,
    )
    .context("insert the demo pricing job")?;

    // Operator decisions, through the same routes' writers.
    jobs::set_buyer_partner(
        &guard,
        quote_id,
        t,
        Some(buyer_partner_id),
        fetched + Duration::hours(2),
    )
    .context("assign the demo quote's buyer partner")?;
    if let Some((kind, od, id, len)) = part.stock_form {
        jobs::set_stock_form(
            &guard,
            quote_id,
            t,
            Some(kind),
            od,
            id,
            len,
            fetched + Duration::hours(2),
        )
        .context("set the demo quote's stock form")?;
    }
    let (band, ref spec) = part.tolerance;
    jobs::set_tolerance(
        &guard,
        quote_id,
        t,
        Some(band),
        Some(&serde_json::to_string(&serde_json::json!({
            "overall": spec,
            "critical_features": [],
        }))?),
        Some(spec.requires_manual_review()),
        fetched + Duration::hours(2),
    )
    .context("set the demo quote's tolerance")?;

    // Fetched → Pricing, carrying the extracted graph.
    jobs::set_extracted(
        guard.conn(),
        quote_id,
        t,
        &graph_hash,
        &graph_json,
        fetched + Duration::hours(3),
    )
    .context("stamp the demo quote's extraction")?;

    // The price. Real engine, real catalogues, real margin policy.
    let outcome = quote_pricing_pipeline::reprice_quote(&guard, t, quote_id, None)
        .context("price the demo quote")?
        .context("the demo quote had no feature graph to price")?;

    jobs::set_priced(
        guard.conn(),
        quote_id,
        t,
        &outcome.breakdown_json,
        outcome.total_price,
        fetched + Duration::hours(3) + Duration::seconds(4),
    )
    .context("stamp the demo quote's price")?;
    jobs::set_margin_result(
        &guard,
        quote_id,
        t,
        &outcome.breakdown_json,
        outcome.total_price,
        outcome.below_floor,
        outcome.floor_pct,
        fetched + Duration::hours(3) + Duration::seconds(4),
    )
    .context("stamp the demo quote's margin verdict")?;

    // S427 lead-time — the daemon's own computation, verbatim: the shop's
    // enabled machines, the last 30 days of Posted load, and this batch's
    // projected hours on the family the route picks.
    let breakdown: aberp_quote_engine::QuoteBreakdown =
        serde_json::from_str(&outcome.breakdown_json).context("decode the demo breakdown")?;
    {
        let since = iso(OffsetDateTime::now_utc() - Duration::days(30));
        let machines = crate::quoting_machines::list_enabled_capacities(&guard, t)
            .context("load the demo machine capacities")?;
        let existing = jobs::sum_posted_machining_hours_by_family(&guard, t, &since, quote_id)
            .context("sum the demo shop load")?;
        let mut new_hours = std::collections::BTreeMap::new();
        let proj_h = (breakdown.machining_minutes / 60.0 * (part.quantity as f64)).max(0.0);
        if proj_h > 0.0 {
            new_hours.insert(
                aberp_quote_engine::MachineFamily::for_route(breakdown.route_to_5_axis),
                proj_h,
            );
        }
        let est = aberp_quote_engine::lead_time_days(&machines, &existing, &new_hours);
        jobs::set_computed_lead_time(&guard, quote_id, t, est.days)
            .context("stamp the demo quote's computed lead time")?;
    }

    // Pricing → Rendering → PostingBack, with the REAL customer PDF on disk
    // so the operator's "Download PDF" opens a document, not a 404.
    let valid_until = (fetched + Duration::days(30))
        .date()
        .format(&time::macros::format_description!("[year]-[month]-[day]"))
        .context("format the demo quote's valid-until date")?;
    let pdf_bytes = aberp_quote_pdf::render(&aberp_quote_pdf::QuoteInputs {
        quote_id,
        customer_email: part.contact_email,
        customer_name: part.contact_name,
        customer_company: part.company,
        quantity: part.quantity,
        notes: part.notes,
        valid_until_iso: &valid_until,
        extractor_version: aberp_cad_extract_wrapper::WRAPPER_VERSION,
        engine_version: &breakdown.engine_version,
        feature_graph: &part.graph,
        breakdown: &breakdown,
        target_tolerance: tolerance_range_for(band),
        stock_alert: false,
        lead_time_days: jobs::get_effective_lead_time_days(&guard, quote_id, t)?,
    })
    .map_err(|e| anyhow::anyhow!("render the demo quote PDF: {e}"))?;
    let pdf_path = cad_dir.join("priced.pdf");
    std::fs::write(&pdf_path, &pdf_bytes)
        .with_context(|| format!("write the demo quote PDF {}", pdf_path.display()))?;

    // The CAD file the job points at. A placeholder, and it says so — the
    // seed has no STEP geometry to ship and will not pretend otherwise. The
    // FeatureGraph above is what the pipeline actually consumed.
    std::fs::write(
        &cad_path,
        format!(
            "ISO-10303-21;\n\
             /* DEMO PLACEHOLDER — not a STEP file.\n\
                `aberp demo-seed` records the extracted FeatureGraph for\n\
                {} directly, because the seed ships no CAD geometry. Drop a\n\
                real STEP file over this path to re-run the extractor. */\n\
             ENDSEC;\nEND-ISO-10303-21;\n",
            part.cad_filename
        ),
    )
    .with_context(|| format!("write the demo CAD placeholder {}", cad_path.display()))?;

    jobs::set_rendered(
        guard.conn(),
        quote_id,
        t,
        &pdf_path.to_string_lossy(),
        &valid_until,
        fetched + Duration::hours(3) + Duration::seconds(9),
    )
    .context("stamp the demo quote's render")?;
    jobs::set_state(
        &guard,
        quote_id,
        t,
        jobs::JobState::Posted,
        fetched + Duration::hours(3) + Duration::seconds(12),
    )
    .context("post the demo quote back")?;
    Ok(())
}

/// Map the stored tolerance-band db-string onto the engine enum for the PDF's
/// addendum-1 surcharge line. Total over the closed vocab; an unknown token is
/// a programming error in this module, not operator input, so it is loud.
fn tolerance_range_for(band: &str) -> aberp_quote_engine::ToleranceRange {
    use aberp_quote_engine::ToleranceRange as R;
    match band {
        "loose" => R::Loose,
        "standard" => R::Standard,
        "tight" => R::Tight,
        "precision" => R::Precision,
        "ultra_precision" => R::UltraPrecision,
        other => unreachable!("demo seed used an unknown tolerance band {other:?}"),
    }
}

/// The customer-facing half of the same two quotes: `quote_intake_log` rows,
/// which is what the Invoices → **Quotes** tab lists. One is already DEAL'd
/// (so the tab shows the post-deal chip); the other is still actionable, so
/// an operator can drive the DEAL gate live on stage.
fn seed_intake_rows(
    db: &HandleArc,
    tenant: &TenantId,
    parts: &[DemoPart],
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let t = tenant.as_str();
    let guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the quote-intake rows: {e}"))?;
    for part in parts {
        let raw = serde_json::json!({
            "id": part.quote_id,
            "contact": {
                "name": part.contact_name,
                "email": part.contact_email,
                "company": part.company,
            },
            "material": part.grade,
            "quantity": part.quantity,
            "notes": part.notes,
            "files": [{ "filename": part.cad_filename }],
            "status": "accepted",
        });
        let draft = serde_json::json!({
            "customer": { "legal_name": part.company },
            "lines": [{
                "description": format!("{} — {} ×{}", part.cad_filename, part.grade, part.quantity),
            }],
        });
        aberp_quote_intake::log_table::insert_intake(
            &guard,
            t,
            part.quote_id,
            // No invoice has been issued for these yet; the column is the
            // intake's own correlation slot, and the pickup route overwrites
            // it when the operator makes a draft.
            "",
            &iso(days_ago(part.fetched_days_ago)),
            days_ago(part.fetched_days_ago) + Duration::minutes(4),
            &raw.to_string(),
            &draft.to_string(),
        )
        .map_err(|e| anyhow::anyhow!("seed a quote-intake row: {e}"))?;
        s.intake_rows += 1;
    }
    Ok(())
}

// ── Act 6 · the inspection plan ─────────────────────────────────────

/// Four balloon-numbered characteristics for the bracket and two for the
/// manifold, carrying the ADR-0199 §D3(a) AS9102 identity metadata
/// (balloon number, designator, type, method, sheet/zone, accountability).
/// This is what the QC Inspection Plans screen lists and what a FAIR's
/// characteristic accountability counts against.
fn seed_inspection_plans(
    db: &HandleArc,
    tenant: &TenantId,
    cast: &mut Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    use aberp_qa::qc::{CharacteristicDesignator, CharacteristicType, InspectionMethod};
    let t = tenant.as_str();
    let guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the inspection plans: {e}"))?;

    let plans: [(
        &str,
        &str,
        f64,
        f64,
        f64,
        &str,
        CharacteristicDesignator,
        InspectionMethod,
        &str,
        bool,
    ); 4] = [
        (
            "1",
            "Bore Ø12 H7 (fwd trunnion)",
            12.018,
            0.012,
            -0.012,
            "mm",
            CharacteristicDesignator::Key,
            InspectionMethod::Cmm,
            "1/C3",
            true,
        ),
        (
            "2",
            "Bore Ø12 H7 (aft trunnion)",
            12.018,
            0.012,
            -0.012,
            "mm",
            CharacteristicDesignator::Key,
            InspectionMethod::Cmm,
            "1/C6",
            true,
        ),
        (
            "3",
            "Flange thickness",
            8.0,
            0.05,
            -0.05,
            "mm",
            CharacteristicDesignator::Major,
            InspectionMethod::Gauge,
            "2/B4",
            true,
        ),
        (
            "4",
            "Trunnion centre distance",
            100.0,
            0.03,
            -0.03,
            "mm",
            CharacteristicDesignator::Critical,
            InspectionMethod::Cmm,
            "1/C4",
            true,
        ),
    ];

    for (num, feature, nominal, upper, lower, units, designator, method, zone, required) in plans {
        let plan = aberp_qa::qc::create_plan(
            &guard,
            t,
            aberp_qa::qc::NewInspectionPlan {
                product_id: cast.bracket_product_id.clone(),
                feature_name: feature.to_string(),
                nominal_value: nominal,
                upper_tol: upper,
                lower_tol: lower,
                units: units.to_string(),
                optional_probe_cycle_id: None,
                enabled: true,
                characteristic_number: Some(num.to_string()),
                characteristic_designator: Some(designator),
                characteristic_type: Some(CharacteristicType::Dimensional),
                inspection_method: Some(method),
                sheet_zone: Some(zone.to_string()),
                is_required: Some(required),
            },
        )
        .map_err(|e| anyhow::anyhow!("seed the bracket inspection plan {feature:?}: {e}"))?;
        cast.bracket_plan_ids.push(plan.plan_id);
        s.inspection_plans += 1;
    }

    for (num, feature, nominal, tol, zone, method) in [
        (
            "1",
            "Port P1 bore Ø9.0",
            9.0,
            0.04,
            "1/A2",
            InspectionMethod::OnMachineProbe,
        ),
        (
            "2",
            "Face flatness datum A",
            0.02,
            0.01,
            "1/A1",
            InspectionMethod::Cmm,
        ),
    ] {
        aberp_qa::qc::create_plan(
            &guard,
            t,
            aberp_qa::qc::NewInspectionPlan {
                product_id: cast.manifold_product_id.clone(),
                feature_name: feature.to_string(),
                nominal_value: nominal,
                upper_tol: tol,
                lower_tol: -tol,
                units: "mm".to_string(),
                optional_probe_cycle_id: None,
                enabled: true,
                characteristic_number: Some(num.to_string()),
                characteristic_designator: Some(CharacteristicDesignator::Major),
                characteristic_type: Some(CharacteristicType::Dimensional),
                inspection_method: Some(method),
                sheet_zone: Some(zone.to_string()),
                is_required: Some(true),
            },
        )
        .map_err(|e| anyhow::anyhow!("seed the manifold inspection plan {feature:?}: {e}"))?;
        s.inspection_plans += 1;
    }
    Ok(())
}

// ── Act 7 · the shop floor ──────────────────────────────────────────

/// Three work orders in three different states, so the wall-TV state grid
/// has more than one bucket and every downstream screen has something to
/// point at:
///
/// * **WO-2026-0101** — the first bracket batch. Completed: every routing op
///   done, every QA inspection passed, parts marked, ready to dispatch.
/// * **WO-2026-0102** — the manifold batch. In progress, first op complete,
///   which leaves one **Pending** QA inspection sitting in the queue.
/// * **WO-2026-0103** — the second bracket batch. Also completed and marked,
///   but act 9 opens a non-conformance against its units, so its dispatch is
///   the one the ship gate refuses.
fn seed_work_orders(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    cast: &mut Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let bracket_ops = || {
        vec![
            routing_op("10 — Saw bar to length", 12, 900),
            routing_op("20 — Turn-mill Ø62 body (NLX 2500)", 96, 7_400),
            routing_op("30 — 5-axis mill trunnions + pockets (DMU 50)", 148, 13_200),
            routing_op("40 — Deburr + CMM first article", 44, 3_100),
        ]
    };

    cast.wo_bracket_a = create_wo(
        db,
        tenant,
        binary_hash,
        "WO-2026-0101",
        &cast.bracket_product_id,
        12,
        Some(cast.bracket_quote_id.clone()),
        "Meridian LG-BRKT-4412 rev C, batch 1 of 2. Heat HT-2026-TI-88431.",
        bracket_ops(),
    )?;
    cast.wo_manifold = create_wo(
        db,
        tenant,
        binary_hash,
        "WO-2026-0102",
        &cast.manifold_product_id,
        25,
        Some(cast.manifold_quote_id.clone()),
        "Meridian HYD-MAN-2207 rev B. Heat HT-2026-AL-55210.",
        vec![
            routing_op("10 — Saw plate blanks", 18, 1_100),
            routing_op("20 — 3-axis mill body (VF-2SS)", 72, 4_600),
            routing_op("30 — Cross-drill + tap ports", 51, 3_300),
            routing_op("40 — Deburr + leak test", 30, 2_000),
        ],
    )?;
    cast.wo_bracket_b = create_wo(
        db,
        tenant,
        binary_hash,
        "WO-2026-0103",
        &cast.bracket_product_id,
        6,
        Some(cast.bracket_quote_id.clone()),
        "Meridian LG-BRKT-4412 rev C, batch 2 of 2. Heat HT-2026-TI-88431.",
        bracket_ops(),
    )?;
    s.work_orders += 3;

    // WO-0101 and WO-0103 run all the way to Completed; WO-0102 stops after
    // its first operation so the QA queue has a live Pending row.
    drive_wo_to_completion(db, tenant, binary_hash, &cast.wo_bracket_a)?;
    drive_wo_to_completion(db, tenant, binary_hash, &cast.wo_bracket_b)?;
    drive_wo_to_first_op(db, tenant, binary_hash, &cast.wo_manifold)?;
    Ok(())
}

fn routing_op(name: &str, minutes: i32, cost_huf: i64) -> aberp_work_orders::RoutingOpInput {
    aberp_work_orders::RoutingOpInput {
        op_name: name.to_string(),
        est_time_min: Some(minutes),
        est_cost_huf: Some(Decimal::new(cost_huf, 0)),
    }
}

fn wo_ctx<'a>(
    tenant: &'a str,
    ledger_meta: &'a LedgerMeta,
) -> aberp_work_orders::WoWriteContext<'a> {
    aberp_work_orders::WoWriteContext {
        tenant,
        actor: aberp_inventory::ActorKind::SpaOperator {
            operator_login: DEMO_OPERATOR.to_string(),
        },
        ledger_meta,
        ledger_actor: actor(),
    }
}

#[allow(clippy::too_many_arguments)]
fn create_wo(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    wo_number: &str,
    product_id: &str,
    qty: i64,
    source_quote_id: Option<String>,
    notes: &str,
    ops: Vec<aberp_work_orders::RoutingOpInput>,
) -> Result<String> {
    let ledger_meta = meta(tenant, binary_hash);
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the demo work order: {e}"))?;
    let tx = guard
        .transaction()
        .context("begin the demo work-order transaction")?;
    let (wo, _ops) = aberp_work_orders::create_work_order(
        &tx,
        &wo_ctx(tenant.as_str(), &ledger_meta),
        aberp_work_orders::CreateWorkOrderInputs {
            wo_number: wo_number.to_string(),
            product_id: product_id.to_string(),
            qty_target: Decimal::new(qty, 0),
            notes: Some(notes.to_string()),
            routing_ops: ops,
            idempotency_key: format!("demo-seed:wo-create:{wo_number}"),
            source_quote_id,
        },
    )
    .map_err(|e| anyhow::anyhow!("create the demo work order {wo_number}: {e}"))?;
    tx.commit()
        .context("commit the demo work-order transaction")?;
    Ok(wo.wo_id)
}

/// Release + Start, then complete the FIRST routing op only. The op
/// completion auto-creates a Pending `qa_inspections` row (ADR-0063 §2) —
/// which is exactly the state the QA queue exists to show.
fn drive_wo_to_first_op(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    wo_id: &str,
) -> Result<()> {
    let ledger_meta = meta(tenant, binary_hash);
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the WO transitions: {e}"))?;
    let tx = guard.transaction().context("begin the WO transition tx")?;
    let ctx = wo_ctx(tenant.as_str(), &ledger_meta);
    for action in [
        aberp_work_orders::WoAction::Release,
        aberp_work_orders::WoAction::Start,
    ] {
        aberp_work_orders::transition_work_order(
            &tx,
            &ctx,
            wo_id,
            aberp_work_orders::TransitionInputs {
                action,
                reason: None,
                source_event_id: None,
                idempotency_key: format!("demo-seed:wo:{wo_id}:{}", action.as_str()),
                actual_machining_minutes: None,
            },
        )
        .map_err(|e| anyhow::anyhow!("transition the demo WO: {e}"))?;
    }
    let ops = aberp_work_orders::list_routing_ops_for_wo(&tx, tenant.as_str(), wo_id)
        .map_err(|e| anyhow::anyhow!("list the demo WO's routing ops: {e}"))?;
    let first = ops.first().context("the demo WO has no routing ops")?;
    aberp_work_orders::transition_routing_op(
        &tx,
        &ctx,
        &first.routing_op_id,
        aberp_work_orders::RoutingOpTransitionInputs {
            action: aberp_work_orders::RoutingOpAction::Complete,
            source_event_id: None,
            idempotency_key: format!("demo-seed:rop:{}", first.routing_op_id),
        },
    )
    .map_err(|e| anyhow::anyhow!("complete the demo WO's first op: {e}"))?;
    tx.commit().context("commit the WO transition tx")?;
    Ok(())
}

/// Release → Start → every op complete → every QA inspection passed → WO
/// Complete. The Complete edge is *gated* on those passes
/// (`all_live_inspections_passed_for_wo`); the seed satisfies the gate the
/// same way an operator does, rather than writing the terminal state
/// directly.
fn drive_wo_to_completion(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    wo_id: &str,
) -> Result<()> {
    let t = tenant.as_str();
    let ledger_meta = meta(tenant, binary_hash);
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the WO run: {e}"))?;
    let tx = guard.transaction().context("begin the WO run tx")?;
    let ctx = wo_ctx(t, &ledger_meta);
    let qa_ctx = aberp_qa::QaWriteContext {
        tenant: t,
        actor: aberp_inventory::ActorKind::SpaOperator {
            operator_login: DEMO_OPERATOR.to_string(),
        },
        ledger_meta: &ledger_meta,
        ledger_actor: actor(),
    };

    for action in [
        aberp_work_orders::WoAction::Release,
        aberp_work_orders::WoAction::Start,
    ] {
        aberp_work_orders::transition_work_order(
            &tx,
            &ctx,
            wo_id,
            aberp_work_orders::TransitionInputs {
                action,
                reason: None,
                source_event_id: None,
                idempotency_key: format!("demo-seed:wo:{wo_id}:{}", action.as_str()),
                actual_machining_minutes: None,
            },
        )
        .map_err(|e| anyhow::anyhow!("transition the demo WO: {e}"))?;
    }

    let ops = aberp_work_orders::list_routing_ops_for_wo(&tx, t, wo_id)
        .map_err(|e| anyhow::anyhow!("list the demo WO's routing ops: {e}"))?;
    for op in &ops {
        let outcome = aberp_work_orders::transition_routing_op(
            &tx,
            &ctx,
            &op.routing_op_id,
            aberp_work_orders::RoutingOpTransitionInputs {
                action: aberp_work_orders::RoutingOpAction::Complete,
                source_event_id: None,
                idempotency_key: format!("demo-seed:rop:{}", op.routing_op_id),
            },
        )
        .map_err(|e| anyhow::anyhow!("complete a demo routing op: {e}"))?;
        aberp_qa::decide_qa(
            &tx,
            &qa_ctx,
            &outcome.qa_inspection_id,
            aberp_qa::DecideQaInputs {
                decision: aberp_qa::QaDecision::Pass,
                reason: Some(format!("In-process check on {} — conforming.", op.op_name)),
                measurement: None,
                source_event_id: None,
                idempotency_key: format!("demo-seed:qa:{}", outcome.qa_inspection_id),
            },
        )
        .map_err(|e| anyhow::anyhow!("pass a demo QA inspection: {e}"))?;
    }

    // Actual minutes, so the S429 closed-loop calibration hook has a real
    // sample to compare against the quote's estimate.
    let estimated: i64 = ops
        .iter()
        .filter_map(|o| o.est_time_min)
        .map(i64::from)
        .sum();
    aberp_work_orders::transition_work_order(
        &tx,
        &ctx,
        wo_id,
        aberp_work_orders::TransitionInputs {
            action: aberp_work_orders::WoAction::Complete,
            reason: None,
            source_event_id: None,
            idempotency_key: format!("demo-seed:wo:{wo_id}:complete"),
            actual_machining_minutes: Some(estimated as f64 * 1.08),
        },
    )
    .map_err(|e| anyhow::anyhow!("complete the demo WO: {e}"))?;
    tx.commit().context("commit the WO run tx")?;
    Ok(())
}

// ── Act 8 · every unit gets a UID ───────────────────────────────────

/// Mint one part UID + serial + data-matrix payload per unit on both
/// completed bracket batches, each carrying the heat lot as its
/// chain-of-custody tail. This is what the ADR-0089 shipment gate counts
/// against `qty_target`, and what Part UID Lookup resolves.
fn seed_part_marks(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    cast: &Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let t = tenant.as_str();
    for (wo_id, units) in [(&cast.wo_bracket_a, 12u32), (&cast.wo_bracket_b, 6)] {
        let marked_at = iso(days_ago(3));
        let marks: Vec<part_marking::PartMark> = (1..=units)
            .map(|unit_index| {
                let part_uid = part_marking::generate_part_uid();
                let serial = part_marking::auto_serial(wo_id, unit_index);
                part_marking::PartMark {
                    wo_id: wo_id.clone(),
                    unit_index,
                    data_matrix_payload: part_marking::data_matrix_payload(
                        &part_uid,
                        &serial,
                        Some(HEAT_LOT_TI),
                    ),
                    part_uid,
                    serial_number: serial,
                    heat_lot_reference: Some(HEAT_LOT_TI.to_string()),
                    marked_at_utc: marked_at.clone(),
                    marked_by_operator: DEMO_OPERATOR.to_string(),
                }
            })
            .collect();

        {
            let guard = db
                .write()
                .map_err(|e| anyhow::anyhow!("shared writer for the part marks: {e}"))?;
            part_marking::record_part_marks(&guard, t, wo_id, &marks)
                .map_err(|e| anyhow::anyhow!("record the demo part marks: {e}"))?;
        }
        part_marking::append_mark_events(
            db,
            tenant.clone(),
            binary_hash,
            wo_id,
            DEMO_OPERATOR,
            &marked_at,
            Some(HEAT_LOT_TI),
            &marks,
        )
        .context("record the part-marking audit trail")?;
        s.part_marks += marks.len();
    }
    Ok(())
}

// ── Act 9 · measure it, and say so when it fails ────────────────────

/// Dimensional QC against the seeded inspection plans, on real part UIDs.
///
/// Batch 1 measures **conforming** on all four characteristics. Batch 2's
/// trunnion centre distance measures **out of band** — the engine computes
/// the failing verdict itself, the seed raises the NCR that verdict
/// recommends, and links it back onto the inspection row. That open NCR is
/// what makes batch 2's dispatch refusable.
fn seed_qc_and_ncr(
    db_path: &std::path::Path,
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    cast: &Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    use aberp_qa::qc::{QcSource, RecordInspectionInputs, Verdict};
    let t = tenant.as_str();
    let ledger_meta = meta(tenant, binary_hash);

    // The plans, and the first unit of each batch to measure against.
    let (plans, uid_a, uid_b) = {
        let conn = db
            .read()
            .map_err(|e| anyhow::anyhow!("read the demo inspection plans: {e}"))?;
        let plans = aberp_qa::qc::list_plans(&conn, t, Some(&cast.bracket_product_id), false)
            .map_err(|e| anyhow::anyhow!("list the bracket inspection plans: {e}"))?;
        let first_uid = |wo_id: &str| -> Result<String> {
            Ok(part_marking::list_part_marks(&conn, t, wo_id)
                .context("list the demo part marks")?
                .first()
                .context("a completed demo WO had no part marks")?
                .part_uid
                .clone())
        };
        (
            plans,
            first_uid(&cast.wo_bracket_a)?,
            first_uid(&cast.wo_bracket_b)?,
        )
    };

    // Batch 1 — all conforming. Measured values sit just inside the band,
    // which is what a real first article looks like.
    let failing_feature = "Trunnion centre distance";
    let mut failing_qci: Option<(String, Verdict)> = None;
    {
        let mut guard = db
            .write()
            .map_err(|e| anyhow::anyhow!("shared writer for the QC inspections: {e}"))?;
        let tx = guard.transaction().context("begin the QC inspection tx")?;
        let qc_ctx = aberp_qa::qc::QcWriteContext {
            tenant: t,
            actor: aberp_inventory::ActorKind::SpaOperator {
                operator_login: DEMO_OPERATOR.to_string(),
            },
            ledger_meta: &ledger_meta,
            ledger_actor: actor(),
        };
        let now = OffsetDateTime::now_utc();
        for plan in &plans {
            // A hair above nominal — inside the band on every plan.
            let good = plan.nominal_value + plan.upper_tol * 0.35;
            aberp_qa::qc::record_inspection(
                &tx,
                &qc_ctx,
                RecordInspectionInputs {
                    plan,
                    source: QcSource::Cmm,
                    source_event_id: None,
                    actual_value: good,
                    units: plan.units.clone(),
                    probe_serial: Some("ZEISS-CONTURA-77214".to_string()),
                    last_calibration_at: Some(now - Duration::days(21)),
                    measured_at: now - Duration::days(3),
                    current_time: now,
                    stale_window_seconds: 86_400 * 180,
                    linked_part_uid: Some(uid_a.clone()),
                    linked_heat_lot: Some(HEAT_LOT_TI.to_string()),
                    linked_wo_id: Some(cast.wo_bracket_a.clone()),
                    recorded_by: DEMO_OPERATOR.to_string(),
                },
            )
            .map_err(|e| anyhow::anyhow!("record a conforming demo QC inspection: {e}"))?;
            s.qc_inspections += 1;

            // Batch 2 — the same characteristic set, but the centre
            // distance is out of band.
            let batch_b_value = if plan.feature_name == failing_feature {
                plan.nominal_value + plan.upper_tol * 2.4
            } else {
                plan.nominal_value + plan.lower_tol * 0.3
            };
            let recorded = aberp_qa::qc::record_inspection(
                &tx,
                &qc_ctx,
                RecordInspectionInputs {
                    plan,
                    source: QcSource::Cmm,
                    source_event_id: None,
                    actual_value: batch_b_value,
                    units: plan.units.clone(),
                    probe_serial: Some("ZEISS-CONTURA-77214".to_string()),
                    last_calibration_at: Some(now - Duration::days(21)),
                    measured_at: now - Duration::days(2),
                    current_time: now,
                    stale_window_seconds: 86_400 * 180,
                    linked_part_uid: Some(uid_b.clone()),
                    linked_heat_lot: Some(HEAT_LOT_TI.to_string()),
                    linked_wo_id: Some(cast.wo_bracket_b.clone()),
                    recorded_by: DEMO_OPERATOR.to_string(),
                },
            )
            .map_err(|e| anyhow::anyhow!("record a demo QC inspection for batch 2: {e}"))?;
            s.qc_inspections += 1;
            if recorded.auto_ncr_recommended {
                failing_qci = Some((recorded.inspection.qci_id.clone(), recorded.verdict));
            }
        }
        tx.commit().context("commit the QC inspection tx")?;
    }

    // The NCR the failing measurement recommends. Raised through the real
    // writer (own transaction + its own audit append), then linked back onto
    // the inspection row so the provenance is a join, not a note.
    let (qci_id, verdict) = failing_qci
        .context("the seeded out-of-band measurement did not produce a failing verdict")?;
    let ncr = quality::create_ncr(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        quality::NewNcr {
            severity: quality::NcrSeverity::Major,
            category: quality::NcrCategory::Workmanship,
            description: format!(
                "{failing_feature} measured outside the drawing band on WO-2026-0103 \
                 (CMM, ZEISS-CONTURA-77214). Suspect fixture shift on the 5-axis \
                 operation; batch quarantined pending containment."
            ),
            affected_part_uids: vec![uid_b.clone()],
            affected_wo_ids: vec![cast.wo_bracket_b.clone()],
            affected_heat_lots: vec![HEAT_LOT_TI.to_string()],
            photos: Vec::new(),
        },
    )
    .map_err(|e| anyhow::anyhow!("raise the demo NCR: {e}"))?;
    s.ncrs += 1;

    {
        let mut guard = db
            .write()
            .map_err(|e| anyhow::anyhow!("shared writer for the auto-NCR link: {e}"))?;
        let tx = guard.transaction().context("begin the auto-NCR link tx")?;
        aberp_qa::qc::link_auto_ncr(
            &tx,
            &aberp_qa::qc::QcWriteContext {
                tenant: t,
                actor: aberp_inventory::ActorKind::SpaOperator {
                    operator_login: DEMO_OPERATOR.to_string(),
                },
                ledger_meta: &ledger_meta,
                ledger_actor: actor(),
            },
            &qci_id,
            &ncr.ncr_id,
            verdict,
        )
        .map_err(|e| anyhow::anyhow!("link the demo auto-NCR: {e}"))?;
        tx.commit().context("commit the auto-NCR link tx")?;
    }

    // Move the NCR one step down its lifecycle so the Quality screen shows a
    // workflow in motion rather than a pile of Open rows. It stays
    // non-terminal on purpose: an OPEN non-conformance against these part
    // UIDs is precisely what the ship gate refuses.
    quality::transition_ncr(
        db_path,
        db,
        tenant.clone(),
        binary_hash,
        DEMO_OPERATOR,
        &ncr.ncr_id,
        quality::NcrState::Contained,
        "Batch 2 quarantined at the CMM. Fixture pulled for re-datum.",
    )
    .map_err(|e| anyhow::anyhow!("transition the demo NCR: {e}"))?;
    Ok(())
}

// ── Act 10 · the shipping desk, and the gate ────────────────────────

/// One drafted dispatch per completed batch, plus the invoice draft that
/// would follow. Both stop at **Drafted** deliberately: shipping is the step
/// the demo performs live, and it is the step where the gates speak.
///
/// * Batch 1's dispatch is clean — parts marked, QA passed, no open
///   non-conformance — so pressing Ship walks the whole export-control +
///   invoice-spawn path in front of the customer.
/// * Batch 2's dispatch carries the act-9 non-conformance, so pressing Ship
///   is **refused**. That refusal is the demo, not a failure of it.
fn seed_dispatch_and_drafts(
    db: &HandleArc,
    tenant: &TenantId,
    binary_hash: BinaryHash,
    cast: &Cast,
    s: &mut DemoSeedSummary,
) -> Result<()> {
    let t = tenant.as_str();
    let ledger_meta = meta(tenant, binary_hash);
    let mut guard = db
        .write()
        .map_err(|e| anyhow::anyhow!("shared writer for the demo dispatches: {e}"))?;
    let tx = guard.transaction().context("begin the demo dispatch tx")?;

    for (wo_id, product_id, qty, note) in [
        (
            &cast.wo_bracket_a,
            &cast.bracket_product_id,
            12_i64,
            "Batch 1 of 2 — 12 pcs, all units marked, FAI pack attached.",
        ),
        (
            &cast.wo_bracket_b,
            &cast.bracket_product_id,
            6,
            "Batch 2 of 2 — HELD. Open non-conformance against these unit UIDs.",
        ),
    ] {
        let dispatch = aberp_dispatch::create_dispatch(
            &tx,
            &aberp_dispatch::DispatchWriteContext {
                tenant: t,
                actor: aberp_inventory::ActorKind::SpaOperator {
                    operator_login: DEMO_OPERATOR.to_string(),
                },
                ledger_meta: &ledger_meta,
                ledger_actor: actor(),
            },
            aberp_dispatch::CreateDispatchInputs {
                wo_id: wo_id.clone(),
                partner_id: cast.customer_partner_id.clone(),
                notes: Some(note.to_string()),
                idempotency_key: format!("demo-seed:dispatch:{wo_id}"),
            },
        )
        .map_err(|e| anyhow::anyhow!("create a demo dispatch: {e}"))?;
        s.dispatches += 1;

        invoice_draft::create_draft_in_tx(
            &tx,
            &ledger_meta,
            actor(),
            invoice_draft::CreateDraftInputs {
                tenant: t.to_string(),
                partner_id: cast.customer_partner_id.clone(),
                source_dispatch_id: Some(dispatch.dsp_id.clone()),
                source_wo_id: Some(wo_id.clone()),
                source_quote_id: Some(cast.bracket_quote_id.clone()),
                product_id: product_id.clone(),
                qty: Decimal::new(qty, 0),
                notes: Some(note.to_string()),
                actor: DEMO_OPERATOR.to_string(),
                idempotency_key: format!("demo-seed:draft:{wo_id}"),
            },
        )
        .context("create a demo invoice draft")?;
        s.invoice_drafts += 1;
    }
    tx.commit().context("commit the demo dispatch tx")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one rule that keeps this command away from real data. Exact
    /// match, so no `demo-`-prefixed slug slips through.
    #[test]
    fn only_the_bundled_demo_slug_is_seedable() {
        assert!(refuse_non_demo_slug("demo").is_ok());
        for refused in ["defense", "prod", "demo-defense", "Demo", "demo2", ""] {
            let err = refuse_non_demo_slug(refused)
                .expect_err("a non-demo slug must be refused")
                .to_string();
            assert!(
                err.contains("demo-seed refuses tenant"),
                "refusal for {refused:?} must name itself: {err}"
            );
        }
    }

    /// The tolerance-band map is total over the closed vocab the
    /// `quote_pricing_jobs.tolerance_class` column stores. A band the DB can
    /// hold but this map cannot answer would panic at seed time.
    #[test]
    fn tolerance_bands_cover_the_stored_vocabulary() {
        use aberp_quote_engine::ToleranceRange as R;
        for (band, expected) in [
            ("loose", R::Loose),
            ("standard", R::Standard),
            ("tight", R::Tight),
            ("precision", R::Precision),
            ("ultra_precision", R::UltraPrecision),
        ] {
            assert_eq!(tolerance_range_for(band), expected, "band {band:?}");
        }
    }

    /// The two seeded FeatureGraphs must round-trip through the wire the
    /// pipeline stores them on — a graph the engine cannot decode would fail
    /// the seed at `reprice_quote`, and the failure would be a JSON error
    /// several acts downstream of the mistake.
    #[test]
    fn seeded_feature_graphs_round_trip() {
        for graph in [bracket_graph(), manifold_graph()] {
            let json = serde_json::to_string(&graph).expect("encode");
            let back: FeatureGraph = serde_json::from_str(&json).expect("decode");
            assert_eq!(back.material_grade, graph.material_grade);
            assert_eq!(back.stock_form, graph.stock_form);
            assert_eq!(back.located_holes.len(), graph.located_holes.len());
            assert!(
                back.volume_mm3 > 0.0 && back.bounding_box_mm.iter().all(|d| *d > 0.0),
                "a seeded graph with a zero dimension would price as free"
            );
        }
    }

    /// Both seeded grades must exist in the boot-seeded `quoting_materials`
    /// catalogue, or the engine answers `MaterialNotInCatalogue` and the
    /// whole quote act fails.
    #[test]
    fn seeded_grades_are_catalogue_grades() {
        let mut conn = duckdb::Connection::open_in_memory().expect("open in-memory DuckDB");
        crate::quoting_materials::seed_if_empty(&mut conn, "t").expect("seed the catalogue");
        let grades = crate::quoting_materials::list_materials(&conn, "t").expect("list");
        for wanted in [GRADE_TITANIUM, GRADE_ALUMINIUM, GRADE_STAINLESS] {
            assert!(
                grades.iter().any(|m| m.grade == wanted),
                "{wanted} is not in the seeded catalogue; the demo quote would not price"
            );
        }
    }

    /// Heat lots go through `aberp_compliance::lot_heat` validation on their
    /// way onto a balance row — `[A-Za-z0-9-]` only.
    #[test]
    fn seeded_heat_lots_are_valid_lot_ids() {
        for lot in [HEAT_LOT_TI, HEAT_LOT_AL] {
            aberp_compliance::lot_heat::LotId::new(lot)
                .unwrap_or_else(|e| panic!("heat lot {lot} is not a valid lot id: {e}"));
        }
    }
}
