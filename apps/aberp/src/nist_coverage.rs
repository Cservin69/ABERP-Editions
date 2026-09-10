//! D-04 (ADR-0121) — NIST SP 800-171 Rev. 2 control-evidence map + coverage
//! fold.
//!
//! The 110 control constants live in [`aberp_compliance::nist_800_171`]; this
//! module is their consumer. It answers the assessor's question — "which ledger
//! events evidence which control, and have they occurred?" — as a **derived,
//! read-side** report, never by stamping anything onto an event.
//!
//! ## Where the tag lives, and why here (ADR-0121)
//!
//! A control tag is **not** a per-event payload field. The mapping "kind K
//! evidences control C" is revisable analyst / SSP judgment; the ledger is
//! append-only and hash-pinned. Baking a revisable opinion into uncorrectable
//! bytes at ~191 firing sites is the D-01 / D-03 anti-pattern. Instead the map
//! is a **static table keyed on the [`EventKind`] enum** (a rename/removal is a
//! compile error; referenced controls are the `nist_800_171` constants, so a
//! typo does not compile), and evidence is **derived on read**.
//!
//! It lives in `apps/aberp` (not `aberp-compliance`) because the map references
//! both the control constants *and* [`EventKind`]: `aberp-compliance` is a lean
//! leaf crate that must not gain an `aberp-audit-ledger` dependency, and
//! `apps/aberp` already depends on both — the same home the sibling
//! [`crate::cui_marking`] / [`crate::cyber_incident`] / [`crate::dpas_rating`]
//! feature modules use.
//!
//! ## The honesty rule
//!
//! The report says **"evidence present,"** never "satisfied" or "compliant."
//! Presence of a mapped event is necessary, not sufficient — an assessor
//! decides satisfaction. [`EvidenceState`] deliberately has **no** `Satisfied`
//! variant. ABERP reports what its ledger shows; it never grades itself. This
//! is slice 1: the map + the fold, library-only and inert (no ledger reader, no
//! renderer — those are slice 2).

use aberp_audit_ledger::{Entry, EventKind};
use aberp_compliance::nist_800_171 as nist;
use time::OffsetDateTime;

/// One asserted evidentiary link: emitting `kind` contributes evidence toward
/// satisfying `control`. `rationale` states WHY in one line, so a reviewer or
/// assessor can challenge the link — a map entry is an analyst claim, and the
/// claim must be legible.
///
/// `control` is always one of the [`nist::ALL_CONTROLS`] constants (referenced
/// by constant, never a bare string, so a typo does not compile); this is
/// asserted for every entry by [`tests::every_link_control_is_a_real_constant`].
#[derive(Debug, Clone)]
pub struct EvidenceLink {
    /// The audit event kind whose emission is the evidence. Keyed on the enum,
    /// so a renamed/removed variant breaks the build here.
    pub kind: EventKind,
    /// The control this kind contributes evidence toward (an `ALL_CONTROLS`
    /// member).
    pub control: &'static str,
    /// One line: why this kind is defensible evidence of this control.
    pub rationale: &'static str,
}

/// The complete, reviewed set of evidentiary links.
///
/// **Starter set (ADR-0121 Open Q1).** High-confidence, deliberately
/// under-claiming: a kind is linked only where its emission is direct,
/// defensible evidence of the control's activity. Doubtful associations are
/// left OUT — the report then honestly shows the control as unevidenced rather
/// than inflate the coverage number an assessor trusts. Later passes grow this
/// map by reviewed code change (its git history is the mapping's own audit
/// trail).
static EVIDENCE_LINKS: &[EvidenceLink] = &[
    // ── Access Control (AC) ──────────────────────────────────────────────
    EvidenceLink {
        kind: EventKind::PersonnelAccessGranted,
        control: nist::AC_3_1_2, // limit access to permitted transactions/functions
        rationale:
            "a per-request access GRANT decision is the enforcement of which \
             transactions/functions an authenticated operator may perform",
    },
    EvidenceLink {
        kind: EventKind::PersonnelAccessDenied,
        control: nist::AC_3_1_2,
        rationale:
            "the DENY side of the same transaction/function access enforcement — \
             an under-cleared operator is refused and the refusal recorded",
    },
    EvidenceLink {
        kind: EventKind::CuiAccessEvent,
        control: nist::AC_3_1_3, // control the flow of CUI per approved authorizations
        rationale:
            "every read of a CUI-marked artifact records an access decision — \
             the CUI access trail IS the flow-control record",
    },
    // ── Media Protection (MP) ────────────────────────────────────────────
    EvidenceLink {
        kind: EventKind::CuiMarkingApplied,
        control: nist::MP_3_8_4, // mark media with CUI markings and distribution limitations
        rationale:
            "applying a typed CuiMarking (category + dissemination controls) to \
             an artifact is literally marking media with its CUI markings",
    },
    // ── Identification & Authentication (IA) ─────────────────────────────
    EvidenceLink {
        kind: EventKind::PersonnelIdRegistered,
        control: nist::IA_3_5_1, // identify system users, processes, and devices
        rationale:
            "registering an operator identity is the act of identifying a system \
             user before access is mediated",
    },
    // ── Audit & Accountability (AU) ──────────────────────────────────────
    EvidenceLink {
        kind: EventKind::PersonnelSignatureApplied,
        control: nist::AU_3_3_2, // uniquely trace user actions to the user
        rationale:
            "an e-signature binds a specific record action to a specific signer \
             identity + algorithm — the strongest unique-traceability record",
    },
    // ── Incident Response (IR) ───────────────────────────────────────────
    EvidenceLink {
        kind: EventKind::IncidentCyberDetected,
        control: nist::IR_3_6_1, // operational incident-handling capability
        rationale:
            "an operator-declared cyber incident, captured with its DoD 72h \
             deadline, evidences an operational incident-handling capability",
    },
    EvidenceLink {
        kind: EventKind::IncidentCyberDetected,
        control: nist::IR_3_6_2, // track, document, report incidents
        rationale:
            "the same intake documents the incident (severity, affected flags, \
             detection source) — the tracking/documentation half of 3.6.2",
    },
];

/// The reviewed evidentiary links (see [`EVIDENCE_LINKS`]).
pub fn evidence_links() -> &'static [EvidenceLink] {
    EVIDENCE_LINKS
}

/// Controls that `kind` contributes evidence toward (possibly empty — most
/// kinds evidence no specific control). Order follows [`EVIDENCE_LINKS`].
pub fn controls_for(kind: &EventKind) -> Vec<&'static str> {
    EVIDENCE_LINKS
        .iter()
        .filter(|l| &l.kind == kind)
        .map(|l| l.control)
        .collect()
}

/// Event kinds whose emission evidences `control` (possibly empty). Order
/// follows [`EVIDENCE_LINKS`].
pub fn kinds_for(control: &str) -> Vec<EventKind> {
    EVIDENCE_LINKS
        .iter()
        .filter(|l| l.control == control)
        .map(|l| l.kind.clone())
        .collect()
}

/// A kind observed in the ledger, already scoped to the report's window by the
/// reader (slice 2): `count` is the number of such events IN scope. The fold
/// treats `count > 0` as "evidence present"; the count itself is informational.
#[derive(Debug, Clone)]
pub struct ObservedKind {
    pub kind: EventKind,
    pub count: u64,
}

/// The assessment window the coverage was computed over — metadata echoed into
/// the report so a reader never mistakes all-time evidence for period evidence.
/// `None` bounds mean open-ended; both `None` is explicitly all-time.
#[derive(Debug, Clone, Default)]
pub struct TimeWindow {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
}

/// One kind's contribution to an evidenced control, for the report detail.
#[derive(Debug, Clone)]
pub struct EvidencingKind {
    pub kind: EventKind,
    pub count: u64,
    pub rationale: &'static str,
}

/// The three honest states a control can be in. **There is deliberately no
/// `Satisfied` / `Compliant` variant** — evidence presence is necessary, not
/// sufficient, and the assessor grades satisfaction (ADR-0121 honesty rule).
#[derive(Debug, Clone)]
pub enum EvidenceState {
    /// Mapped to ≥1 kind AND ≥1 such event is present in the window.
    Evidenced { kinds: Vec<EvidencingKind> },
    /// Mapped to ≥1 kind, but none of them occurred in the window — a designed
    /// evidence path with no activity behind it (yet).
    MappedNotExercised { kinds: Vec<EventKind> },
    /// No mapped kind at all: an organizational / physical / policy control no
    /// ABERP software event can evidence. Shown honestly, never hidden.
    NoAutomatedEvidence,
}

/// One control's coverage row.
#[derive(Debug, Clone)]
pub struct ControlCoverage {
    /// The `ALL_CONTROLS` constant (`"3.X.Y: title"`).
    pub control: &'static str,
    pub state: EvidenceState,
}

/// The full coverage report over all 110 controls.
#[derive(Debug, Clone)]
pub struct CoverageReport {
    /// The window the observed set was scoped to (metadata for the header).
    pub window: TimeWindow,
    /// All 110 controls, in [`nist::ALL_CONTROLS`] order.
    pub controls: Vec<ControlCoverage>,
    /// Observed kinds that map to NO control — surfaced (not dropped) so an
    /// analyst can decide whether they should evidence something.
    pub observed_unmapped_kinds: Vec<EventKind>,
}

impl CoverageReport {
    /// Count of controls in [`EvidenceState::Evidenced`].
    pub fn evidenced_count(&self) -> usize {
        self.controls
            .iter()
            .filter(|c| matches!(c.state, EvidenceState::Evidenced { .. }))
            .count()
    }
}

/// Fold the observed kinds against the evidence map into a per-control report.
///
/// Pure and total: it iterates all 110 [`nist::ALL_CONTROLS`] in order and
/// classifies each into one [`EvidenceState`]. `observed` is presumed already
/// window-scoped (slice 2's reader applies the window in SQL); `window` is
/// recorded verbatim for the report header. A control mapped to several kinds
/// is `Evidenced` if ANY mapped kind is observed (union). An observed kind that
/// maps to no control lands in `observed_unmapped_kinds`.
pub fn coverage(observed: &[ObservedKind], window: TimeWindow) -> CoverageReport {
    let observed_count = |kind: &EventKind| -> Option<u64> {
        observed
            .iter()
            .find(|o| &o.kind == kind)
            .map(|o| o.count)
            .filter(|c| *c > 0)
    };

    let controls = nist::ALL_CONTROLS
        .iter()
        .map(|&control| {
            let mapped = kinds_for(control);
            let state = if mapped.is_empty() {
                EvidenceState::NoAutomatedEvidence
            } else {
                // Which mapped kinds actually occurred (in window)?
                let evidencing: Vec<EvidencingKind> = EVIDENCE_LINKS
                    .iter()
                    .filter(|l| l.control == control)
                    .filter_map(|l| {
                        observed_count(&l.kind).map(|count| EvidencingKind {
                            kind: l.kind.clone(),
                            count,
                            rationale: l.rationale,
                        })
                    })
                    .collect();
                if evidencing.is_empty() {
                    EvidenceState::MappedNotExercised { kinds: mapped }
                } else {
                    EvidenceState::Evidenced { kinds: evidencing }
                }
            };
            ControlCoverage { control, state }
        })
        .collect();

    // Observed kinds that evidence no control at all.
    let observed_unmapped_kinds = observed
        .iter()
        .filter(|o| o.count > 0 && controls_for(&o.kind).is_empty())
        .map(|o| o.kind.clone())
        .collect();

    CoverageReport {
        window,
        controls,
        observed_unmapped_kinds,
    }
}

impl TimeWindow {
    /// Is `t` within this window? Open bounds (`None`) never exclude; both
    /// `None` is all-time. Comparison is on the instant (epoch ms), NOT on the
    /// RFC3339 string — the sub-second-precision string-compare trap A1's reaper
    /// and the QC-report ordering bug both taught us to avoid.
    pub fn contains(&self, t: OffsetDateTime) -> bool {
        let ms = (t.unix_timestamp_nanos() / 1_000_000) as i64;
        self.from_ms.is_none_or(|from| ms >= from) && self.to_ms.is_none_or(|to| ms <= to)
    }
}

/// D-04 (ADR-0121) slice 2 — the reader fold: count the audit `Entry`s per kind,
/// within `window`, into the [`ObservedKind`] set [`coverage`] consumes.
///
/// **Pure over the entries** (no DB open here): slice 3's transport reads the
/// entries through serve's existing shared `Handle` (`Ledger::entries()`), so
/// this module adds no new database opener — the frozen-opener ledger
/// (cut-gate CHECK 10i) is untouched. The window is applied HERE, on the parsed
/// instant, so `coverage` receives an already-window-scoped set. A kind that
/// evidences no control still counts (the report surfaces it as
/// observed-but-unmapped); the caller passes ALL entries, not a pre-filtered
/// slice.
pub fn observed_kinds(entries: &[Entry], window: &TimeWindow) -> Vec<ObservedKind> {
    let mut counts: Vec<(EventKind, u64)> = Vec::new();
    for e in entries {
        if !window.contains(e.time_wall) {
            continue;
        }
        if let Some(slot) = counts.iter_mut().find(|(k, _)| *k == e.kind) {
            slot.1 += 1;
        } else {
            counts.push((e.kind.clone(), 1));
        }
    }
    counts
        .into_iter()
        .map(|(kind, count)| ObservedKind { kind, count })
        .collect()
}

/// D-04 (ADR-0121) slice 2 — render a [`CoverageReport`] as a structured JSON
/// value (the shape slice 3's serve route / SPA consumes). Language is honest:
/// `state` is `evidenced` / `mapped_not_exercised` / `no_automated_evidence`,
/// never `compliant`; the top-level `disclaimer` states that evidence presence
/// is not satisfaction.
pub fn render_report_json(report: &CoverageReport) -> serde_json::Value {
    let controls: Vec<serde_json::Value> = report
        .controls
        .iter()
        .map(|c| match &c.state {
            EvidenceState::Evidenced { kinds } => serde_json::json!({
                "control": c.control,
                "state": "evidenced",
                "kinds": kinds.iter().map(|k| serde_json::json!({
                    "kind": k.kind.as_str(),
                    "count": k.count,
                    "rationale": k.rationale,
                })).collect::<Vec<_>>(),
            }),
            EvidenceState::MappedNotExercised { kinds } => serde_json::json!({
                "control": c.control,
                "state": "mapped_not_exercised",
                "kinds": kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            }),
            EvidenceState::NoAutomatedEvidence => serde_json::json!({
                "control": c.control,
                "state": "no_automated_evidence",
            }),
        })
        .collect();

    let (mut evidenced, mut mapped, mut none) = (0usize, 0usize, 0usize);
    for c in &report.controls {
        match c.state {
            EvidenceState::Evidenced { .. } => evidenced += 1,
            EvidenceState::MappedNotExercised { .. } => mapped += 1,
            EvidenceState::NoAutomatedEvidence => none += 1,
        }
    }

    serde_json::json!({
        "disclaimer":
            "Shows which ledger events evidence which NIST SP 800-171 control. \
             Evidence presence is necessary, not sufficient — an assessor decides \
             control satisfaction. This report does not assert compliance.",
        "window": { "from_ms": report.window.from_ms, "to_ms": report.window.to_ms },
        "summary": {
            "total": report.controls.len(),
            "evidenced": evidenced,
            "mapped_not_exercised": mapped,
            "no_automated_evidence": none,
        },
        "controls": controls,
        "observed_unmapped_kinds":
            report.observed_unmapped_kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
    })
}

/// D-04 (ADR-0121) slice 2 — render a [`CoverageReport`] as a plain-text table
/// for a terminal / log. Same honest language as the JSON.
pub fn render_report_table(report: &CoverageReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "NIST SP 800-171 Rev. 2 — ledger evidence coverage");
    let _ = writeln!(
        out,
        "  (evidence present ≠ control satisfied; an assessor grades satisfaction)"
    );
    let window = match (report.window.from_ms, report.window.to_ms) {
        (None, None) => "all time".to_string(),
        (from, to) => format!("from={from:?} to={to:?} (epoch ms)"),
    };
    let _ = writeln!(out, "  window: {window}\n");

    for c in &report.controls {
        let tag = match &c.state {
            EvidenceState::Evidenced { kinds } => {
                let total: u64 = kinds.iter().map(|k| k.count).sum();
                format!("[EVIDENCED   ] {total} event(s)")
            }
            EvidenceState::MappedNotExercised { .. } => "[mapped, none]".to_string(),
            EvidenceState::NoAutomatedEvidence => "[no auto ev. ]".to_string(),
        };
        let _ = writeln!(out, "  {tag}  {}", c.control);
    }

    let (mut evidenced, mut mapped, mut none) = (0usize, 0usize, 0usize);
    for c in &report.controls {
        match c.state {
            EvidenceState::Evidenced { .. } => evidenced += 1,
            EvidenceState::MappedNotExercised { .. } => mapped += 1,
            EvidenceState::NoAutomatedEvidence => none += 1,
        }
    }
    let _ = writeln!(
        out,
        "\n  {} of {} controls evidenced, {} mapped-but-unexercised, {} with no automated evidence in ABERP",
        evidenced,
        report.controls.len(),
        mapped,
        none
    );
    if !report.observed_unmapped_kinds.is_empty() {
        let _ = writeln!(
            out,
            "  observed kinds mapped to no control ({}): {}",
            report.observed_unmapped_kinds.len(),
            report
                .observed_unmapped_kinds
                .iter()
                .map(|k| k.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every link's `control` is a real `ALL_CONTROLS` member — the compile-time
    /// constant reference already guarantees this, but pin it so a future edit
    /// that (wrongly) switched to a bare string could not slip a typo past.
    #[test]
    fn every_link_control_is_a_real_constant() {
        for l in evidence_links() {
            assert!(
                nist::ALL_CONTROLS.contains(&l.control),
                "link for {:?} references a non-ALL_CONTROLS string: {:?}",
                l.kind,
                l.control
            );
        }
    }

    /// No duplicate `(kind, control)` pair — a duplicate would double the
    /// evidencing-kind detail for one control with no added information.
    #[test]
    fn no_duplicate_kind_control_pairs() {
        let mut seen: Vec<(&EventKind, &str)> = Vec::new();
        for l in evidence_links() {
            let pair = (&l.kind, l.control);
            assert!(
                !seen.contains(&pair),
                "duplicate (kind, control) link: {:?} → {:?}",
                l.kind,
                l.control
            );
            seen.push(pair);
        }
    }

    /// Every rationale is non-empty — a link is an analyst claim and must carry
    /// its justification.
    #[test]
    fn every_link_has_a_rationale() {
        for l in evidence_links() {
            assert!(
                !l.rationale.trim().is_empty(),
                "link {:?} → {:?} has an empty rationale",
                l.kind,
                l.control
            );
        }
    }

    /// The honesty rule is structural: `EvidenceState` exposes no way to say a
    /// control is satisfied/compliant. This test documents+guards the intent by
    /// enumerating the allowed states (a new `Satisfied` variant would force a
    /// compile error here, prompting the author to reconsider ADR-0121).
    #[test]
    fn evidence_state_has_no_satisfied_variant() {
        fn assert_exhaustive(s: &EvidenceState) -> &'static str {
            match s {
                EvidenceState::Evidenced { .. } => "evidenced",
                EvidenceState::MappedNotExercised { .. } => "mapped_not_exercised",
                EvidenceState::NoAutomatedEvidence => "no_automated_evidence",
            }
        }
        assert_eq!(
            assert_exhaustive(&EvidenceState::NoAutomatedEvidence),
            "no_automated_evidence"
        );
    }

    /// The fold covers all 110 controls, in order, and classifies each of the
    /// three states correctly against a known observed set.
    #[test]
    fn coverage_partitions_all_110_controls() {
        // Observe an access grant (evidences AC 3.1.2) and a marking (MP 3.8.4).
        let observed = vec![
            ObservedKind {
                kind: EventKind::PersonnelAccessGranted,
                count: 3,
            },
            ObservedKind {
                kind: EventKind::CuiMarkingApplied,
                count: 1,
            },
        ];
        let report = coverage(&observed, TimeWindow::default());
        assert_eq!(report.controls.len(), 110, "all 110 controls present");
        // Order preserved.
        assert_eq!(report.controls[0].control, nist::ALL_CONTROLS[0]);

        let state_of = |ctrl: &str| {
            report
                .controls
                .iter()
                .find(|c| c.control == ctrl)
                .map(|c| &c.state)
                .expect("control present")
        };

        // AC 3.1.2 — evidenced (grant observed), and the count rides through.
        match state_of(nist::AC_3_1_2) {
            EvidenceState::Evidenced { kinds } => {
                assert!(kinds.iter().any(|k| k.kind == EventKind::PersonnelAccessGranted
                    && k.count == 3));
            }
            other => panic!("AC 3.1.2 should be Evidenced, got {other:?}"),
        }
        // AC 3.1.3 — mapped (to cui.access_event) but not exercised here.
        assert!(matches!(
            state_of(nist::AC_3_1_3),
            EvidenceState::MappedNotExercised { .. }
        ));
        // MP 3.8.4 — evidenced (marking observed).
        assert!(matches!(
            state_of(nist::MP_3_8_4),
            EvidenceState::Evidenced { .. }
        ));
        // AC 3.1.1 — no mapped kind at all (out-of-system for now).
        assert!(matches!(
            state_of(nist::AC_3_1_1),
            EvidenceState::NoAutomatedEvidence
        ));
        assert_eq!(report.evidenced_count(), 2, "exactly AC 3.1.2 + MP 3.8.4");
    }

    /// A control mapped to several kinds is evidenced if ANY is observed
    /// (union), and multiple observed kinds for one control all show up.
    #[test]
    fn union_semantics_and_multi_kind_control() {
        // IncidentCyberDetected maps to BOTH IR 3.6.1 and IR 3.6.2.
        let observed = vec![ObservedKind {
            kind: EventKind::IncidentCyberDetected,
            count: 1,
        }];
        let report = coverage(&observed, TimeWindow::default());
        for ctrl in [nist::IR_3_6_1, nist::IR_3_6_2] {
            let c = report.controls.iter().find(|c| c.control == ctrl).unwrap();
            assert!(
                matches!(c.state, EvidenceState::Evidenced { .. }),
                "{ctrl} should be Evidenced by the incident kind"
            );
        }
    }

    /// An observed kind that maps to no control is surfaced, not dropped.
    #[test]
    fn observed_unmapped_kind_is_surfaced() {
        let observed = vec![ObservedKind {
            kind: EventKind::Test, // never mapped to a control
            count: 5,
        }];
        let report = coverage(&observed, TimeWindow::default());
        assert!(report.observed_unmapped_kinds.contains(&EventKind::Test));
        assert_eq!(report.evidenced_count(), 0);
    }

    /// A zero-count observation does not evidence anything (defensive: the
    /// reader should not emit zero rows, but the fold must not trust it).
    #[test]
    fn zero_count_observation_is_not_evidence() {
        let observed = vec![ObservedKind {
            kind: EventKind::PersonnelAccessGranted,
            count: 0,
        }];
        let report = coverage(&observed, TimeWindow::default());
        assert!(matches!(
            report
                .controls
                .iter()
                .find(|c| c.control == nist::AC_3_1_2)
                .unwrap()
                .state,
            EvidenceState::MappedNotExercised { .. }
        ));
        assert!(report.observed_unmapped_kinds.is_empty());
    }

    /// `controls_for` / `kinds_for` are consistent inverses over the map.
    #[test]
    fn controls_for_and_kinds_for_agree() {
        for l in evidence_links() {
            assert!(controls_for(&l.kind).contains(&l.control));
            assert!(kinds_for(l.control).contains(&l.kind));
        }
    }

    // ── slice 2: the reader fold + renderers ─────────────────────────────

    /// Seed a fresh in-memory ledger with one event per entry in `kinds` and
    /// return the decoded entries (same pattern as `audit_query.rs`'s tests).
    fn seed_entries(kinds: &[EventKind]) -> Vec<Entry> {
        use aberp_audit_ledger::{Actor, BinaryHash, Ledger, TenantId};
        let mut ledger = Ledger::open_in_memory(
            TenantId::new("T").expect("tenant"),
            BinaryHash::from_bytes([0u8; 32]),
        )
        .expect("open in-memory ledger");
        for k in kinds {
            let actor = Actor::from_local_cli("01H0000000000000000000000Z".to_string(), "tester");
            ledger
                .append(k.clone(), b"{}".to_vec(), actor, None)
                .expect("append");
        }
        ledger.entries().expect("entries")
    }

    #[test]
    fn time_window_contains_bounds() {
        let t = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let ms = 1_700_000_000_000i64;
        assert!(TimeWindow::default().contains(t), "all-time includes everything");
        assert!(TimeWindow {
            from_ms: Some(ms - 1),
            to_ms: Some(ms + 1)
        }
        .contains(t));
        // Inclusive on both ends.
        assert!(TimeWindow {
            from_ms: Some(ms),
            to_ms: Some(ms)
        }
        .contains(t));
        // Starts after / ends before → excluded.
        assert!(!TimeWindow {
            from_ms: Some(ms + 1),
            to_ms: None
        }
        .contains(t));
        assert!(!TimeWindow {
            from_ms: None,
            to_ms: Some(ms - 1)
        }
        .contains(t));
    }

    #[test]
    fn observed_kinds_counts_and_windows() {
        let entries = seed_entries(&[
            EventKind::PersonnelAccessGranted,
            EventKind::PersonnelAccessGranted,
            EventKind::CuiMarkingApplied,
        ]);
        let obs = observed_kinds(&entries, &TimeWindow::default());
        let count_of = |k: &EventKind| {
            obs.iter().find(|o| &o.kind == k).map(|o| o.count).unwrap_or(0)
        };
        assert_eq!(count_of(&EventKind::PersonnelAccessGranted), 2);
        assert_eq!(count_of(&EventKind::CuiMarkingApplied), 1);

        // A window entirely in the past excludes the ~now entries.
        let past = TimeWindow {
            from_ms: None,
            to_ms: Some(0),
        };
        assert!(
            observed_kinds(&entries, &past).is_empty(),
            "no entry falls before epoch 0"
        );

        // End-to-end into the fold: the two grants evidence AC 3.1.2.
        let report = coverage(&obs, TimeWindow::default());
        assert!(matches!(
            report
                .controls
                .iter()
                .find(|c| c.control == nist::AC_3_1_2)
                .unwrap()
                .state,
            EvidenceState::Evidenced { .. }
        ));
    }

    #[test]
    fn render_json_is_honest_and_structured() {
        let obs = vec![ObservedKind {
            kind: EventKind::CuiMarkingApplied,
            count: 2,
        }];
        let report = coverage(&obs, TimeWindow::default());
        let v = render_report_json(&report);
        let s = v.to_string();
        // No compliance/satisfaction verdict leaks into the rendered output.
        assert!(!s.contains("compliant"), "must not claim compliant");
        assert!(!s.contains("satisfied"), "must not claim satisfied");
        assert!(v["disclaimer"]
            .as_str()
            .unwrap()
            .contains("not sufficient"));
        assert_eq!(v["summary"]["total"], 110);
        assert_eq!(v["summary"]["evidenced"], 1); // MP 3.8.4 only
        let controls = v["controls"].as_array().unwrap();
        let mp = controls
            .iter()
            .find(|c| c["control"] == nist::MP_3_8_4)
            .unwrap();
        assert_eq!(mp["state"], "evidenced");
        assert_eq!(mp["kinds"][0]["kind"], "cui.marking_applied");
        assert_eq!(mp["kinds"][0]["count"], 2);
        assert!(!mp["kinds"][0]["rationale"].as_str().unwrap().is_empty());
    }

    #[test]
    fn render_table_states_the_caveat_and_summary() {
        let report = coverage(&[], TimeWindow::default());
        let t = render_report_table(&report);
        assert!(t.contains("evidence present ≠ control satisfied"));
        assert!(t.contains("0 of 110 controls evidenced"));
    }
}
