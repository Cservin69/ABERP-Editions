# `aberp-mes` — Stage 3 manufacturing-adapter framework (Phase α)

The bones for ABERP's shop-floor integration: a closed canonical-event
vocabulary, a minimal `Adapter` trait, a runtime registry, and the
audit-ledger integration. **No real hardware code lives here.** Phase β
(next session in this strand) will add the first real adapter — most
likely a barcode scanner per the Stage 3 research's standing
recommendation.

For the architectural decision and rationale, read
[ADR-0060](../../adr/0060-stage3-manufacturing-adapter-framework.md).
For the protocol landscape, read
[docs/research/stage3/](../../docs/research/stage3/).

## What's in the box

| Module | What it exports | Purpose |
|---|---|---|
| `events` | `CanonicalEvent`, `MachineState`, `QualityOutcome`, `WorkOrderState` | The closed enum vocabulary every adapter emits |
| `adapter` | `Adapter` trait, `AdapterHealth` | The async contract a vendor-specific adapter implements |
| `registry` | `AdapterRegistry` | Runtime map of registered adapters; `start_all` / `stop_all` / `health` snapshots |
| `noop` | `NoopAdapter` | Reference implementation — no real protocol, useful for tests + as a copy-paste starting point |
| `audit` | `MesAdapterEventPayload`, `write_mes_adapter_event`, `audit_kind_string` | Audit-ledger integration — turns a `CanonicalEvent` into an `EventKind::MesAdapterEvent` entry |
| `error` | `AdapterError`, `RegistryError` | Closed-vocab errors |

## The next adapter author's first hour

You're writing a new adapter — e.g. an MTConnect adapter for a DMG-Mori
NMH 6300. The minimum you need:

1. **Pick a name.** Stable identifier; typically `vendor-model-instance`,
   e.g. `dmg-mori-nmh-6300-cell-A`. This is the registry key and the
   `adapter_name` field on every audit-ledger entry the adapter
   produces. Names ARE on-disk strings — don't change them after the
   adapter has emitted events.

2. **Create a new workspace crate.** `crates/aberp-adapter-mtconnect/`
   (or whatever vendor). Depend on `aberp-mes` for the trait + event
   types. Don't re-implement `CanonicalEvent`.

3. **Implement `Adapter`.** The skeleton is in
   [`src/noop.rs`](src/noop.rs) — copy it and replace the inert
   body with real protocol code:

   ```rust
   #[async_trait::async_trait]
   impl Adapter for MyAdapter {
       fn name(&self) -> &str { &self.name }

       async fn start(&self) -> Result<(), AdapterError> {
           // Spawn the background polling / TCP listener / MQTT
           // subscriber. Return Ok once tasks are spinning.
           Ok(())
       }

       async fn stop(&self) -> Result<(), AdapterError> {
           // Signal cancellation, await the background tasks.
           Ok(())
       }

       fn health(&self) -> AdapterHealth {
           // Read your internal AtomicU8 / Mutex<State> / etc.
           AdapterHealth::Healthy
       }

       fn subscribe(&self) -> broadcast::Receiver<CanonicalEvent> {
           self.broadcast_sender.subscribe()
       }
   }
   ```

4. **Translate vendor dialect into `CanonicalEvent`.** This is the only
   place your adapter is allowed to know about MTConnect XML / OPC-UA
   nodes / Renishaw probe output. Everything outside your adapter
   crate sees the canonical vocabulary, never the vendor's wire shape.

   Example: an MTConnect `<Execution>` data item transitioning from
   `READY` to `ACTIVE` becomes a `CanonicalEvent::MachineStateChanged`
   with `previous_state: Idle`, `new_state: Running`.

5. **Don't invent new event types.** If your vendor emits something
   the canonical vocab doesn't cover, push back into ABERP-design
   discussion before you add a string field or a struct payload.
   Extending `CanonicalEvent` is a closed-process: a new variant lands
   via a Rust enum extension + a serde round-trip test + an
   architectural decision (the variant's audit-ledger entry is locked
   to `EventKind::MesAdapterEvent`, so the kind doesn't bump, but the
   downstream consumers — operations dashboard projection, SPA UI —
   need to learn the new type).

6. **Register at boot.** A future Phase β PR will wire boot-time
   adapter registration (likely from a `[mes]` section in
   `seller.toml`). For now, the framework's tests show the manual
   shape:

   ```rust
   let mut registry = AdapterRegistry::new();
   let adapter: Arc<dyn Adapter> = Arc::new(MyAdapter::new(config));
   registry.register(adapter)?;
   registry.start_all().await;
   ```

7. **Subscribe + write to the ledger.** Each registered adapter has a
   broadcast receiver. A future runtime task (also β-phase) drains the
   receivers and writes via `write_mes_adapter_event`:

   ```rust
   let mut rx = adapter.subscribe();
   while let Ok(event) = rx.recv().await {
       let payload = MesAdapterEventPayload::new(
           adapter.name(),
           Ulid::new().to_string(),  // operator-decision idempotency
           event,
       );
       let tx = conn.transaction()?;
       write_mes_adapter_event(&tx, &meta, actor.clone(), &payload)?;
       tx.commit()?;
   }
   ```

## Rules adapter authors MUST follow

These come from the ABERP architecture and the Stage 3 memo. Breaking
any of them is a code-review red flag.

- **Speak only `CanonicalEvent`.** Vendor strings stay inside your
  adapter crate. The audit-ledger sees the typed event, not raw XML.
- **No DB writes from your adapter.** The audit-ledger is the only
  durable side-effect surface. Don't open your own DuckDB connection;
  don't write to disk except for vendor-specific transient state.
- **No secrets in `CanonicalEvent`.** Credentials, tokens, certificate
  bytes — never in the payload. The ledger keeps entries forever; a
  leaked secret cannot be redacted.
- **`name()` MUST be stable.** Renaming an adapter post-launch breaks
  the audit-ledger's join-by-name on every entry the adapter has
  emitted. If a rename is forced (e.g. operator moves a machine
  between cells), file an ADR.
- **`health()` is sync.** Don't make it an async function that probes
  the upstream — track state internally and read cached state. Probing
  on every health query is a denial-of-service against your own
  upstream.
- **Fail loud, not silent.** If a vendor message fails to parse, emit
  a `MachineStateChanged` with `MachineState::Unknown` or log at
  `error!` — never silently drop. Per CLAUDE.md rule 12, silent
  fallback is the worst-class failure mode.
- **No DB-vendor lock.** Per `[[no-sql-specific]]` extended by the
  Stage 3 memo: invariants in Rust code, not as DuckDB CHECK / triggers
  / stored procs. Audit-ledger schema migrations are plain forward-only
  SQL.
- **No CLI subcommands in your adapter crate.** Operator surfaces
  belong in the `apps/aberp` binary, not in the adapter crate. Your
  crate exposes a Rust library only.

## Lifecycle invariants

The framework gives adapter authors three guarantees:

1. `start()` and `stop()` are called on a single-threaded path through
   the registry's `start_all` / `stop_all`. You won't get a concurrent
   `start` while a prior `start` is mid-flight — the registry
   serializes.
2. `subscribe()` may be called concurrently from multiple consumers;
   your impl must be thread-safe (`Send + Sync` is required by the
   trait).
3. `health()` is called frequently — once per registry health-snapshot
   call. Keep it cheap. No locks; an `AtomicU8` is enough for most
   adapters.

What the framework does NOT guarantee:

- **Event ordering across adapters.** Two adapters firing
  simultaneously appear in the audit ledger in the order their
  per-broadcast receivers were drained — which is not necessarily the
  order they emitted. For per-machine ordering, subscribe to that
  machine's adapter alone.
- **No event loss.** The broadcast channel is bounded; a slow consumer
  loses tail events when the channel fills. Phase β will pin the size
  + lag-detection. If your adapter emits in bursts, size the channel
  generously.
- **Idempotency on the wire.** If your vendor re-sends an event (TCP
  retry, MQTT QoS 1), your adapter must dedupe. The
  `idempotency_key` on the payload is operator-decision, not wire-level.

## What stays out of an adapter crate (Phase α scope)

- **Bidirectional control / commands.** No write-back to the machine
  yet. The trait will gain an `AdapterCommand` enum + `dispatch` method
  in a future PR when the first real adapter needs it.
- **Operator configuration surface.** No TOML parsing in your adapter;
  Phase β lands the `[mes]` `seller.toml` section.
- **UI / SPA surface.** No HTTP routes, no Svelte components. The
  registry's `health()` snapshot will surface through a future
  `/api/mes/health` route; the adapter author doesn't write the route.
- **Cell-controller / offline-first split.** Phase α assumes a single
  ABERP process. The offline-first ADR lands when first cell hardware
  ships and the SaaS-migration topology firms up.

## Where to read next

- [ADR-0060](../../adr/0060-stage3-manufacturing-adapter-framework.md)
  — the architectural decision behind this crate.
- [Stage 3 research package](../../docs/research/stage3/) — protocol
  landscape (MTConnect, OPC-UA, Renishaw, robot controllers, scanners,
  cell controllers, OEE, FMS, laser).
- [ADR-0006](../../adr/0006-module-boundaries.md) — module contract
  conventions (command vs event, the event-bus model, port/adapter
  layering).
- [ADR-0008](../../adr/0008-audit-ledger.md) — audit-ledger entry
  shape, hash chain, attestation cadence.
