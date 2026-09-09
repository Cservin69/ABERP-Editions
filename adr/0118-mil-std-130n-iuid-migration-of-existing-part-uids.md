# ADR-0118 — MIL-STD-130N IUID: migrate the existing `dp-` part UIDs (D-03)

- **Status:** Accepted — records Ervin's D-03 decision (2026-09-09); the deciding
  authority is the decision itself, not a later adversarial pass.
- **Date:** 2026-09-09
- **Deciders:** Ervin Áben
- **Related:** ADR-0089 (per-unit part-UID marking — the `dp-<ULID>` mint this
  extends), ADR-0075 (`part.*` audit family), `aberp-compliance::uid`
  (`IuidConstruct1` / `validate_iac`), `[[mock-everything-principle]]`,
  `[[trust-code-not-operator]]`.

## Context

Per-unit part marking is Live (ADR-0089): each produced unit is minted a
`dp-<ULID>` part UID, a DataMatrix payload `dp-<ULID>|<serial>|<heat>` is stored,
and a shipment gate + forward/reverse trace hang off it. `aberp-compliance::uid`
already models the real MIL-STD-130N vocabulary — `IuidConstruct1`
(`IAC + EID + Serial`), `IuidConstruct2`, `validate_iac`, IRI rendering — but
nothing constructs a real IUID: the mint is a `dp-` ULID, not a DoD IUID.

Two questions were open (backlog D-03): (a) what to do with the already-minted
`dp-` UIDs, and (b) the assigned **enterprise identifier** — a real CAGE / DoD
EID the shop does not yet have.

Ervin decided both (2026-09-09):

1. **Migrate the existing mint forward — do NOT orphan or re-mint.** The
   already-minted `dp-` UIDs carry into the MIL-STD-130N scheme; continuity is
   preserved.
2. **Use a clearly-marked MOCK enterprise identifier** (per the
   mock-everything rule), flagged in code + this ADR as a placeholder the real
   CAGE / DoD-assigned EID must replace before production. **Swapping in the
   real EID must be a config change, not a re-migration.**

## Decision

- **The part UID IS the serial.** The MIL-STD-130N IUID is Construct 1 —
  `IAC + EID + Serial` — with the existing `dp-<ULID>` part UID used *verbatim*
  as the serial. It passes `aberp-compliance::uid`'s field validation
  (alphanumeric + `-`, ≤ 50 chars), so no transformation and no re-mint: the
  exact existing string is carried forward. This is total continuity — every
  `dp-` UID, past and future, maps 1:1 to an IUID.
- **The IUID IRI is DERIVED, never persisted.** `part_marking::part_uid_to_iuid_iri`
  computes `IuidConstruct1::new(IUID_IAC, MOCK_ENTERPRISE_ID, part_uid).to_iri()`
  on read and surfaces it as the `PartMark::iuid` view field. The
  `wo_part_marks` schema and the stored DataMatrix payload are **unchanged** —
  the enterprise identifier is **never written to a row**. Therefore replacing
  the mock EID with the shop's real CAGE code is a **one-line config change**
  (`MOCK_ENTERPRISE_ID` in `apps/aberp/src/part_marking.rs`) that re-renders
  every IUID — existing and new — with **zero data migration**.
- **The mock EID is unmistakable.** `IUID_IAC = "D"` (the CAGE-code construct);
  `MOCK_ENTERPRISE_ID = "MOCK0"` — a CAGE-shaped placeholder that spells `MOCK`
  so it can never be read as a real CAGE code. A loud module banner and a unit
  test (`the_mock_enterprise_id_is_unmistakably_a_mock`) keep it that way.
- **The physical DataMatrix keeps the `dp-` continuity anchor.** The stored scan
  string stays `dp-<ULID>|serial|heat` (ADR-0089); the IUID is the derived
  MIL-STD-130N *expression* of the same part. Re-rendering the IUID into the
  physical mark once a real EID is assigned is a per-part re-mark decision that
  belongs to real production, not this pilot (there are no physically-marked
  parts yet — `[[defense-pilot-mode]]`).

## Consequences

Every part — every already-minted `dp-` UID included — now carries a valid
MIL-STD-130N IUID IRI in every read / trace / JSON, for free, the moment this
lands. Swapping the real EID is a config edit, not a migration or a re-mint. The
`uid` module's IUID vocabulary finally has a producer.

**What we lock in / what's still owed:** the mock EID is not a real identity —
it MUST be replaced before any production marking, and until then no IUID here
is a genuine DoD identifier. Construct 1 (serial unique across all enterprise
items) is assumed; if the shop needs Construct 2 (part-number-scoped serials)
that is a follow-on. Whether the physical DataMatrix should encode the IUID
(vs the `dp-` anchor) once a real EID exists is deferred (Open Q2).

## Adversarial review

1. **"A mock EID shipped as if it were an IUID is dangerous — someone reads
   `DMOCK0dp-…` as a real DoD identifier."** *Mitigated by construction.* The
   EID literally spells `MOCK`, a unit test pins that it stays a mock, and a
   loud module banner + this ADR flag it. It is inert data in a pilot with no
   physical parts; the guard is that it can never be *mistaken* for real, not
   that it is absent (the mock-everything rule wants a working mock, loudly
   labelled).
2. **"Using `dp-<ULID>` as the serial bakes an aberp-internal namespace tag
   into a DoD identifier."** *Accepted, deliberately.* Continuity was Ervin's
   explicit instruction: the serial must be the exact existing UID so no part is
   orphaned or re-minted. The `dp-` prefix is a valid UID-field character
   sequence; a future scheme that strips it is a superseding decision, but it
   would break the 1:1 continuity this ADR was told to preserve.
3. **"Deriving the IUID instead of storing it means a stale config silently
   changes historical IUIDs."** *This is the point, not a bug.* The IUID is a
   pure function of (config EID, IAC, stored serial); the serial is immutable,
   so the only thing a config change moves is the EID half — which is exactly
   the swap Ervin asked to be a config change. Historical *serials* (the
   stored, load-bearing identity) never move.

## Alternatives considered

- **Bake the IUID IRI into the stored DataMatrix payload (the backlog's
  original "render the IRI into the payload").** Lost: it persists the EID, so
  swapping the real EID would require re-migrating every stored row — the exact
  outcome Ervin ruled out. Deriving on read is what makes the swap config-only.
- **Re-mint existing parts as fresh IUIDs.** Lost: orphans the `dp-` history and
  breaks the trace chains — explicitly forbidden ("do NOT orphan or re-mint").
- **Block D-03 until a real CAGE EID is assigned.** Lost: Ervin directed a mock,
  loudly flagged, so the scheme ships now and the real EID drops in later.

## Open questions

- **Q1 — the real enterprise identifier.** The assigned CAGE / DoD EID must
  replace `MOCK_ENTERPRISE_ID` before production marking. Config change only.
- **Q2 — physical DataMatrix encoding.** Whether the physical mark should encode
  the IUID (vs the `dp-` anchor) once a real EID exists — a per-part re-mark
  question deferred to real production.
- **Q3 — Construct 1 vs 2.** Construct 1 (enterprise-wide unique serial) is
  assumed; a part-number-scoped Construct 2 is a follow-on if the shop needs it.
