# ADR-0060 — Stage 3 manufacturing-adapter framework (Phase α)

- **Status:** Accepted
- **Date:** 2026-06-03
- **Deciders:** Ervin (via S228 framework-α brief)

## Context

Stage 3 is the multi-year vision named in `project_aberp_stage3_manufacturing.md`:
when 3–4 DMG-Mori CNCs, a laser, robot transport, and a Renishaw quality gate
land on Áben's shop floor, ABERP becomes the orchestration brain — the
work-order dispatch, the audit ledger, the status board, the operations
dashboard. The hardware is **not here yet**. The integration landscape was
mapped in `docs/research/stage3/` (June 2026) and the architectural posture is
fixed: **open-standard-first, vendor-neutral, adapter-pattern**.

Phase α (this ADR) lays the **framework skeleton**: canonical event vocabulary,
the `Adapter` trait, an `AdapterRegistry`, audit-ledger integration, and a
`NoopAdapter` reference implementation. No real hardware code lands here.
Phase β will pick the first real adapter (most likely a barcode scanner per
the research README's standing recommendation — cheap, useful before any CNC
arrives). Subsequent phases add MTConnect / OPC-UA / Renishaw / robot
adapters as hardware arrives.

The constraints this ADR has to honour:

- **No database lock-in** — `feedback_no_sql_specific.md` extended by the
  Stage 3 memo: invariants in Rust code, no CHECK / triggers / stored procs,
  schema migrations as plain forward-only SQL. The adapter framework MUST
  NOT introduce a vendor-specific persistence layer.
- **Vertical integration** — `feedback_spacex_vertical_integration.md`:
  build the adapter layer in-house, no off-the-shelf MES SDK as load-bearing
  infra. Regulatory dependencies (NAV / DSGVO / CE) are the only exceptions;
  shop-floor protocol stacks are NOT in that list.
- **Type-system enforcement** — `feedback_trust_code_not_operator.md`: the
  framework should make wrong adapters **impossible to write** where the
  type system can carry the load. Canonical events are a closed Rust enum;
  storage strings are unit-test-pinned; the F12 four-edit ritual applies to
  the new `EventKind` variant.
- **Additive only** — no existing crate's contract changes. `audit-ledger`
  gains one new `EventKind` variant + the matching `as_str` /
  `from_storage_str` arms + the variants-array entry; no existing arm is
  edited.

## Decision

ABERP gains a new workspace member `crates/aberp-mes/` ("MES" for
Manufacturing Execution System — the standard industry term). The crate ships
four primitives, in this layering:

### 1. The canonical event vocabulary

A closed Rust `enum CanonicalEvent` with **six initial variants** drawn from
the Stage 3 memo's enumeration plus the research-package's data-item
categories (`docs/research/stage3/01-machine-protocols.md` §"Vocabulary",
§"OPC 40501 — Machine Tools"):

| Variant | Purpose | Phase that consumes it first |
|---|---|---|
| `PartMoved` | Robot / conveyor / scan-inferred movement of a physical part between two stations | Phase η (robot adapter) + Phase β (scanner-inferred) |
| `MachineStateChanged` | A CNC / laser machine transitioned between operational states (closed vocab: `Idle / Running / Setup / Down / Fault / Unknown`) | Phase ε (first DMG adapter) |
| `QualityResultReceived` | A measurement gate (Renishaw / on-machine probe / hand-gauge) emitted a pass/fail/hold-for-review outcome against a part | Phase ζ |
| `ScanReceived` | A barcode / QR scanner read a code at a station | Phase β |
| `WorkOrderStateChanged` | A work order transitioned between operational states (closed vocab: `Created / Released / InProgress / Completed / Cancelled / OnHold`) | Phase δ |
| `RobotTaskQueued` | A robot task was queued (description + priority); the *outcome* of the task surfaces as a downstream `PartMoved` | Phase η |

The vocabulary is **deliberately small** for α. Future variants land
incrementally when a real adapter needs them; adding one is a Rust enum
extension + serde round-trip test + audit-payload encoding pin, not a
breaking change to the audit-ledger crate (see §4 below).

`CanonicalEvent` is `serde::{Serialize, Deserialize}`-derived with
`#[serde(tag = "type", rename_all = "snake_case")]` — wire shape is a
self-describing JSON object whose discriminator is `type`. Closed-vocab
fields (`MachineState`, `QualityOutcome`, `WorkOrderState`) are also
`snake_case`-rename serde enums. No external schema registry; the Rust enum
IS the schema, and serde JSON round-trip tests pin the on-disk form.

### 2. The `Adapter` trait

A minimal async trait with five methods:

```rust
#[async_trait::async_trait]
pub trait Adapter: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &str;
    async fn start(&self) -> Result<(), AdapterError>;
    async fn stop(&self) -> Result<(), AdapterError>;
    fn health(&self) -> AdapterHealth;
    fn subscribe(&self) -> broadcast::Receiver<CanonicalEvent>;
}
```

- `name()` — stable identifier (typically `vendor-model-instance`,
  e.g. `dmg-mori-nmh-6300-cell-A`). Used as the registry key.
- `async fn start(&self)` — boot background tasks (TCP listener, MTConnect
  polling loop, MQTT subscriber). Returns once the adapter is up; does NOT
  block until cancellation. Idempotent — calling `start` while already
  started is a no-op (returns Ok).
- `async fn stop(&self)` — signal the background tasks to halt and await
  their completion. Idempotent.
- `fn health(&self) -> AdapterHealth` — sync snapshot. Adapter tracks state
  internally; this method just reads cached state. Three states:
  `Starting / Healthy / Degraded { reason } / Unhealthy { reason } / Stopped`.
- `fn subscribe(&self) -> broadcast::Receiver<CanonicalEvent>` — fresh
  receiver per call. Multiple consumers (the ledger writer, future SPA push,
  future operations-dashboard projection) can subscribe independently.

**Why `start`/`stop` instead of a single `run(cancel)`**: the brief
explicitly pins this shape, and it isolates the adapter's task management
inside the impl. Adapter authors who need a cancellation token construct
their own `CancellationToken` internally; the trait doesn't expose it. This
matches the "stateful service" shape rather than the "blocking future"
shape (which `run(cancel)` would be).

**Why `broadcast` over `mpsc`**: matches ADR-0006 §"Event bus" Phase 1
posture (in-process broadcast-style bus). Multiple consumers per adapter
event stream is the expected shape (ledger writer + future SPA pushers).
broadcast's lossiness for slow consumers is acceptable for shop-floor
telemetry — the next event catches up.

**Why `#[async_trait]` rather than native AFIT**: native async-fn-in-trait
is dyn-incompatible without `trait_variant` or `Box<dyn Future>` ergonomics.
The workspace already pins `async-trait = "0.1"` (PR-60); reusing it keeps
the adapter trait `Box<dyn Adapter>`-compatible for the registry.

### 3. The `AdapterRegistry`

A struct holding `HashMap<String, Arc<dyn Adapter>>` plus a few
batch-lifecycle helpers:

```rust
pub struct AdapterRegistry { /* private */ }

impl AdapterRegistry {
    pub fn new() -> Self;
    pub fn register(&mut self, adapter: Arc<dyn Adapter>) -> Result<(), RegistryError>;
    pub fn unregister(&mut self, name: &str) -> Option<Arc<dyn Adapter>>;
    pub fn get(&self, name: &str) -> Option<Arc<dyn Adapter>>;
    pub fn names(&self) -> Vec<String>;
    pub fn health(&self) -> HashMap<String, AdapterHealth>;
    pub async fn start_all(&self) -> Vec<(String, Result<(), AdapterError>)>;
    pub async fn stop_all(&self) -> Vec<(String, Result<(), AdapterError>)>;
}
```

- `register` rejects duplicate-name registrations with
  `RegistryError::DuplicateName` (closed-vocab; never silently overwrites).
- `unregister` returns the removed adapter so the caller can `stop()` it.
- The registry is **runtime state**, not persisted. Per `[[no-sql-specific]]`
  extended by the Stage 3 memo: adapter membership belongs in code (boot
  config + dynamic registration), not in a DuckDB table. A future
  `aberp-mes-config` crate may load adapter definitions from operator
  configuration (TOML), but that lives outside this ADR and outside the
  audit-ledger.

### 4. Audit-ledger integration

The framework records every emitted `CanonicalEvent` as a single audit-ledger
entry of kind `EventKind::MesAdapterEvent` (storage string
`mes.adapter_event`).

**One kind, payload-discriminator design**: the canonical event vocabulary
evolves in `aberp-mes` (Rust enum extension); the audit-ledger crate's kind
list does NOT balloon to one variant per event subtype. The payload struct
`MesAdapterEventPayload` carries the typed `CanonicalEvent` plus the
adapter's `name` (so the audit trail names WHO emitted the event) plus the
operator-decision idempotency key (the F8 pattern — same `ulid` shape used
by every other system-prefixed kind).

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MesAdapterEventPayload {
    pub adapter_name: String,
    pub idempotency_key: String,
    pub event: CanonicalEvent,
}
```

**Storage prefix `mes.`** — a new prefix family alongside `invoice.` (per
outgoing-invoice surface) and `system.` (everything else system-lifecycle).
Rationale:

- Stage 3 will accumulate many subtypes over years; segregating them under
  `mes.*` makes future operations-dashboard projections trivially globbable.
- `system.*` consumers (per-OUTGOING-invoice export bundle's exclusion glob,
  the AP-side query helpers) don't get accidentally swept by MES traffic.
- Future MES-side export bundles can use a `mes.*` glob mirror of the
  per-invoice bundle pattern.

A new prefix-pin test guards this — `mes.adapter_event` MUST start with
`mes.` and MUST NOT start with `invoice.` or `system.`. Future Stage 3 PRs
that add additional `EventKind` variants for MES sub-surfaces (e.g. an
adapter-registered event distinct from per-event-recording) should keep the
prefix.

The F12 four-edit ritual fires once for this PR: variant + `as_str` arm +
`from_storage_str` arm + the variants array in `round_trip_for_every_variant`.
A new prefix test in the same module pins the `mes.` discriminator.

The actual audit append happens through `audit-ledger`'s existing
`append_in_tx(tx, meta, EventKind::MesAdapterEvent, payload_bytes, actor,
Some(idempotency_key))` — same write surface every other audit producer uses.
No new write API on the audit-ledger crate.

### 5. The `NoopAdapter` reference implementation

A complete impl of `Adapter` that does nothing — `start`/`stop` flip an
internal `AtomicU8` state, `health()` reads it, `subscribe()` returns a
receiver that never produces events. Two purposes:

- A stub the framework's own tests instantiate (lifecycle, registry, health).
- A reference implementation future adapter authors copy as a starting
  point: it shows the minimal shape that satisfies the trait without
  pulling in any external protocol crate. The README walks through it.

### 6. Migration safety — additive only

This PR touches the audit-ledger crate by **adding** one variant. No
existing arm is edited; no existing pin test changes meaning; the
round-trip variants array grows by one. The aberp-mes crate is new.
The workspace `Cargo.toml` members list grows by one. The downstream
binary `apps/aberp` and the verifier `crates/aberp-verify` are NOT touched
by this PR — the new event kind is declared in the audit-ledger but no
production code path emits it yet (NoopAdapter exists but is never
registered into a real registry). β will land the first real registration
+ the runtime task that subscribes to adapter streams and writes to the
ledger.

### 7. Offline-first per-cell architecture — deferred

The Stage 3 memo names "offline-first per cell" as a hard constraint:
each cell (CNC + robot + Renishaw + packaging) keeps working if the
central ABERP is briefly unreachable. The architectural shape that
delivers this — a local audit queue at the cell controller plus a
sync-when-reconnected mechanism between cell ABERP and central ABERP — is
**deliberately out of scope for this ADR**. It's a non-trivial design
that depends on the cell controller hardware choice
(`docs/research/stage3/05-cell-controllers.md` covers the candidates) and
on the SaaS-migration topology
(`[[aberp-saas-migration]]` named in the Stage 3 memo).

This ADR establishes the **single-process** assumption: there is one
ABERP process, one audit ledger, one registry. Adapters live in-process
with the audit-ledger writer. The offline-first ADR — call it tentatively
ADR-NNNN — gets filed when the first cell-controller hardware ships and
the topology question becomes load-bearing.

**Constraint flagged for future-ADR consumers**: nothing in this ADR's
type design forecloses an offline-first topology. The `Adapter` trait
talks to a `broadcast` channel, not to a database connection. A future
"adapter runs on cell controller, ledger writer runs on central ABERP"
split replaces the in-process broadcast with a network transport without
touching adapter author code.

## Consequences

- **Adding the next adapter is mechanical.** A future
  `crates/aberp-adapter-mtconnect` (or similar per-vendor crate) impls
  `Adapter`, registers itself with the registry at boot, and is done. The
  audit-ledger interaction is shared infrastructure.
- **The audit-ledger crate stays small.** One `EventKind` variant + one
  payload struct + one prefix pin is the entire surface this ADR adds. The
  canonical event vocabulary's growth lives in `aberp-mes`, not in
  `audit-ledger`.
- **Future canonical-event additions don't fire F12.** A new variant on
  `CanonicalEvent` is a Rust enum extension + serde round-trip test +
  matching changes downstream (consumer projections). The audit-ledger
  payload bytes are self-describing JSON; old entries deserialize unchanged.
- **Vendor swap is mechanical.** If DMG drops MTConnect and re-platforms
  on OPC-UA-only in 2030, the adapter implementation is swapped; the
  canonical event vocabulary stays. The cost lands inside one crate, not
  scattered across the codebase.
- **Type-system enforcement is partial.** A future adapter author who
  writes a string-typed event class instead of using `CanonicalEvent` will
  not be caught at the type level — the framework can't stop someone from
  bypassing it. The README + future code review catches this.
- **broadcast lossiness is real.** A slow consumer (e.g. a stalled audit
  writer) loses events from the broadcast tail when the channel fills. For
  shop-floor telemetry this is acceptable (the next event from the same
  machine catches up). For one-shot events (PartMoved, QualityResultReceived)
  it's a problem; the registry's ledger-writer task must size the channel
  generously and log when receiver lag fires.
- **Registry is not persisted.** Operator-supplied adapter configuration
  loads at boot (from a future TOML section) and is registered into a
  fresh registry. There is no "list of registered adapters" table that
  could go stale relative to the running process.

## Adversarial review

- *"Why not pick OPC-UA-or-MTConnect at the framework level — make the
  adapter trait protocol-aware?"* — Because the framework MUST stay
  protocol-neutral per the vertical-integration posture. Hard-coding
  OPC-UA into the trait would lock the entire ABERP into a protocol the
  Stage 3 research deliberately refused to commit to before hardware lands.
  The adapter is the protocol-neutral boundary; protocol specifics live
  inside per-vendor adapter crates.
- *"One EventKind for all MES events is too coarse — how does the
  operator filter the audit ledger?"* — The payload's
  `event.type` discriminator is the filter. A SQL query like
  `SELECT payload FROM audit_ledger WHERE kind = 'mes.adapter_event' AND
  json_extract(payload, '$.event.type') = 'machine_state_changed'`
  resolves it. We're trading one-kind-per-subtype (which would explode the
  F12 ritual surface and bake the vocab into audit-ledger) for one-kind +
  structured payload. Same trade the existing
  `IncomingInvoiceStatusChanged` (from/to status as payload fields) and
  `InvoiceSubmissionAttemptFailed` (error_class as payload field) made.
- *"`mes.` prefix is a third prefix family — was `system.` rejected
  on principle?"* — Not on principle, on segregation. The `system.*` glob
  is currently the catch-all for non-invoice events; sweeping Stage 3
  events into it would force every existing `system.*` consumer to know
  the difference between "AP sync cycle completed" and "robot reported
  arm position." Separating them at the prefix layer keeps each consumer's
  pattern globble narrow. Future Stage 3 sub-surfaces stay under `mes.*`.
- *"broadcast lossiness on the ledger-writer path will lose audit
  entries. Isn't that an integrity violation?"* — Yes if the channel
  overflows. The mitigation: ledger writer must size the receiver
  generously (the brief uses default `bounded(1024)` per-adapter) AND
  log a loud WARN every time `RecvError::Lagged` fires AND emit a counter
  the future operations dashboard surfaces. β will land the actual writer
  task and pin the size + lag-detection. If lossiness becomes a real
  operational concern, switch the per-adapter channel to mpsc (single
  consumer = no lossiness; subscribers wishing fan-out get it from the
  registry's combined stream).
- *"NoopAdapter has no real value — why ship it?"* — Two reasons. (1) The
  framework's own tests need a non-trivial `dyn Adapter` to exercise the
  registry without coupling to a real protocol crate. (2) Future adapter
  authors copy it as a starting point; without a reference impl they
  invent the lifecycle pattern from scratch and probably get the
  `AtomicU8` state-tracking wrong.
- *"Why is offline-first deferred? Isn't it a hard requirement from the
  Stage 3 memo?"* — It's a hard requirement when first cell hardware
  ships. Before any hardware ships, designing the topology forces choices
  on hardware that hasn't been bought yet (cell controller spec, network
  segmentation, ABERP's SaaS topology). Per `[[think-then-act]]`, the
  conservative move is to flag the constraint and design the trait such
  that an offline-first split is achievable later, not to commit to a
  topology now. The trait shape (adapter → broadcast → ledger writer)
  factors cleanly into "adapter on cell controller, broadcast over
  network, ledger writer on central ABERP" without touching adapter
  author code.

## Alternatives considered

- **One EventKind per canonical event subtype** (e.g.
  `MesPartMoved`, `MesMachineStateChanged`, ...). Refused — the F12 ritual
  would fire once per subtype every time the vocabulary grew, baking the
  Stage 3 vocab into the audit-ledger crate. Existing pattern (e.g.
  `IncomingInvoiceStatusChanged` with from/to as payload fields) already
  validates the one-kind-with-structured-payload posture.
- **Adapter trait with `run(cancel)`** instead of `start`/`stop`. Refused
  per brief — `start`/`stop` matches operator-mental-model of services and
  isolates cancellation tokens inside the adapter implementation. The cost
  is each adapter author manages its own `JoinHandle` or `CancellationToken`;
  cheap.
- **mpsc per-adapter** instead of broadcast. Refused — multiple consumers
  per stream is the expected shape (ledger writer + future SPA + future
  operations-dashboard projection). If broadcast lossiness ever bites,
  switch is mechanical (single-receiver mpsc + a fan-out layer above).
- **Persisted adapter registry** (a `mes_adapter` DuckDB table). Refused
  per `[[no-sql-specific]]` — adapter membership is runtime state, derived
  from boot config + dynamic register/unregister calls. A persisted table
  would introduce a "two sources of truth" problem (table vs in-memory
  registry).
- **Defer the ADR to Phase β** (when the first real adapter lands). Refused
  per the brief — Phase α's value is locking the trait shape + canonical
  vocabulary BEFORE three different protocol authors invent three different
  shapes and force a refactor.
- **Use the trait_variant crate** for AFIT instead of async-trait. Refused
  — async-trait is already in the workspace and dyn-compatible without
  ceremony; introducing a second async-trait dependency would split the
  pattern across the codebase.

## Open questions

- **Cell-controller topology + offline-first ADR.** Triggers: first
  cell-controller hardware specced + the SaaS-migration topology firms up.
  Likely numbered later than ADR-0060.
- **Operator-configurable adapter registration.** Where does the boot-time
  list of adapters come from? Likely a new `[mes]` section in `seller.toml`
  (becoming the 7th preservation slot per the
  `[[seller.toml-write-invariant]]` family). Filed alongside Phase β when
  the first real adapter needs configuration.
- **broadcast channel sizing + lag detection.** Phase β will pin the
  per-adapter channel size and the lag-detection WARN cadence. Default
  `bounded(1024)` is a starting point; real adapter event rates will tune
  it.
- **Operations dashboard projection.** OEE
  (`docs/research/stage3/07-oee-mes-metrics.md`) eventually consumes the
  audit-ledger MES entries and surfaces availability × performance ×
  quality. Filed separately when the financial dashboard's sibling
  operations dashboard becomes a buildable surface.
- **Command surface (writing back to adapters).** This ADR pins
  read-only event emission. Future bidirectional control (ABERP sends
  a work-order dispatch command to a CNC over OPC-UA) requires an
  additional trait method or a paired `AdapterCommand` enum. Deferred per
  the brief — "don't pre-design commands; we'll add when needed."
- **Type-system enforcement for `idempotency_key`.** Currently a `String`
  on the payload; a `IdempotencyKey` newtype with format validation would
  be stronger. Filed alongside the audit-ledger's wider newtype-tightening
  pass; not blocking.
