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
