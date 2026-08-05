//! [`TrumpfAdapter`] — Trumpf laser-cutter adapter, built as a thin
//! **backend seam** ([`TrumpfSource`]) whose v1 implementation routes
//! through the already-shipped MTConnect wire path.
//!
//! ## Why a seam instead of a protocol stack
//!
//! Trumpf's own published integration path is OPC UA (natively on newer
//! TruLaser controls, via the OPC UA Retrofit Extension Cube on older
//! ones), with job-level data living in Oseon / TruTops Fab rather than
//! on the machine. Our vendor survey
//! (`docs/research/stage3/09-laser-workflow.md`) found **no** first-party
//! MTConnect support — in practice a Trumpf reaches MTConnect only via a
//! gateway (MachineMetrics, Memex Merlin, an OPC-UA→MTConnect bridge).
//!
//! Committing to OPC UA today would mean adding this workspace's first
//! OPC-UA crate tree — a large, security-sensitive dependency with its
//! own certificate / trust-list / security-policy handshake — into a
//! Defense-line binary, plus a per-machine browse-and-map step (Trumpf
//! node IDs are not standardised across models). That is a dependency
//! decision, not a wire-format decision, and it is not reversible
//! cheaply.
//!
//! So this adapter splits the two decisions apart:
//!
//! - **[`TrumpfSource`]** is the named integration point — "one poll,
//!   one normalised [`LaserSnapshot`]". It is the ONLY thing that knows
//!   a protocol.
//! - **[`MtconnectLaserSource`]** is the v1 backend. It reuses
//!   [`mtconnect::poll_once`](crate::adapters::mtconnect) and
//!   [`parse_mtconnect_current`](crate::adapters::mtconnect) verbatim —
//!   **zero new dependencies**, and the DoS bounds / error
//!   classification are literally the same code, not a copy.
//! - **[`OpcUaLaserSource`] / [`OseonLaserSource`]** are declared,
//!   documented, and deliberately unimplemented (§"v2 backends").
//!
//! Swapping the backend later is one `impl TrumpfSource` block. The
//! adapter, the registry, `build_adapter`, the ledger writer, the health
//! monitor, and the SPA all stay untouched — they only ever see
//! `Arc<dyn Adapter>`.
//!
//! ## Honest scope note
//!
//! Per `09-laser-workflow.md` §"where the time actually goes", the
//! manual downstream ops (bend / weld / deburr) dominate the real shop
//! time budget, and those are already covered by the shipped
//! [`BarcodeScannerAdapter`](crate::BarcodeScannerAdapter). This adapter
//! is a clean seam and a modest telemetry win — it is not the
//! shop-floor-value centrepiece, and it is scoped accordingly.
//!
//! ## Event emission
//!
//! Edge-triggered, exactly like the MTConnect adapter, over the SAME
//! [`CanonicalEvent`] vocabulary — **no new [`CanonicalEvent`] variants
//! and no new audit `EventKind`s.** Everything rides the existing
//! `mes.adapter_event` ledger row, so the `ALL_KINDS_COUNT` pin is
//! untouched.
//!
//! | Edge | Emitted |
//! |---|---|
//! | `machine_state` changed | [`CanonicalEvent::MachineStateChanged`] |
//! | job `None → Some(b)` | [`CanonicalEvent::WorkOrderStateChanged`] `b: Released → InProgress` |
//! | job `Some(a) → None` | `a: InProgress → Completed` |
//! | job `Some(a) → Some(b)`, `a != b` | **two** events: `a: … → Completed` **then** `b: Released → InProgress` |
//!
//! The first observation emits `Unknown → X` as the machine-state
//! baseline (same contract as `mtconnect`), and treats a job seen on the
//! first poll as a start edge.
//!
//! ### `work_order_id` is a VENDOR string, not an ABERP work-order id
//!
//! The `work_order_id` field on an emitted `WorkOrderStateChanged`
//! carries the vendor's job identity **verbatim** — for the MTConnect
//! backend that is the NC program / nest name. It is NOT resolved
//! against ABERP's `wo_*` ids, and nothing downstream treats it as one:
//! the only consumer of this broadcast is
//! [`spawn_ledger_writer`](crate::spawn_ledger_writer), which records the
//! payload as an audit row. Inventing a mapping without knowing the
//! target system's id shape would be guessing, so v1 records and stops.
//!
//! ## What v1 does NOT do
//!
//! - **No writes to work orders / inventory / QA / dispatch.** No adapter
//!   in this tree does that today; the adapter → broadcast → audit-ledger
//!   path is the whole contract.
//! - **No bidirectional control** — no job dispatch *to* the laser. The
//!   [`Adapter`] trait has no write surface.
//! - **No nest-report / CAM file ingestion** — separate strand.
//! - **No `MachineState::Setup`** via the MTConnect backend: MTConnect's
//!   `Execution` vocabulary has no setup/sheet-change value, and
//!   inferring one from `ControllerMode` would be exactly the silent
//!   misclassification the closed map exists to prevent. A backend that
//!   genuinely distinguishes setup can populate it through the same
//!   seam.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::adapter::{Adapter, AdapterHealth};
use crate::adapters::common::AdapterLifecycle;
use crate::adapters::mtconnect::{map_execution_to_state, poll_once};
use crate::error::AdapterError;
use crate::events::{CanonicalEvent, MachineState, WorkOrderState};

/// Default TCP port of the MTConnect gateway fronting the laser. Same
/// well-known Agent port the CNC adapter uses — the v1 backend IS an
/// MTConnect Agent.
pub const DEFAULT_AGENT_PORT: u16 = 5000;

/// Default interval between consecutive polls. Matches the MTConnect
/// adapter's cadence; a laser's state changes on the order of minutes,
/// so 5s is ample resolution.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default hard cap on a single backend poll. Set BELOW
/// [`DEFAULT_POLL_INTERVAL`] so a stalled request cannot pile up across
/// ticks.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

/// Default threshold above which a successful poll reports `Degraded`
/// rather than `Healthy`.
pub const DEFAULT_SLOW_THRESHOLD: Duration = Duration::from_secs(2);

/// Default broadcast channel capacity. Matches the other adapters.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Default cap on a single backend response body.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Default device path when the config omits one. Mirrors the MTConnect
/// adapter's default so a laser behind a gateway with one device works
/// out of the box.
pub const DEFAULT_DEVICE_NAME: &str = "default";

/// MTConnect's sentinel for "this data item currently has no value".
///
/// Load-bearing: an Agent publishes `UNAVAILABLE` for `Program` whenever
/// no NC program is loaded, so taking the raw string as a job identity
/// would mint a phantom work order named `UNAVAILABLE` — and then a
/// second phantom "completion" when a real program loads. Normalised
/// away in [`normalise_data_item`].
pub const MTCONNECT_UNAVAILABLE: &str = "UNAVAILABLE";

/// One poll's worth of "what is the laser doing right now", normalised
/// away from whichever backend produced it.
///
/// This is the seam's data contract. Every field except `machine_state`
/// is optional because backends expose wildly different amounts: a
/// Basic-Connectivity-Kit Trumpf publishes three status signals and
/// nothing else, while Oseon knows the order, the nest, and the sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaserSnapshot {
    /// Closed-vocab machine state. [`MachineState::Unknown`] whenever the
    /// backend reported something unrecognised or nothing at all — never
    /// a plausible-looking default.
    pub machine_state: MachineState,
    /// Vendor job identity, verbatim. See the module docs: this is NOT
    /// an ABERP work-order id.
    pub active_job_id: Option<String>,
    /// NC program / nest name, when the backend exposes it.
    pub program_name: Option<String>,
    /// Sheet / material designation, when the backend exposes it. No
    /// MTConnect Streams data item carries this, so the v1 backend
    /// always reports `None`; Oseon would populate it.
    pub material_ref: Option<String>,
    /// Good-parts counter, when the backend exposes it.
    pub piece_count: Option<u64>,
}

/// A snapshot with nothing observed. `machine_state` defaults to
/// [`MachineState::Unknown`] — never to a plausible-looking `Idle`. Hand-
/// written rather than derived because the shared [`MachineState`] vocab
/// deliberately carries no `Default` impl of its own.
impl Default for LaserSnapshot {
    fn default() -> Self {
        Self {
            machine_state: MachineState::Unknown,
            active_job_id: None,
            program_name: None,
            material_ref: None,
            piece_count: None,
        }
    }
}

/// The Trumpf integration seam.
///
/// One method, one poll, one normalised snapshot. Implementations own
/// **all** protocol knowledge — transport, framing, vocabulary mapping,
/// and their own DoS bounds. The adapter around them owns lifecycle,
/// cadence, health, and edge-triggered emission, and is backend-agnostic.
///
/// The `Err` string is operator-readable and lands verbatim in
/// [`AdapterHealth::Unhealthy`], so it MUST NOT carry credential bytes.
#[async_trait]
pub trait TrumpfSource: Send + Sync + std::fmt::Debug {
    /// Poll the machine once. Returns the current snapshot, or an
    /// operator-readable failure reason.
    async fn poll(&self) -> Result<LaserSnapshot, String>;

    /// Short label naming the backend, for logs and health context
    /// (e.g. `"mtconnect"`). Display only.
    fn backend(&self) -> &'static str;
}

/// **v1 backend.** Routes the laser through the shipped MTConnect wire
/// path: the same `poll_once` (both response-size caps, the same reqwest
/// error classification), the same `parse_mtconnect_current`, and the
/// same closed `Execution` → [`MachineState`] table.
///
/// Requires an MTConnect Agent or gateway in front of the machine — see
/// the module docs. Zero new dependencies.
#[derive(Debug)]
pub struct MtconnectLaserSource {
    client: reqwest::Client,
    url: String,
    max_response_bytes: usize,
}

impl MtconnectLaserSource {
    /// Build the backend from an adapter config. Fails only if the HTTP
    /// client cannot be constructed.
    pub fn new(config: &TrumpfAdapterConfig) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .timeout(config.request_timeout)
            .build()
            .map_err(|e| format!("build HTTP client: {e}"))?;
        Ok(Self {
            client,
            url: config.current_url(),
            max_response_bytes: config.max_response_bytes,
        })
    }
}

#[async_trait]
impl TrumpfSource for MtconnectLaserSource {
    async fn poll(&self) -> Result<LaserSnapshot, String> {
        let (snapshot, _elapsed) =
            poll_once(&self.client, &self.url, self.max_response_bytes).await?;

        // The gateway republishes the laser's program/nest name in the
        // `Program` data item; that is the only job identity an
        // MTConnect wire exposes. `UNAVAILABLE` and blanks are NOT
        // identities.
        let program = normalise_data_item(snapshot.program);

        Ok(LaserSnapshot {
            machine_state: snapshot
                .execution
                .as_deref()
                .map(map_execution_to_state)
                .unwrap_or(MachineState::Unknown),
            active_job_id: program.clone(),
            program_name: program,
            // No MTConnect Streams data item carries the sheet/material
            // designation. Left absent rather than guessed.
            material_ref: None,
            piece_count: snapshot.part_count,
        })
    }

    fn backend(&self) -> &'static str {
        "mtconnect"
    }
}

/// **v2 backend — declared, not implemented.** Trumpf's published path:
/// OPC UA natively on newer TruLaser controls, or via the OPC UA
/// Retrofit Extension Cube on older ones.
///
/// Landing this means (1) adding this workspace's first OPC-UA
/// dependency tree, (2) a certificate / trust-list / security-policy
/// handshake — a real credential-handling surface, and (3) a per-machine
/// browse-and-map step, because Trumpf node IDs are not standardised
/// across models. The first implementation step is an address-space
/// capture from the target machine.
///
/// It returns a loud `Err` rather than panicking: a `todo!()` here would
/// abort the spawned poll task inside a Defense binary, whereas an `Err`
/// surfaces as `Unhealthy { reason }` on the Adapters page — visible,
/// per CLAUDE.md rule 12, without being fatal. It is unreachable from
/// operator config regardless: `build_adapter` never constructs it.
#[derive(Debug, Default)]
pub struct OpcUaLaserSource;

#[async_trait]
impl TrumpfSource for OpcUaLaserSource {
    async fn poll(&self) -> Result<LaserSnapshot, String> {
        Err("OPC UA laser backend is not implemented in v1 \
             (needs an OPC-UA dependency + an address-space capture \
             from the target machine)"
            .to_string())
    }

    fn backend(&self) -> &'static str {
        "opc-ua"
    }
}

/// **v2 backend — declared, not implemented.** Oseon / TruTops Fab is
/// where the *job-level* data actually lives (order linkage, nest
/// identity, completion) — closer to what ABERP wants than machine
/// telemetry. Blocked on a deployment: availability, API surface, and
/// auth model are all shop-specific and possibly commercially gated.
///
/// Same non-panicking posture as [`OpcUaLaserSource`].
#[derive(Debug, Default)]
pub struct OseonLaserSource;

#[async_trait]
impl TrumpfSource for OseonLaserSource {
    async fn poll(&self) -> Result<LaserSnapshot, String> {
        Err("Oseon / TruTops Fab laser backend is not implemented in v1 \
             (needs a licensed Oseon deployment to design against)"
            .to_string())
    }

    fn backend(&self) -> &'static str {
        "oseon"
    }
}

/// Scripted backend for tests and hardware-free bring-up: pops the next
/// scripted outcome per poll, and the last entry sticks for every
/// subsequent poll.
///
/// Never constructed by [`build_adapter`](crate::build_adapter) — it is
/// reachable only from Rust call sites, so it cannot appear in an
/// operator's adapter list.
#[derive(Debug)]
pub struct MockLaserSource {
    scripted: Mutex<Vec<Result<LaserSnapshot, String>>>,
    /// Artificial delay per poll, to drive the slow → `Degraded` path.
    delay: Duration,
}

impl MockLaserSource {
    /// A source that replays `scripted` in order, sticking on the last
    /// entry. An empty script polls as `Unknown` forever.
    pub fn new(scripted: Vec<Result<LaserSnapshot, String>>) -> Self {
        Self {
            scripted: Mutex::new(scripted),
            delay: Duration::ZERO,
        }
    }

    /// Same, but each poll takes `delay` first.
    pub fn with_delay(scripted: Vec<Result<LaserSnapshot, String>>, delay: Duration) -> Self {
        Self {
            scripted: Mutex::new(scripted),
            delay,
        }
    }
}

#[async_trait]
impl TrumpfSource for MockLaserSource {
    async fn poll(&self) -> Result<LaserSnapshot, String> {
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let mut scripted = self.scripted.lock().expect("mock script mutex poisoned");
        match scripted.len() {
            0 => Ok(LaserSnapshot::default()),
            1 => scripted[0].clone(),
            _ => scripted.remove(0),
        }
    }

    fn backend(&self) -> &'static str {
        "mock"
    }
}

/// Trim a backend data-item value into a real identity, or `None`.
///
/// Blank and MTConnect's `UNAVAILABLE` sentinel both mean "no value" —
/// see [`MTCONNECT_UNAVAILABLE`] for why conflating them with an
/// identity mints phantom work orders.
fn normalise_data_item(raw: Option<String>) -> Option<String> {
    let trimmed = raw?.trim().to_string();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(MTCONNECT_UNAVAILABLE) {
        return None;
    }
    Some(trimmed)
}

/// Construction-time configuration for a [`TrumpfAdapter`].
///
/// DoS bounds (`request_timeout`, `max_response_bytes`) and cadence are
/// exposed only so tests can shrink them; production paths use the
/// `DEFAULT_*` constants per [[trust-code-not-operator]]. The backend is
/// deliberately NOT an operator field — v1 compiles in the MTConnect
/// backend, and changing that is a code decision.
#[derive(Debug, Clone)]
pub struct TrumpfAdapterConfig {
    /// Stable identifier; becomes the adapter's [`Adapter::name`] and the
    /// registry key. MUST be unique across registered adapters.
    pub machine_id: String,
    /// Operator-readable display name for the dashboard tile.
    pub friendly_name: String,
    /// Backend host — IP address or DNS name of the MTConnect gateway
    /// fronting the laser.
    pub host: String,
    /// Backend TCP port.
    pub port: u16,
    /// Backend device path. For the MTConnect backend this is the
    /// Agent's device name (`/{device_name}/current`); a future OPC-UA
    /// backend would read it as a node prefix, and Oseon as a machine
    /// key. Reusing one slot keeps the persisted config schema — and its
    /// seller.toml preservation invariant — untouched.
    pub device_name: String,
    pub poll_interval: Duration,
    pub request_timeout: Duration,
    pub slow_threshold: Duration,
    pub max_response_bytes: usize,
    pub channel_capacity: usize,
}

impl TrumpfAdapterConfig {
    /// Construct a config with default DoS bounds + poll cadence; only
    /// the five operator-meaningful fields are exposed.
    pub fn new(
        machine_id: impl Into<String>,
        friendly_name: impl Into<String>,
        host: impl Into<String>,
        port: u16,
        device_name: impl Into<String>,
    ) -> Self {
        Self {
            machine_id: machine_id.into(),
            friendly_name: friendly_name.into(),
            host: host.into(),
            port,
            device_name: device_name.into(),
            poll_interval: DEFAULT_POLL_INTERVAL,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            slow_threshold: DEFAULT_SLOW_THRESHOLD,
            max_response_bytes: DEFAULT_MAX_RESPONSE_BYTES,
            channel_capacity: DEFAULT_CHANNEL_CAPACITY,
        }
    }

    /// The MTConnect `/current` URL this config points the v1 backend at.
    fn current_url(&self) -> String {
        format!(
            "http://{}:{}/{}/current",
            self.host, self.port, self.device_name
        )
    }
}

/// What the adapter has observed so far, for edge detection. `None`
/// fields mean "nothing observed yet".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LaserObservation {
    state: Option<MachineState>,
    job: Option<String>,
}

/// The Trumpf laser-cutter [`Adapter`] implementation.
///
/// Backend-agnostic: it drives whatever [`TrumpfSource`] it was built
/// with. `new()` selects the v1 MTConnect backend;
/// [`with_source`](TrumpfAdapter::with_source) injects any other.
#[derive(Debug)]
pub struct TrumpfAdapter {
    config: TrumpfAdapterConfig,
    /// Cancel token, poll-task handle, and cached health.
    lifecycle: AdapterLifecycle,
    sender: broadcast::Sender<CanonicalEvent>,
    observed: Arc<Mutex<LaserObservation>>,
    /// Injected backend. `None` means `start()` builds the default
    /// MTConnect-backed source from `config` (deferred to `start()`
    /// because building the HTTP client is fallible).
    source: Mutex<Option<Arc<dyn TrumpfSource>>>,
}

impl TrumpfAdapter {
    /// Construct a stopped adapter that will use the **v1 MTConnect
    /// backend**, built on first `start()`.
    pub fn new(config: TrumpfAdapterConfig) -> Self {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        Self {
            config,
            lifecycle: AdapterLifecycle::new(),
            sender,
            observed: Arc::new(Mutex::new(LaserObservation::default())),
            source: Mutex::new(None),
        }
    }

    /// Construct a stopped adapter driving an explicit backend. This is
    /// the seam's whole point: a future `OpcUaLaserSource` reaches
    /// production through this constructor and nothing else changes.
    pub fn with_source(config: TrumpfAdapterConfig, source: Arc<dyn TrumpfSource>) -> Self {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        Self {
            config,
            lifecycle: AdapterLifecycle::new(),
            sender,
            observed: Arc::new(Mutex::new(LaserObservation::default())),
            source: Mutex::new(Some(source)),
        }
    }

    /// Operator-readable friendly name, for the dashboard tile.
    pub fn friendly_name(&self) -> &str {
        &self.config.friendly_name
    }

    /// Resolve the backend, building the default MTConnect one on first
    /// use. Later starts reuse the same source, so a stop/start cycle
    /// does not rebuild an injected backend.
    fn resolve_source(&self) -> Result<Arc<dyn TrumpfSource>, AdapterError> {
        let mut slot = self.source.lock().expect("source mutex poisoned");
        if let Some(existing) = slot.as_ref() {
            return Ok(existing.clone());
        }
        let built = MtconnectLaserSource::new(&self.config)
            .map_err(|e| AdapterError::StartFailed(format!("build laser backend: {e}")))?;
        let source: Arc<dyn TrumpfSource> = Arc::new(built);
        *slot = Some(source.clone());
        Ok(source)
    }
}

#[async_trait]
impl Adapter for TrumpfAdapter {
    fn name(&self) -> &str {
        &self.config.machine_id
    }

    fn kind(&self) -> &'static str {
        "laser-cutter"
    }

    fn endpoint_host(&self) -> Option<String> {
        Some(self.config.host.clone())
    }

    fn endpoint_port(&self) -> Option<u16> {
        Some(self.config.port)
    }

    async fn start(&self) -> Result<(), AdapterError> {
        // Resolve the backend BEFORE claiming the lifecycle, so a failed
        // client build leaves the adapter cleanly Stopped (and
        // restartable) rather than stranded in Starting.
        let source = self.resolve_source()?;

        // Idempotent start guard: None means already running → no-op.
        let Some(cancel) = self.lifecycle.begin_start() else {
            return Ok(());
        };

        let health_slot = self.lifecycle.health_slot();
        let slow_threshold = self.config.slow_threshold;

        // Initial poll synchronously so the first `health()` read after
        // start sees the real outcome, not the transient Starting.
        let outcome = poll_source(source.as_ref()).await;
        apply_poll_outcome(
            outcome,
            &health_slot,
            &self.observed,
            &self.sender,
            &self.config.machine_id,
            slow_threshold,
        );

        let observed = self.observed.clone();
        let sender = self.sender.clone();
        let machine_id = self.config.machine_id.clone();
        let poll_interval = self.config.poll_interval;

        let handle = tokio::spawn(async move {
            run_poll_loop(
                source,
                cancel,
                poll_interval,
                slow_threshold,
                health_slot,
                observed,
                sender,
                machine_id,
            )
            .await;
        });

        self.lifecycle.attach(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), AdapterError> {
        self.lifecycle.stop(&self.config.machine_id).await;
        *self.observed.lock().expect("observed mutex poisoned") = LaserObservation::default();
        Ok(())
    }

    fn health(&self) -> AdapterHealth {
        self.lifecycle.health()
    }

    fn subscribe(&self) -> broadcast::Receiver<CanonicalEvent> {
        self.sender.subscribe()
    }
}

/// Time one backend poll. The seam returns only the snapshot; the
/// adapter owns the clock so every backend gets the same
/// `Healthy` / `Degraded` verdict rule.
async fn poll_source(source: &dyn TrumpfSource) -> Result<(LaserSnapshot, Duration), String> {
    let start = std::time::Instant::now();
    let snapshot = source.poll().await?;
    Ok((snapshot, start.elapsed()))
}

#[allow(clippy::too_many_arguments)]
async fn run_poll_loop(
    source: Arc<dyn TrumpfSource>,
    cancel: CancellationToken,
    poll_interval: Duration,
    slow_threshold: Duration,
    health_slot: Arc<Mutex<AdapterHealth>>,
    observed: Arc<Mutex<LaserObservation>>,
    sender: broadcast::Sender<CanonicalEvent>,
    machine_id: String,
) {
    let mut tick = tokio::time::interval(poll_interval);
    // The first interval tick fires immediately; skip it (the initial
    // poll already ran synchronously in `start()`).
    tick.tick().await;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                tracing::debug!(machine_id = %machine_id, "Trumpf poll loop cancelled");
                return;
            }
            _ = tick.tick() => {
                // Race the in-flight poll against cancel so shutdown
                // drains within one request boundary, not one full
                // poll_interval.
                let outcome = tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::debug!(machine_id = %machine_id, "Trumpf poll cancelled mid-request");
                        return;
                    }
                    o = poll_source(source.as_ref()) => o,
                };
                apply_poll_outcome(
                    outcome,
                    &health_slot,
                    &observed,
                    &sender,
                    &machine_id,
                    slow_threshold,
                );
            }
        }
    }
}

fn apply_poll_outcome(
    outcome: Result<(LaserSnapshot, Duration), String>,
    health_slot: &Arc<Mutex<AdapterHealth>>,
    observed_slot: &Arc<Mutex<LaserObservation>>,
    sender: &broadcast::Sender<CanonicalEvent>,
    machine_id: &str,
    slow_threshold: Duration,
) {
    match &outcome {
        Ok((_, elapsed)) => {
            let new_health = if *elapsed > slow_threshold {
                AdapterHealth::Degraded {
                    reason: format!("slow response {}ms", elapsed.as_millis()),
                }
            } else {
                AdapterHealth::Healthy
            };
            *health_slot.lock().expect("health mutex poisoned") = new_health;
        }
        Err(reason) => {
            *health_slot.lock().expect("health mutex poisoned") = AdapterHealth::Unhealthy {
                reason: reason.clone(),
            };
        }
    }

    let Ok((snapshot, _)) = outcome else {
        return;
    };

    let at_iso8601 = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());

    let mut observed = observed_slot.lock().expect("observed mutex poisoned");
    let events = derive_events(&observed, &snapshot, machine_id, &at_iso8601);

    if !events.is_empty() {
        // The seam's richer fields have no canonical event variant yet
        // (adding one is deferred until a backend that populates them
        // exists). Surface them here so a nest is traceable in the logs
        // even though only the job id reaches the ledger.
        tracing::info!(
            machine_id = %machine_id,
            job = ?snapshot.active_job_id,
            program = ?snapshot.program_name,
            material = ?snapshot.material_ref,
            piece_count = ?snapshot.piece_count,
            state = ?snapshot.machine_state,
            "Trumpf laser edge observed"
        );
    }

    for event in events {
        // Ignore `SendError` — broadcast::send returns Err only when no
        // receivers exist, which is a legitimate state (no ledger writer
        // attached yet).
        let _ = sender.send(event);
    }

    observed.state = Some(snapshot.machine_state);
    observed.job = snapshot.active_job_id;
}

/// PURE edge-detection: given what was observed before and what the
/// backend just reported, produce the canonical events for this tick.
///
/// Extracted from the poll loop so the whole emission contract is
/// testable without a socket, a clock, or a runtime. Ordering is
/// deterministic — machine-state edge first, then job edges, and on a
/// job **swap** the completion of the outgoing job precedes the start of
/// the incoming one.
fn derive_events(
    observed: &LaserObservation,
    next: &LaserSnapshot,
    machine_id: &str,
    at_iso8601: &str,
) -> Vec<CanonicalEvent> {
    let mut events = Vec::new();

    // Machine-state edge. First observation baselines from `Unknown`,
    // matching the MTConnect adapter's contract.
    let previous_state = observed.state.unwrap_or(MachineState::Unknown);
    if previous_state != next.machine_state {
        events.push(CanonicalEvent::MachineStateChanged {
            machine_id: machine_id.to_string(),
            previous_state,
            new_state: next.machine_state,
            at_iso8601: at_iso8601.to_string(),
        });
    }

    // Job edges. The vendor job string is recorded verbatim — see the
    // module docs on why it is not resolved to an ABERP work-order id.
    let job_event = |work_order_id: &str, previous: WorkOrderState, new: WorkOrderState| {
        CanonicalEvent::WorkOrderStateChanged {
            work_order_id: work_order_id.to_string(),
            previous_state: previous,
            new_state: new,
            at_iso8601: at_iso8601.to_string(),
        }
    };

    match (observed.job.as_deref(), next.active_job_id.as_deref()) {
        // No job before, no job now — nothing happened.
        (None, None) => {}
        // Same job still running — edge-triggered means no duplicate.
        (Some(before), Some(now)) if before == now => {}
        // A job finished (or the machine dropped it).
        (Some(before), None) => {
            events.push(job_event(
                before,
                WorkOrderState::InProgress,
                WorkOrderState::Completed,
            ));
        }
        // A job started — including the first job seen after boot.
        (None, Some(now)) => {
            events.push(job_event(
                now,
                WorkOrderState::Released,
                WorkOrderState::InProgress,
            ));
        }
        // A swap between two polls: the outgoing job completed and the
        // incoming one started. TWO events, ordered — collapsing them
        // into one would silently lose the completion.
        (Some(before), Some(now)) => {
            events.push(job_event(
                before,
                WorkOrderState::InProgress,
                WorkOrderState::Completed,
            ));
            events.push(job_event(
                now,
                WorkOrderState::Released,
                WorkOrderState::InProgress,
            ));
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;

    // ====== Fixtures ======

    fn snap(state: MachineState, job: Option<&str>) -> LaserSnapshot {
        LaserSnapshot {
            machine_state: state,
            active_job_id: job.map(|s| s.to_string()),
            program_name: job.map(|s| s.to_string()),
            ..LaserSnapshot::default()
        }
    }

    fn observation(state: Option<MachineState>, job: Option<&str>) -> LaserObservation {
        LaserObservation {
            state,
            job: job.map(|s| s.to_string()),
        }
    }

    const AT: &str = "2026-08-05T09:00:00Z";

    fn derive(observed: &LaserObservation, next: &LaserSnapshot) -> Vec<CanonicalEvent> {
        derive_events(observed, next, "laser-1", AT)
    }

    /// Destructure a `WorkOrderStateChanged` into a comparable tuple, or
    /// panic naming what it actually was.
    fn wo(event: &CanonicalEvent) -> (&str, WorkOrderState, WorkOrderState) {
        match event {
            CanonicalEvent::WorkOrderStateChanged {
                work_order_id,
                previous_state,
                new_state,
                ..
            } => (work_order_id.as_str(), *previous_state, *new_state),
            other => panic!("expected WorkOrderStateChanged, got {}", other.type_tag()),
        }
    }

    fn ms(event: &CanonicalEvent) -> (&str, MachineState, MachineState) {
        match event {
            CanonicalEvent::MachineStateChanged {
                machine_id,
                previous_state,
                new_state,
                ..
            } => (machine_id.as_str(), *previous_state, *new_state),
            other => panic!("expected MachineStateChanged, got {}", other.type_tag()),
        }
    }

    // ====== Layer 1 — pure edge detection (no I/O, no clock) ======

    /// Steady state emits NOTHING. This is the whole point of
    /// edge-triggering: a laser idle for an hour must not write 720
    /// audit rows.
    #[test]
    fn unchanged_snapshot_emits_no_events() {
        let observed = observation(Some(MachineState::Running), Some("NEST_A.LST"));
        let next = snap(MachineState::Running, Some("NEST_A.LST"));
        assert!(derive(&observed, &next).is_empty());
    }

    /// First observation baselines the machine state from `Unknown` —
    /// the same contract the MTConnect adapter gives its consumers.
    #[test]
    fn first_observation_baselines_from_unknown() {
        let events = derive(&LaserObservation::default(), &snap(MachineState::Idle, None));
        assert_eq!(events.len(), 1);
        assert_eq!(
            ms(&events[0]),
            ("laser-1", MachineState::Unknown, MachineState::Idle)
        );
    }

    /// A first observation that is genuinely `Unknown` with no job is
    /// NOT an edge — `Unknown → Unknown` must stay silent, or every
    /// adapter boot against an unreachable-but-parseable agent would
    /// mint a spurious row.
    #[test]
    fn first_observation_of_unknown_with_no_job_is_silent() {
        let events = derive(&LaserObservation::default(), &LaserSnapshot::default());
        assert!(events.is_empty(), "got {events:?}");
    }

    #[test]
    fn state_change_emits_exactly_one_machine_state_event() {
        let observed = observation(Some(MachineState::Idle), None);
        let events = derive(&observed, &snap(MachineState::Running, None));
        assert_eq!(events.len(), 1);
        assert_eq!(
            ms(&events[0]),
            ("laser-1", MachineState::Idle, MachineState::Running)
        );
    }

    #[test]
    fn job_appearing_emits_released_to_in_progress() {
        let observed = observation(Some(MachineState::Idle), None);
        let events = derive(&observed, &snap(MachineState::Idle, Some("NEST_A.LST")));
        assert_eq!(events.len(), 1);
        assert_eq!(
            wo(&events[0]),
            (
                "NEST_A.LST",
                WorkOrderState::Released,
                WorkOrderState::InProgress
            )
        );
    }

    #[test]
    fn job_disappearing_emits_in_progress_to_completed() {
        let observed = observation(Some(MachineState::Idle), Some("NEST_A.LST"));
        let events = derive(&observed, &snap(MachineState::Idle, None));
        assert_eq!(events.len(), 1);
        assert_eq!(
            wo(&events[0]),
            (
                "NEST_A.LST",
                WorkOrderState::InProgress,
                WorkOrderState::Completed
            )
        );
    }

    /// A job swap between two polls is TWO events in a fixed order —
    /// completion of the outgoing nest, then the start of the incoming
    /// one. Collapsing them into a single "start" would silently lose
    /// the completion of a nest that really did finish.
    #[test]
    fn job_swap_emits_completion_then_start_in_order() {
        let observed = observation(Some(MachineState::Running), Some("NEST_A.LST"));
        let events = derive(&observed, &snap(MachineState::Running, Some("NEST_B.LST")));
        assert_eq!(events.len(), 2, "swap must not collapse: {events:?}");
        assert_eq!(
            wo(&events[0]),
            (
                "NEST_A.LST",
                WorkOrderState::InProgress,
                WorkOrderState::Completed
            )
        );
        assert_eq!(
            wo(&events[1]),
            (
                "NEST_B.LST",
                WorkOrderState::Released,
                WorkOrderState::InProgress
            )
        );
    }

    /// State and job changing on the SAME poll produce two independent
    /// events, machine-state first — not one merged event.
    #[test]
    fn simultaneous_state_and_job_change_emit_two_ordered_events() {
        let observed = observation(Some(MachineState::Idle), None);
        let events = derive(&observed, &snap(MachineState::Running, Some("NEST_A.LST")));
        assert_eq!(events.len(), 2);
        assert_eq!(
            ms(&events[0]),
            ("laser-1", MachineState::Idle, MachineState::Running)
        );
        assert_eq!(
            wo(&events[1]),
            (
                "NEST_A.LST",
                WorkOrderState::Released,
                WorkOrderState::InProgress
            )
        );
    }

    /// Every emitted event carries the timestamp it was derived with —
    /// no event silently stamps itself from a different clock read.
    #[test]
    fn every_derived_event_carries_the_supplied_timestamp() {
        let observed = observation(Some(MachineState::Idle), Some("A"));
        let events = derive(&observed, &snap(MachineState::Running, Some("B")));
        assert_eq!(events.len(), 3);
        for event in &events {
            let at = match event {
                CanonicalEvent::MachineStateChanged { at_iso8601, .. }
                | CanonicalEvent::WorkOrderStateChanged { at_iso8601, .. } => at_iso8601.as_str(),
                other => panic!("unexpected variant {}", other.type_tag()),
            };
            assert_eq!(at, AT);
        }
    }

    /// The adapter emits ONLY the two variants documented in the module
    /// docs. A future contributor adding a third would have to change
    /// this pin deliberately.
    #[test]
    fn adapter_emits_only_the_two_documented_variants() {
        let observed = observation(Some(MachineState::Idle), Some("A"));
        let events = derive(&observed, &snap(MachineState::Fault, Some("B")));
        for event in &events {
            assert!(
                matches!(
                    event,
                    CanonicalEvent::MachineStateChanged { .. }
                        | CanonicalEvent::WorkOrderStateChanged { .. }
                ),
                "unexpected variant {}",
                event.type_tag()
            );
        }
    }

    // ====== Layer 2 — the UNAVAILABLE trap ======

    /// MTConnect publishes `UNAVAILABLE` for a data item with no current
    /// value. Treating that as a job identity would mint a phantom work
    /// order literally named "UNAVAILABLE" — and then a phantom
    /// completion the moment a real program loads.
    #[test]
    fn unavailable_and_blanks_are_not_identities() {
        assert_eq!(normalise_data_item(Some("UNAVAILABLE".into())), None);
        // Agents are inconsistent about case.
        assert_eq!(normalise_data_item(Some("unavailable".into())), None);
        assert_eq!(normalise_data_item(Some("Unavailable".into())), None);
        assert_eq!(normalise_data_item(Some(String::new())), None);
        assert_eq!(normalise_data_item(Some("   ".into())), None);
        assert_eq!(normalise_data_item(None), None);
        // A real value survives, trimmed.
        assert_eq!(
            normalise_data_item(Some("  NEST_A.LST  ".into())),
            Some("NEST_A.LST".to_string())
        );
        // A program that merely CONTAINS the sentinel is still a real
        // program name.
        assert_eq!(
            normalise_data_item(Some("UNAVAILABLE_PARTS.LST".into())),
            Some("UNAVAILABLE_PARTS.LST".to_string())
        );
    }

    // ====== Layer 3 — the adapter over a scripted backend ======

    fn cfg_for_test(machine_id: &str, port: u16) -> TrumpfAdapterConfig {
        TrumpfAdapterConfig {
            machine_id: machine_id.to_string(),
            friendly_name: format!("Test {machine_id}"),
            host: "127.0.0.1".to_string(),
            port,
            device_name: "TruLaser".to_string(),
            // Tight bounds for tests.
            poll_interval: Duration::from_millis(150),
            request_timeout: Duration::from_millis(500),
            slow_threshold: Duration::from_millis(250),
            max_response_bytes: 64 * 1024,
            channel_capacity: 16,
        }
    }

    async fn next_event(
        rx: &mut broadcast::Receiver<CanonicalEvent>,
        within: Duration,
    ) -> Option<CanonicalEvent> {
        tokio::time::timeout(within, rx.recv())
            .await
            .ok()
            .and_then(|r| r.ok())
    }

    fn mock_adapter(
        machine_id: &str,
        scripted: Vec<Result<LaserSnapshot, String>>,
    ) -> Arc<TrumpfAdapter> {
        Arc::new(TrumpfAdapter::with_source(
            cfg_for_test(machine_id, 1),
            Arc::new(MockLaserSource::new(scripted)),
        ))
    }

    #[tokio::test]
    async fn config_defaults_match_documented_constants() {
        let cfg = TrumpfAdapterConfig::new("laser-1", "TruLaser 5030", "10.0.2.40", 5000, "L1");
        assert_eq!(cfg.poll_interval, DEFAULT_POLL_INTERVAL);
        assert_eq!(cfg.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(cfg.slow_threshold, DEFAULT_SLOW_THRESHOLD);
        assert_eq!(cfg.max_response_bytes, DEFAULT_MAX_RESPONSE_BYTES);
        assert_eq!(cfg.channel_capacity, DEFAULT_CHANNEL_CAPACITY);
        // The DoS bound MUST stay below the cadence or stalled requests
        // pile up across ticks.
        assert!(
            DEFAULT_REQUEST_TIMEOUT < DEFAULT_POLL_INTERVAL,
            "request timeout must be below the poll interval"
        );
        assert!(DEFAULT_SLOW_THRESHOLD < DEFAULT_REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn trait_metadata_matches_config() {
        let adapter = TrumpfAdapter::new(cfg_for_test("laser-line-a", 5001));
        assert_eq!(adapter.name(), "laser-line-a");
        assert_eq!(adapter.kind(), "laser-cutter");
        assert_eq!(adapter.endpoint_host().as_deref(), Some("127.0.0.1"));
        assert_eq!(adapter.endpoint_port(), Some(5001));
        assert_eq!(adapter.friendly_name(), "Test laser-line-a");
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    /// The adapter is usable as `Arc<dyn Adapter>` — the shape the
    /// registry, ledger writer, and shutdown coordinator all consume.
    #[tokio::test]
    async fn adapter_is_dyn_safe() {
        let adapter: Arc<dyn Adapter> = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-dyn", 1)));
        assert_eq!(adapter.kind(), "laser-cutter");
    }

    #[tokio::test]
    async fn first_poll_emits_baseline_and_job_start() {
        let adapter = mock_adapter(
            "laser-baseline",
            vec![Ok(snap(MachineState::Running, Some("NEST_A.LST")))],
        );
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        let first = next_event(&mut rx, Duration::from_secs(2))
            .await
            .expect("baseline machine-state event");
        assert_eq!(
            ms(&first),
            ("laser-baseline", MachineState::Unknown, MachineState::Running)
        );
        let second = next_event(&mut rx, Duration::from_secs(2))
            .await
            .expect("job start event");
        assert_eq!(
            wo(&second),
            (
                "NEST_A.LST",
                WorkOrderState::Released,
                WorkOrderState::InProgress
            )
        );
        assert_eq!(adapter.health(), AdapterHealth::Healthy);
        adapter.stop().await.unwrap();
    }

    /// A steady laser across many polls emits its baseline and then
    /// NOTHING. Guards the edge-trigger against a regression that would
    /// flood the audit ledger.
    #[tokio::test]
    async fn steady_state_emits_no_duplicates_across_polls() {
        let adapter = mock_adapter(
            "laser-steady",
            vec![Ok(snap(MachineState::Running, Some("NEST_A.LST")))],
        );
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        // Drain the two baseline events.
        next_event(&mut rx, Duration::from_secs(2)).await.unwrap();
        next_event(&mut rx, Duration::from_secs(2)).await.unwrap();

        // Several more polls elapse (150ms cadence) with identical data.
        tokio::time::sleep(Duration::from_millis(700)).await;
        let extra = next_event(&mut rx, Duration::from_millis(200)).await;
        assert!(extra.is_none(), "unchanged state re-emitted: {extra:?}");
        adapter.stop().await.unwrap();
    }

    /// A transition observed across two polls emits exactly one event.
    #[tokio::test]
    async fn transition_across_polls_emits_one_event() {
        let adapter = mock_adapter(
            "laser-transition",
            vec![
                Ok(snap(MachineState::Idle, None)),
                Ok(snap(MachineState::Running, None)),
            ],
        );
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        let baseline = next_event(&mut rx, Duration::from_secs(2)).await.unwrap();
        assert_eq!(
            ms(&baseline),
            (
                "laser-transition",
                MachineState::Unknown,
                MachineState::Idle
            )
        );
        let moved = next_event(&mut rx, Duration::from_secs(2))
            .await
            .expect("transition event");
        assert_eq!(
            ms(&moved),
            (
                "laser-transition",
                MachineState::Idle,
                MachineState::Running
            )
        );
        // And the now-steady Running state stops emitting.
        let extra = next_event(&mut rx, Duration::from_millis(400)).await;
        assert!(extra.is_none(), "re-emitted after settling: {extra:?}");
        adapter.stop().await.unwrap();
    }

    /// A backend failure surfaces the reason VERBATIM on the health
    /// snapshot — that string is what the operator reads on the
    /// Adapters page.
    #[tokio::test]
    async fn backend_error_reports_unhealthy_with_reason_preserved() {
        let adapter = mock_adapter(
            "laser-err",
            vec![Err("gateway refused the connection".to_string())],
        );
        adapter.start().await.unwrap();
        assert_eq!(
            adapter.health(),
            AdapterHealth::Unhealthy {
                reason: "gateway refused the connection".to_string()
            }
        );
        adapter.stop().await.unwrap();
    }

    /// A failing poll must not emit events, and must not corrupt the
    /// observed baseline — the next good poll still reports the real
    /// transition from what was last actually seen.
    #[tokio::test]
    async fn error_poll_emits_nothing_and_preserves_baseline() {
        let adapter = mock_adapter(
            "laser-recover",
            vec![
                Ok(snap(MachineState::Idle, None)),
                Err("transient gateway blip".to_string()),
                Ok(snap(MachineState::Running, None)),
            ],
        );
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        let baseline = next_event(&mut rx, Duration::from_secs(2)).await.unwrap();
        assert_eq!(
            ms(&baseline).2,
            MachineState::Idle,
            "baseline should be the first good poll"
        );

        // The next event must be Idle → Running: the error poll in
        // between contributed no event and did not reset the baseline
        // to Unknown (which would have emitted Unknown → Running).
        let recovered = next_event(&mut rx, Duration::from_secs(2))
            .await
            .expect("post-error transition");
        assert_eq!(
            ms(&recovered),
            (
                "laser-recover",
                MachineState::Idle,
                MachineState::Running
            )
        );
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn slow_backend_reports_degraded() {
        let adapter = Arc::new(TrumpfAdapter::with_source(
            cfg_for_test("laser-slow", 1),
            Arc::new(MockLaserSource::with_delay(
                vec![Ok(snap(MachineState::Running, None))],
                Duration::from_millis(400),
            )),
        ));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Degraded { reason } => {
                assert!(reason.contains("slow response"), "reason was {reason:?}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn start_and_stop_are_idempotent() {
        let adapter = mock_adapter("laser-idem", vec![Ok(snap(MachineState::Idle, None))]);
        adapter.start().await.unwrap();
        let after_first = adapter.health();
        // Second start is a no-op, not a second poll loop.
        adapter.start().await.unwrap();
        assert_eq!(adapter.health(), after_first);

        adapter.stop().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
        adapter.stop().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    /// After a stop the observation is cleared, so a restart re-emits
    /// the `Unknown → X` baseline rather than silently assuming the
    /// laser held its state while the adapter was down.
    #[tokio::test]
    async fn restart_re_emits_the_baseline() {
        let adapter = mock_adapter("laser-restart", vec![Ok(snap(MachineState::Running, None))]);
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();
        next_event(&mut rx, Duration::from_secs(2)).await.unwrap();
        adapter.stop().await.unwrap();

        adapter.start().await.unwrap();
        let again = next_event(&mut rx, Duration::from_secs(2))
            .await
            .expect("baseline after restart");
        assert_eq!(
            ms(&again),
            (
                "laser-restart",
                MachineState::Unknown,
                MachineState::Running
            )
        );
        adapter.stop().await.unwrap();
    }

    // ====== v2 backends — declared, inert, non-panicking ======

    /// The unimplemented backends are visible (loud reason strings) but
    /// must NOT panic: a `todo!()` inside the spawned poll task would
    /// abort it inside a Defense binary.
    #[tokio::test]
    async fn v2_backends_return_errors_rather_than_panicking() {
        let opcua = OpcUaLaserSource;
        assert_eq!(opcua.backend(), "opc-ua");
        let err = opcua.poll().await.expect_err("must not be implemented");
        assert!(err.contains("not implemented"), "reason was {err:?}");

        let oseon = OseonLaserSource;
        assert_eq!(oseon.backend(), "oseon");
        let err = oseon.poll().await.expect_err("must not be implemented");
        assert!(err.contains("not implemented"), "reason was {err:?}");
    }

    /// Driving the adapter with an unimplemented backend degrades to a
    /// visible `Unhealthy`, not a dead task.
    #[tokio::test]
    async fn adapter_over_a_v2_backend_is_unhealthy_not_dead() {
        let adapter = Arc::new(TrumpfAdapter::with_source(
            cfg_for_test("laser-v2", 1),
            Arc::new(OpcUaLaserSource),
        ));
        adapter.start().await.unwrap();
        assert!(matches!(adapter.health(), AdapterHealth::Unhealthy { .. }));
        // The loop is still alive and still stoppable.
        adapter.stop().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    // ====== Layer 4 — the REAL MTConnect backend over a mock agent ======

    async fn pick_free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    enum MockBehaviour {
        Ok(String),
        /// Each accepted connection takes the next body; the last entry
        /// sticks. Lets one test observe a transition across two polls.
        OkSequence(Arc<AsyncMutex<Vec<String>>>),
        Status404,
    }

    async fn spawn_mock_agent(port: u16, behaviour: MockBehaviour) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                let mut acc = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    let n = match sock.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => return,
                    };
                    acc.extend_from_slice(&buf[..n]);
                    if acc.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                match &behaviour {
                    MockBehaviour::Ok(body) => write_ok(&mut sock, body).await,
                    MockBehaviour::OkSequence(seq) => {
                        let body = {
                            let mut g = seq.lock().await;
                            if g.len() > 1 {
                                g.remove(0)
                            } else {
                                g[0].clone()
                            }
                        };
                        write_ok(&mut sock, &body).await;
                    }
                    MockBehaviour::Status404 => write_404(&mut sock).await,
                }
            }
        })
    }

    async fn write_ok(sock: &mut tokio::net::TcpStream, body: &str) {
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.shutdown().await;
    }

    async fn write_404(sock: &mut tokio::net::TcpStream) {
        let body = "<error>no such device</error>";
        let resp = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.shutdown().await;
    }

    /// A laser-flavoured MTConnect Streams document, as a gateway in
    /// front of a TruLaser would publish it.
    fn laser_streams_xml(execution: &str, program: &str, part_count: u64) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MTConnectStreams xmlns="urn:mtconnect.org:MTConnectStreams:1.7">
  <Header creationTime="2026-08-05T09:00:00Z" sender="gateway" instanceId="1" version="1.7.0" />
  <Streams>
    <DeviceStream name="TruLaser" uuid="trumpf-1">
      <ComponentStream component="Path" name="path" componentId="p">
        <Events>
          <Execution dataItemId="exec">{execution}</Execution>
          <Program dataItemId="prog">{program}</Program>
          <ControllerMode dataItemId="cmode">AUTOMATIC</ControllerMode>
        </Events>
        <Samples>
          <PartCount dataItemId="pc">{part_count}</PartCount>
        </Samples>
      </ComponentStream>
      <ComponentStream component="Device" name="device" componentId="d">
        <Events>
          <Availability dataItemId="avail">AVAILABLE</Availability>
        </Events>
      </ComponentStream>
    </DeviceStream>
  </Streams>
</MTConnectStreams>"#
        )
    }

    /// End-to-end over real HTTP: mock gateway → `poll_once` → parser →
    /// seam → edge detection → broadcast. Nothing mocked below the wire.
    #[tokio::test]
    async fn e2e_real_mtconnect_backend_emits_state_and_job_events() {
        let port = pick_free_port().await;
        let agent = spawn_mock_agent(
            port,
            MockBehaviour::Ok(laser_streams_xml("ACTIVE", "NEST_7742.LST", 12)),
        )
        .await;

        let adapter = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-e2e", port)));
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        let first = next_event(&mut rx, Duration::from_secs(3))
            .await
            .expect("machine-state baseline");
        assert_eq!(
            ms(&first),
            ("laser-e2e", MachineState::Unknown, MachineState::Running),
            "ACTIVE must map to Running through the SHIPPED Execution table"
        );
        let second = next_event(&mut rx, Duration::from_secs(3))
            .await
            .expect("job start");
        assert_eq!(
            wo(&second),
            (
                "NEST_7742.LST",
                WorkOrderState::Released,
                WorkOrderState::InProgress
            ),
            "the nest program name is the vendor job identity"
        );
        assert_eq!(adapter.health(), AdapterHealth::Healthy);

        adapter.stop().await.unwrap();
        agent.abort();
    }

    /// ADVERSARIAL: a gateway with no program loaded publishes
    /// `Program: UNAVAILABLE`. The adapter must report the machine state
    /// and emit NO work-order event — a phantom job named "UNAVAILABLE"
    /// would land in the audit ledger and then "complete" the moment a
    /// real nest starts.
    #[tokio::test]
    async fn e2e_unavailable_program_emits_no_phantom_work_order() {
        let port = pick_free_port().await;
        let agent = spawn_mock_agent(
            port,
            MockBehaviour::Ok(laser_streams_xml("READY", "UNAVAILABLE", 0)),
        )
        .await;

        let adapter = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-unavail", port)));
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        let first = next_event(&mut rx, Duration::from_secs(3))
            .await
            .expect("machine-state baseline");
        assert_eq!(
            ms(&first),
            ("laser-unavail", MachineState::Unknown, MachineState::Idle)
        );

        // Several polls elapse. NOTHING else may be emitted.
        tokio::time::sleep(Duration::from_millis(500)).await;
        let extra = next_event(&mut rx, Duration::from_millis(300)).await;
        assert!(
            extra.is_none(),
            "UNAVAILABLE minted a phantom work order: {extra:?}"
        );

        adapter.stop().await.unwrap();
        agent.abort();
    }

    /// ADVERSARIAL: a real nest starting AFTER an `UNAVAILABLE` period
    /// must emit a clean single start — not a "completion" of the
    /// phantom followed by a start.
    #[tokio::test]
    async fn e2e_job_starting_after_unavailable_emits_one_clean_start() {
        let port = pick_free_port().await;
        let seq = Arc::new(AsyncMutex::new(vec![
            laser_streams_xml("READY", "UNAVAILABLE", 0),
            laser_streams_xml("ACTIVE", "NEST_9001.LST", 0),
        ]));
        let agent = spawn_mock_agent(port, MockBehaviour::OkSequence(seq)).await;

        let adapter = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-seq", port)));
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        // Poll 1: Unknown → Idle, no job event.
        let first = next_event(&mut rx, Duration::from_secs(3)).await.unwrap();
        assert_eq!(ms(&first).2, MachineState::Idle);

        // Poll 2: Idle → Running, then the job start.
        let second = next_event(&mut rx, Duration::from_secs(3))
            .await
            .expect("state transition");
        assert_eq!(
            ms(&second),
            ("laser-seq", MachineState::Idle, MachineState::Running)
        );
        let third = next_event(&mut rx, Duration::from_secs(3))
            .await
            .expect("job start");
        assert_eq!(
            wo(&third),
            (
                "NEST_9001.LST",
                WorkOrderState::Released,
                WorkOrderState::InProgress
            )
        );

        adapter.stop().await.unwrap();
        agent.abort();
    }

    #[tokio::test]
    async fn e2e_404_from_gateway_reports_unhealthy_with_status() {
        let port = pick_free_port().await;
        let agent = spawn_mock_agent(port, MockBehaviour::Status404).await;

        let adapter = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-404", port)));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Unhealthy { reason } => {
                assert!(reason.contains("404"), "reason was {reason:?}");
            }
            other => panic!("expected Unhealthy, got {other:?}"),
        }
        adapter.stop().await.unwrap();
        agent.abort();
    }

    #[tokio::test]
    async fn e2e_malformed_xml_reports_unhealthy_parse_error() {
        let port = pick_free_port().await;
        let agent = spawn_mock_agent(
            port,
            MockBehaviour::Ok("<NotAnMTConnectDoc><oops/></NotAnMTConnectDoc>".to_string()),
        )
        .await;

        let adapter = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-badxml", port)));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Unhealthy { reason } => {
                assert!(reason.contains("parse error"), "reason was {reason:?}");
            }
            other => panic!("expected Unhealthy, got {other:?}"),
        }
        adapter.stop().await.unwrap();
        agent.abort();
    }

    /// A closed port is the "gateway is down" case an operator will
    /// actually hit.
    #[tokio::test]
    async fn e2e_closed_port_reports_unhealthy_on_connect() {
        let port = pick_free_port().await; // nothing listening
        let adapter = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-closed", port)));
        adapter.start().await.unwrap();
        assert!(matches!(adapter.health(), AdapterHealth::Unhealthy { .. }));
        adapter.stop().await.unwrap();
    }

    /// The backend is built through the seam and reports its label — the
    /// v1 wiring really is MTConnect, not something else.
    #[tokio::test]
    async fn default_backend_is_mtconnect() {
        let cfg = cfg_for_test("laser-backend", 5000);
        let source = MtconnectLaserSource::new(&cfg).expect("build backend");
        assert_eq!(source.backend(), "mtconnect");
    }

    // ====== Layer 5 — the WHOLE path, gateway to audit ledger ======

    /// The full production wiring, end to end and nothing stubbed below
    /// the wire: a mock MTConnect gateway serves laser Streams XML, the
    /// real `TrumpfAdapter` polls it through the real
    /// `MtconnectLaserSource`, the real `spawn_ledger_writer` drains the
    /// broadcast, and the events land as `mes.adapter_event` rows in a
    /// real DuckDB audit ledger on disk — where the payload round-trips
    /// back to the exact canonical events.
    ///
    /// This is the pin that would catch a regression anywhere in the
    /// seam that unit tests around either end would miss.
    #[tokio::test]
    async fn e2e_laser_events_reach_the_audit_ledger_on_disk() {
        use crate::audit::MesAdapterEventPayload;
        use crate::ledger_writer::{spawn_ledger_writer, LedgerWriterActor, LedgerWriterDeps};
        use aberp_audit_ledger::{ensure_schema, BinaryHash, TenantId};
        use ulid::Ulid;

        let tempdir = std::env::temp_dir().join(format!("aberp-trumpf-e2e-{}", Ulid::new()));
        std::fs::create_dir_all(&tempdir).unwrap();
        let db_path = tempdir.join("audit.duckdb");
        {
            let conn = duckdb::Connection::open(&db_path).unwrap();
            ensure_schema(&conn).unwrap();
        }

        // A gateway that reports an idle laser, then a nest running.
        let port = pick_free_port().await;
        let seq = Arc::new(AsyncMutex::new(vec![
            laser_streams_xml("READY", "UNAVAILABLE", 0),
            laser_streams_xml("ACTIVE", "NEST_4711.LST", 3),
        ]));
        let agent = spawn_mock_agent(port, MockBehaviour::OkSequence(seq)).await;

        let adapter = Arc::new(TrumpfAdapter::new(cfg_for_test("laser-ledger", port)));
        let adapter_for_writer: Arc<dyn Adapter> = adapter.clone();
        let deps = LedgerWriterDeps {
            db_path: db_path.clone(),
            tenant: TenantId::new("ten_test_trumpf_e2e").expect("tenant id"),
            binary_hash: BinaryHash::from_bytes([0u8; 32]),
            actor: LedgerWriterActor {
                session_id: Ulid::new().to_string(),
                operator_login: "test-operator".to_string(),
            },
        };
        let cancel = CancellationToken::new();
        let writer = spawn_ledger_writer(adapter_for_writer, deps, cancel.clone());

        // Subscribe the writer BEFORE the adapter's first synchronous
        // poll, or the baseline event races the subscription.
        tokio::time::sleep(Duration::from_millis(50)).await;
        adapter.start().await.unwrap();

        // Expect exactly three rows: Unknown→Idle, Idle→Running, and
        // the nest start. Poll the DB with a bounded timeout.
        let started = std::time::Instant::now();
        let mut payloads: Vec<MesAdapterEventPayload> = Vec::new();
        while started.elapsed() < Duration::from_secs(10) {
            let conn = duckdb::Connection::open(&db_path).unwrap();
            let mut stmt = conn
                .prepare(
                    "SELECT payload FROM audit_ledger \
                     WHERE kind = 'mes.adapter_event' ORDER BY seq",
                )
                .unwrap();
            payloads = stmt
                .query_map([], |r| r.get::<_, Vec<u8>>(0))
                .unwrap()
                .map(|blob| {
                    serde_json::from_slice::<MesAdapterEventPayload>(&blob.unwrap())
                        .expect("ledger payload must round-trip")
                })
                .collect();
            drop(stmt);
            drop(conn);
            if payloads.len() >= 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        assert_eq!(
            payloads.len(),
            3,
            "expected 3 audit rows, got {:?}",
            payloads.iter().map(|p| p.event.type_tag()).collect::<Vec<_>>()
        );
        for payload in &payloads {
            assert_eq!(payload.adapter_name, "laser-ledger");
        }
        assert_eq!(
            ms(&payloads[0].event),
            ("laser-ledger", MachineState::Unknown, MachineState::Idle)
        );
        assert_eq!(
            ms(&payloads[1].event),
            ("laser-ledger", MachineState::Idle, MachineState::Running)
        );
        assert_eq!(
            wo(&payloads[2].event),
            (
                "NEST_4711.LST",
                WorkOrderState::Released,
                WorkOrderState::InProgress
            ),
            "the nest must reach the ledger as a work-order start"
        );

        // No phantom "UNAVAILABLE" job anywhere in the ledger.
        assert!(
            !payloads.iter().any(|p| matches!(
                &p.event,
                CanonicalEvent::WorkOrderStateChanged { work_order_id, .. }
                    if work_order_id.eq_ignore_ascii_case("UNAVAILABLE")
            )),
            "a phantom UNAVAILABLE work order reached the audit ledger"
        );

        cancel.cancel();
        let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
        adapter.stop().await.unwrap();
        agent.abort();
        std::fs::remove_dir_all(&tempdir).ok();
    }

    /// The v1 backend polls the same `/{device}/current` URL shape the
    /// shipped MTConnect adapter does — the device slot really is the
    /// gateway's device path.
    #[test]
    fn current_url_assembles_per_device() {
        let cfg = TrumpfAdapterConfig::new("l", "L", "10.0.2.40", 5000, "TruLaser");
        assert_eq!(cfg.current_url(), "http://10.0.2.40:5000/TruLaser/current");
    }
}
