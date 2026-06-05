//! [`MtconnectAdapter`] — MTConnect HTTP-poll adapter for CNC machines
//! (S247 / PR-240 / ADR-0060 Phase δ — second hardware-input adapter).
//!
//! ## Why an MTConnect adapter
//!
//! MTConnect (mtconnect.org, ANSI/MTC1.4-2018) is the open standard for
//! shop-floor machine telemetry. Modern CNC controllers — DMG MORI,
//! Mazak, Haas, Okuma, Fanuc, Heidenhain — either ship with a built-in
//! MTConnect Agent or have a sidecar Agent that scrapes the controller's
//! native protocol and re-publishes it as MTConnect Streams XML over
//! HTTP. Picking MTConnect over a vendor SDK is the same
//! [[spacex-vertical-integration]] call as Zebra/ZPL (PR-238): one open
//! protocol covers the entire population instead of N proprietary SDKs.
//!
//! ## Wire shape
//!
//! - HTTP GET `http://{host}:{port}/{device_name}/current` returns an
//!   `<MTConnectStreams>` XML document — a snapshot of every data item
//!   the Agent knows about for the named device. Polled every
//!   `poll_interval` (default 5s).
//! - The Streams document is structured as `<DeviceStream>` →
//!   `<ComponentStream>` → `<Events>` / `<Samples>` / `<Condition>`. This
//!   adapter pattern-matches on leaf data-item element names (Execution,
//!   Availability, Program, PartCount, ControllerMode, RotaryVelocity)
//!   per the MTConnect Streams 1.7 schema.
//!
//! ## Health model
//!
//! - Each 5-second poll IS the periodic probe (no separate liveness
//!   check). After every poll the cached `AdapterHealth` snapshot
//!   reflects the outcome:
//!   - HTTP 200 + parseable `<MTConnectStreams>` root, elapsed under
//!     `slow_threshold` (default 2s) → `Healthy`.
//!   - HTTP 200 + parse OK but elapsed over `slow_threshold` →
//!     `Degraded { reason: "slow response Nms" }`.
//!   - HTTP non-2xx, connect error, body unparseable, request timed out
//!     (>`request_timeout`) → `Unhealthy { reason }`.
//!
//! Per [[trust-code-not-operator]] the operator never needs to notice a
//! transient hiccup; the next 5-second tick re-probes and the Workshop
//! dashboard tile (S240 / PR-234) recovers on its own. The reconnect
//! cadence IS the poll cadence — reqwest pools connections under the
//! hood, so a transient TCP drop costs one fresh handshake on the next
//! tick.
//!
//! ## Event emission
//!
//! On every successful poll the adapter compares the parsed Execution
//! state against the previous tick's value. If it changed, the adapter
//! broadcasts a [`CanonicalEvent::MachineStateChanged`] — the registered
//! ledger writer ([`spawn_ledger_writer`](crate::spawn_ledger_writer))
//! converts that into a `MesAdapterEvent` audit-ledger row, and any other
//! subscriber (future SPA push, future cell-controller projection) sees
//! the same event. The very first observed state emits an
//! `Unknown → <state>` transition so consumers always have a baseline.
//!
//! Execution → [`MachineState`] mapping (closed):
//!
//! | MTConnect `Execution` value | [`MachineState`]   |
//! |-----------------------------|--------------------|
//! | `ACTIVE`                    | [`MachineState::Running`] |
//! | `READY`                     | [`MachineState::Idle`]    |
//! | `STOPPED`                   | [`MachineState::Down`]    |
//! | `INTERRUPTED`               | [`MachineState::Fault`]   |
//! | `FEED_HOLD`                 | [`MachineState::Idle`]    |
//! | `OPTIONAL_STOP`             | [`MachineState::Idle`]    |
//! | `PROGRAM_STOPPED`           | [`MachineState::Idle`]    |
//! | `PROGRAM_COMPLETED`         | [`MachineState::Idle`]    |
//! | anything else / absent      | [`MachineState::Unknown`] |
//!
//! ## What this adapter does NOT do (v1 — queued for follow-ups)
//!
//! Tracked as PR-240 TODOs:
//!
//! - **MTConnect `Sample` stream subscription** — long-poll / chunked
//!   `/sample?from=X&count=Y` with sequence-based gap detection. v1
//!   re-pulls `/current` every 5s; works against any Agent but misses
//!   sub-poll-interval state pulses.
//! - **Condition / fault stream parsing** — Conditions and Warnings have
//!   their own MTConnect vocabularies; v1 surfaces only Execution as a
//!   `MachineStateChanged` trigger. A future pass can map
//!   `Condition.fault` into `MachineState::Fault`.
//! - **Probe stream for tool/fixture introspection** — `/probe` returns
//!   the device's data-item catalog; useful for validating that the
//!   controller exposes the items we care about.
//! - **Asset tracking** — `cuttingTool`, `workpiece` etc. via `/assets`.
//! - **SHDR-side Adapter SDK** — the "machine talks SHDR to a sidecar
//!   Adapter, sidecar publishes Agent" pattern. We assume the Agent is
//!   already up (built-in on modern controllers; sidecar on legacy
//!   ones); we don't ship Adapter-side code.
//!
//! ## DoS bounds (per [[trust-code-not-operator]])
//!
//! Hard limits enforced in code, not via operator config:
//!
//! - `max_response_bytes` (default 4 MiB) — caps the body the adapter
//!   will read off the wire per poll. MTConnect Streams documents for a
//!   single device run a few hundred KiB at most; 4 MiB is generous
//!   even for a heavily-instrumented 5-axis cell.
//! - `request_timeout` (default 4s) — caps the total time a single
//!   `/current` GET may consume. Set BELOW `poll_interval` (5s) so a
//!   stalled request can't pile up across ticks.
//!
//! ## Lifecycle
//!
//! `start()` builds the reqwest client, runs the initial poll
//! synchronously (so the first `health()` call after start observes the
//! real result), then spawns the periodic poll task. `stop()` cancels
//! via [`CancellationToken`] — the cancel races the in-flight GET via
//! `tokio::select!`, so a Tauri-window close or Ctrl-C drains within
//! the next request boundary. Both methods are idempotent.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use quick_xml::events::Event;
use quick_xml::Reader;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::adapter::{Adapter, AdapterHealth};
use crate::error::AdapterError;
use crate::events::{CanonicalEvent, MachineState};

/// Default MTConnect Agent TCP port — the well-known port the reference
/// Agent (mtconnect.org's C++ reference impl + every commercial Agent we
/// surveyed: DMG MORI MTConnect Agent, Mazak SmartBox, Memex Merlin)
/// listens on by default.
pub const DEFAULT_AGENT_PORT: u16 = 5000;

/// Default interval between consecutive `/current` polls. 5s matches the
/// brief and the Workshop dashboard's ~30s refresh budget (six polls per
/// dashboard tick is plenty of resolution for OEE/state aggregation).
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default hard cap on a single `/current` request. Set below
/// `poll_interval` so a stalled request can't pile up across ticks.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(4);

/// Default threshold above which a successful poll is reported as
/// `Degraded` rather than `Healthy`. The brief calls out >2s as the
/// degraded boundary.
pub const DEFAULT_SLOW_THRESHOLD: Duration = Duration::from_secs(2);

/// Default broadcast channel capacity. 1024 matches the Zebra adapter.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Default cap on a single `/current` response body. 4 MiB is generous
/// — real MTConnect Streams docs for one device run a few hundred KiB
/// even on heavily-instrumented 5-axis cells.
pub const DEFAULT_MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

/// Construction-time configuration for an [`MtconnectAdapter`].
///
/// DoS bounds (`request_timeout`, `max_response_bytes`) are exposed only
/// so tests can shrink them; production paths use the `DEFAULT_*`
/// constants per [[trust-code-not-operator]].
#[derive(Debug, Clone)]
pub struct MtconnectAdapterConfig {
    /// Stable identifier; becomes the adapter's [`Adapter::name`]. Used
    /// as the registry key + the `adapter_name` field on every
    /// audit-ledger entry. MUST be unique across registered adapters.
    /// Typical shape: `"cnc-{line}-{n}"` (e.g. `"cnc-line-a-1"`).
    pub machine_id: String,
    /// Operator-readable display name surfaced on the Workshop dashboard
    /// tile. Distinct from `machine_id` so the operator can rename the
    /// physical machine without disturbing the stable registry key.
    pub friendly_name: String,
    /// Agent host — IP address or DNS name. Resolved on each request;
    /// reqwest's connection pool handles keep-alive.
    pub host: String,
    /// Agent TCP port. Production default is 5000; tests pass ephemeral
    /// ports.
    pub port: u16,
    /// MTConnect device-name path segment. An Agent can publish multiple
    /// devices; the URL is `/{device_name}/current`. Typical shape:
    /// `"M1"`, `"DMG_MORI_NHX_4000"`.
    pub device_name: String,
    pub poll_interval: Duration,
    pub request_timeout: Duration,
    pub slow_threshold: Duration,
    pub max_response_bytes: usize,
    pub channel_capacity: usize,
}

impl MtconnectAdapterConfig {
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

    fn current_url(&self) -> String {
        format!(
            "http://{}:{}/{}/current",
            self.host, self.port, self.device_name
        )
    }
}

/// The MTConnect HTTP-poll [`Adapter`] implementation.
///
/// Clone-cheap via `Arc<MtconnectAdapter>`. Internal state (lifecycle,
/// cached health, broadcast sender, last observed Execution, poll-task
/// handle) is interior-mutable.
#[derive(Debug)]
pub struct MtconnectAdapter {
    config: MtconnectAdapterConfig,
    health: Arc<Mutex<AdapterHealth>>,
    sender: broadcast::Sender<CanonicalEvent>,
    cancel: Mutex<Option<CancellationToken>>,
    poll_handle: Mutex<Option<JoinHandle<()>>>,
    /// Most recently observed Execution-derived [`MachineState`].
    /// `None` until the first successful poll establishes a baseline;
    /// the first emitted event always carries
    /// `previous_state: MachineState::Unknown` for that reason.
    last_state: Arc<Mutex<Option<MachineState>>>,
}

impl MtconnectAdapter {
    /// Construct a stopped adapter ready for `start()`.
    pub fn new(config: MtconnectAdapterConfig) -> Self {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        Self {
            config,
            health: Arc::new(Mutex::new(AdapterHealth::Stopped)),
            sender,
            cancel: Mutex::new(None),
            poll_handle: Mutex::new(None),
            last_state: Arc::new(Mutex::new(None)),
        }
    }

    /// Operator-readable friendly name. Surfaces on the Workshop
    /// dashboard alongside the stable `machine_id`.
    pub fn friendly_name(&self) -> &str {
        &self.config.friendly_name
    }
}

#[async_trait]
impl Adapter for MtconnectAdapter {
    fn name(&self) -> &str {
        &self.config.machine_id
    }

    fn kind(&self) -> &'static str {
        "cnc-machine"
    }

    fn endpoint_host(&self) -> Option<String> {
        Some(self.config.host.clone())
    }

    fn endpoint_port(&self) -> Option<u16> {
        Some(self.config.port)
    }

    async fn start(&self) -> Result<(), AdapterError> {
        // Idempotent: if already running, no-op.
        {
            let current = self.health.lock().expect("health mutex poisoned").clone();
            if !matches!(current, AdapterHealth::Stopped) {
                return Ok(());
            }
            *self.health.lock().expect("health mutex poisoned") = AdapterHealth::Starting;
        }

        let client = reqwest::Client::builder()
            .timeout(self.config.request_timeout)
            .build()
            .map_err(|e| AdapterError::StartFailed(format!("build HTTP client: {e}")))?;

        let url = self.config.current_url();
        let slow_threshold = self.config.slow_threshold;
        let max_response_bytes = self.config.max_response_bytes;

        // Initial poll synchronously so the first `health()` read after
        // start sees the real outcome, not the transient Starting.
        let outcome = poll_once(&client, &url, max_response_bytes).await;
        apply_poll_outcome(
            outcome,
            &self.health,
            &self.last_state,
            &self.sender,
            &self.config.machine_id,
            slow_threshold,
        );

        let cancel = CancellationToken::new();
        *self.cancel.lock().expect("cancel mutex poisoned") = Some(cancel.clone());

        let health_slot = self.health.clone();
        let last_state_slot = self.last_state.clone();
        let sender = self.sender.clone();
        let machine_id = self.config.machine_id.clone();
        let poll_interval = self.config.poll_interval;

        let handle = tokio::spawn(async move {
            run_poll_loop(
                client,
                url,
                cancel,
                poll_interval,
                slow_threshold,
                max_response_bytes,
                health_slot,
                last_state_slot,
                sender,
                machine_id,
            )
            .await;
        });

        *self.poll_handle.lock().expect("poll_handle mutex poisoned") = Some(handle);
        Ok(())
    }

    async fn stop(&self) -> Result<(), AdapterError> {
        // Take handle + cancel under the lock, drop the lock, then
        // await — matches the zebra/barcode_scanner stop() posture.
        let cancel_opt = self.cancel.lock().expect("cancel mutex poisoned").take();
        let handle_opt = self
            .poll_handle
            .lock()
            .expect("poll_handle mutex poisoned")
            .take();

        if let Some(token) = cancel_opt {
            token.cancel();
        }
        if let Some(handle) = handle_opt {
            if let Err(e) = handle.await {
                if e.is_panic() {
                    tracing::error!(
                        machine_id = %self.config.machine_id,
                        "MTConnect poll task panicked during stop: {e}"
                    );
                }
            }
        }

        *self.health.lock().expect("health mutex poisoned") = AdapterHealth::Stopped;
        *self.last_state.lock().expect("last_state mutex poisoned") = None;
        Ok(())
    }

    fn health(&self) -> AdapterHealth {
        self.health.lock().expect("health mutex poisoned").clone()
    }

    fn subscribe(&self) -> broadcast::Receiver<CanonicalEvent> {
        self.sender.subscribe()
    }
}

/// Parsed snapshot of the leaf data items this adapter extracts from a
/// `/current` response. Every field is optional — a healthy Agent may
/// omit any individual item depending on its controller's capabilities.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct MtconnectSnapshot {
    pub execution: Option<String>,
    pub availability: Option<String>,
    pub program: Option<String>,
    pub controller_mode: Option<String>,
    pub part_count: Option<u64>,
    pub spindle_rpm: Option<f64>,
}

#[allow(clippy::too_many_arguments)]
async fn run_poll_loop(
    client: reqwest::Client,
    url: String,
    cancel: CancellationToken,
    poll_interval: Duration,
    slow_threshold: Duration,
    max_response_bytes: usize,
    health_slot: Arc<Mutex<AdapterHealth>>,
    last_state_slot: Arc<Mutex<Option<MachineState>>>,
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
                tracing::debug!(machine_id = %machine_id, "MTConnect poll loop cancelled");
                return;
            }
            _ = tick.tick() => {
                // Race the in-flight HTTP request against cancel so
                // shutdown drains within one request boundary, not one
                // full poll_interval.
                let outcome = tokio::select! {
                    _ = cancel.cancelled() => {
                        tracing::debug!(machine_id = %machine_id, "MTConnect poll cancelled mid-request");
                        return;
                    }
                    o = poll_once(&client, &url, max_response_bytes) => o,
                };
                apply_poll_outcome(
                    outcome,
                    &health_slot,
                    &last_state_slot,
                    &sender,
                    &machine_id,
                    slow_threshold,
                );
            }
        }
    }
}

/// Single poll: HTTP GET, classify response, parse body. Returns the
/// parsed snapshot + elapsed time on success; the caller picks the
/// `Healthy` / `Degraded` verdict from `elapsed`.
async fn poll_once(
    client: &reqwest::Client,
    url: &str,
    max_response_bytes: usize,
) -> Result<(MtconnectSnapshot, Duration), String> {
    let start = std::time::Instant::now();
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| classify_reqwest_error(&e))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    if let Some(cl) = resp.content_length() {
        if cl as usize > max_response_bytes {
            return Err(format!(
                "response too large: {cl} bytes > max {max_response_bytes}"
            ));
        }
    }
    let bytes = resp.bytes().await.map_err(|e| format!("body read: {e}"))?;
    if bytes.len() > max_response_bytes {
        return Err(format!(
            "response too large: {} bytes > max {max_response_bytes}",
            bytes.len()
        ));
    }
    let snapshot = parse_mtconnect_current(&bytes).map_err(|e| format!("parse error: {e}"))?;
    Ok((snapshot, start.elapsed()))
}

fn apply_poll_outcome(
    outcome: Result<(MtconnectSnapshot, Duration), String>,
    health_slot: &Arc<Mutex<AdapterHealth>>,
    last_state_slot: &Arc<Mutex<Option<MachineState>>>,
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

    if let Ok((snapshot, _)) = outcome {
        let new_state = snapshot
            .execution
            .as_deref()
            .map(map_execution_to_state)
            .unwrap_or(MachineState::Unknown);
        let mut last = last_state_slot.lock().expect("last_state mutex poisoned");
        let previous_state = last.unwrap_or(MachineState::Unknown);
        if previous_state != new_state {
            let at_iso8601 = OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
            let event = CanonicalEvent::MachineStateChanged {
                machine_id: machine_id.to_string(),
                previous_state,
                new_state,
                at_iso8601,
            };
            // Ignore `SendError` — broadcast::send returns Err only when
            // no receivers exist, which is a legitimate state (no
            // ledger writer yet attached). The next subscriber will
            // observe the cached `last_state` separately.
            let _ = sender.send(event);
            *last = Some(new_state);
        }
    }
}

/// Parse an MTConnect `/current` response, extracting the six leaf data
/// items this adapter knows about. Unknown elements are skipped (forward-
/// compatibility with future MTConnect Streams schema additions).
pub(crate) fn parse_mtconnect_current(xml: &[u8]) -> Result<MtconnectSnapshot, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut snapshot = MtconnectSnapshot::default();
    let mut saw_root = false;
    let mut current: Option<Vec<u8>> = None;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref()).to_vec();
                if local == b"MTConnectStreams" {
                    saw_root = true;
                }
                current = Some(local);
            }
            Ok(Event::Text(t)) => {
                if let Some(name) = current.as_ref() {
                    let raw = t
                        .unescape()
                        .map_err(|e| format!("text unescape failed: {e}"))?
                        .into_owned();
                    match name.as_slice() {
                        b"Execution" => snapshot.execution = Some(raw),
                        b"Availability" => snapshot.availability = Some(raw),
                        b"Program" => snapshot.program = Some(raw),
                        b"ControllerMode" => snapshot.controller_mode = Some(raw),
                        b"PartCount" => {
                            if let Ok(v) = raw.parse::<u64>() {
                                snapshot.part_count = Some(v);
                            }
                        }
                        b"RotaryVelocity" | b"SpindleSpeed" => {
                            if let Ok(v) = raw.parse::<f64>() {
                                snapshot.spindle_rpm = Some(v);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::End(_)) => {
                current = None;
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(format!(
                    "XML parse failed at position {}: {e}",
                    reader.buffer_position()
                ));
            }
            _ => {}
        }
        buf.clear();
    }

    if !saw_root {
        return Err("missing <MTConnectStreams> root element".to_string());
    }
    Ok(snapshot)
}

/// Map an MTConnect Execution data-item value to the canonical
/// [`MachineState`] vocabulary per ADR-0060 §"The canonical event
/// vocabulary". Closed match — anything we don't recognise lands on
/// [`MachineState::Unknown`], never on a default like Idle (silent
/// misclassification is the failure mode CLAUDE.md rule 12 names).
fn map_execution_to_state(execution: &str) -> MachineState {
    match execution {
        "ACTIVE" => MachineState::Running,
        "READY" => MachineState::Idle,
        "STOPPED" => MachineState::Down,
        "INTERRUPTED" => MachineState::Fault,
        "FEED_HOLD" | "OPTIONAL_STOP" | "PROGRAM_STOPPED" | "PROGRAM_COMPLETED" => {
            MachineState::Idle
        }
        _ => MachineState::Unknown,
    }
}

fn local_name(qualified: &[u8]) -> &[u8] {
    match qualified.iter().rposition(|&b| b == b':') {
        Some(i) => &qualified[i + 1..],
        None => qualified,
    }
}

/// Classify a reqwest error into an operator-readable reason string.
/// Timeouts surface specifically so the health snapshot reason makes the
/// difference between "agent down" and "agent slow" obvious.
fn classify_reqwest_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "request timed out".to_string();
    }
    if e.is_connect() {
        return format!("connect error: {e}");
    }
    if e.is_request() {
        return format!("request error: {e}");
    }
    format!("HTTP error: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex as AsyncMutex;

    /// Pick an ephemeral port — same TOCTOU-tolerant pattern as
    /// `zebra::tests::pick_free_port`.
    async fn pick_free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    /// Behaviour of the mock MTConnect Agent for a given test.
    enum MockBehaviour {
        /// Respond 200 with the given body.
        Ok(String),
        /// Respond 200, but with the given body sequence — each accepted
        /// connection takes the next entry (last entry sticks for all
        /// subsequent connections). Lets a single test observe a state
        /// transition between polls.
        OkSequence(Arc<AsyncMutex<Vec<String>>>),
        /// Respond 404 with empty body.
        Status404,
        /// Sleep `Duration` before responding 200 with the body — used
        /// to drive the slow-response → Degraded path.
        Slow(String, Duration),
        /// Accept the connection but never respond — drives the
        /// request-timeout path.
        Hang,
    }

    async fn spawn_mock_agent(port: u16, behaviour: MockBehaviour) -> tokio::task::JoinHandle<()> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _peer) = match listener.accept().await {
                    Ok(t) => t,
                    Err(_) => return,
                };
                // Read until \r\n\r\n. Don't care about the actual
                // request — the test fixtures always GET /M1/current.
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
                    MockBehaviour::Slow(body, d) => {
                        tokio::time::sleep(*d).await;
                        write_ok(&mut sock, body).await;
                    }
                    MockBehaviour::Hang => {
                        // Hold the socket open without writing.
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
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
        let body = "<error>not found</error>";
        let resp = format!(
            "HTTP/1.1 404 Not Found\r\nContent-Type: application/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.shutdown().await;
    }

    fn cfg_for_test(machine_id: &str, port: u16) -> MtconnectAdapterConfig {
        MtconnectAdapterConfig {
            machine_id: machine_id.to_string(),
            friendly_name: format!("Test {machine_id}"),
            host: "127.0.0.1".to_string(),
            port,
            device_name: "M1".to_string(),
            // Tight bounds for tests.
            poll_interval: Duration::from_millis(150),
            request_timeout: Duration::from_millis(500),
            slow_threshold: Duration::from_millis(250),
            max_response_bytes: 64 * 1024,
            channel_capacity: 16,
        }
    }

    fn streams_xml(execution: &str, part_count: u64) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MTConnectStreams xmlns="urn:mtconnect.org:MTConnectStreams:1.7">
  <Header creationTime="2026-06-05T08:00:00Z" sender="agent" instanceId="1" version="1.7.0" />
  <Streams>
    <DeviceStream name="M1" uuid="abc-123">
      <ComponentStream component="Path" name="path" componentId="p">
        <Events>
          <Execution dataItemId="exec">{execution}</Execution>
          <Program dataItemId="prog">PART_42.NC</Program>
          <ControllerMode dataItemId="cmode">AUTOMATIC</ControllerMode>
        </Events>
        <Samples>
          <PartCount dataItemId="pc">{part_count}</PartCount>
        </Samples>
      </ComponentStream>
      <ComponentStream component="Rotary" name="C" componentId="r">
        <Samples>
          <RotaryVelocity dataItemId="rv">5200</RotaryVelocity>
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

    // ====== Defaults ======

    #[test]
    fn config_defaults_match_documented_constants() {
        let cfg = MtconnectAdapterConfig::new(
            "cnc-line-a-1",
            "DMG MORI NHX 4000",
            "10.0.1.50",
            5000,
            "M1",
        );
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.device_name, "M1");
        assert_eq!(cfg.poll_interval, DEFAULT_POLL_INTERVAL);
        assert_eq!(cfg.request_timeout, DEFAULT_REQUEST_TIMEOUT);
        assert_eq!(cfg.slow_threshold, DEFAULT_SLOW_THRESHOLD);
        assert_eq!(cfg.max_response_bytes, DEFAULT_MAX_RESPONSE_BYTES);
        assert_eq!(cfg.channel_capacity, DEFAULT_CHANNEL_CAPACITY);
    }

    #[test]
    fn current_url_assembles_per_device() {
        let cfg = MtconnectAdapterConfig::new("m", "M", "10.0.0.1", 5000, "M1");
        assert_eq!(cfg.current_url(), "http://10.0.0.1:5000/M1/current");
    }

    // ====== Pure parser ======

    #[test]
    fn parse_extracts_all_known_data_items() {
        let xml = streams_xml("ACTIVE", 42);
        let s = parse_mtconnect_current(xml.as_bytes()).unwrap();
        assert_eq!(s.execution.as_deref(), Some("ACTIVE"));
        assert_eq!(s.availability.as_deref(), Some("AVAILABLE"));
        assert_eq!(s.program.as_deref(), Some("PART_42.NC"));
        assert_eq!(s.controller_mode.as_deref(), Some("AUTOMATIC"));
        assert_eq!(s.part_count, Some(42));
        assert_eq!(s.spindle_rpm, Some(5200.0));
    }

    #[test]
    fn parse_rejects_missing_root() {
        let err =
            parse_mtconnect_current(b"<other/>").expect_err("missing MTConnectStreams must error");
        assert!(err.contains("MTConnectStreams"), "{err}");
    }

    #[test]
    fn parse_rejects_malformed_xml() {
        // Unterminated attribute value — quick-xml's tokenizer rejects
        // this at attribute-parse time. Pinned this specific shape
        // because quick-xml is tolerant of unclosed elements (just
        // yields events until EOF) but NOT of unclosed quoted
        // attributes, which is the kind of garbage a misconfigured
        // Agent would actually emit.
        let err = parse_mtconnect_current(b"<MTConnectStreams><Execution attr=\"unterminated")
            .expect_err("malformed XML must error");
        assert!(err.contains("parse"), "{err}");
    }

    #[test]
    fn parse_ignores_unknown_leaf_elements() {
        let xml = r#"<MTConnectStreams>
            <Execution>READY</Execution>
            <SomeFutureItem>whatever</SomeFutureItem>
        </MTConnectStreams>"#;
        let s = parse_mtconnect_current(xml.as_bytes()).unwrap();
        assert_eq!(s.execution.as_deref(), Some("READY"));
        assert!(s.availability.is_none());
    }

    // ====== Execution → MachineState mapping ======

    #[test]
    fn execution_mapping_pins_closed_vocab() {
        assert_eq!(map_execution_to_state("ACTIVE"), MachineState::Running);
        assert_eq!(map_execution_to_state("READY"), MachineState::Idle);
        assert_eq!(map_execution_to_state("STOPPED"), MachineState::Down);
        assert_eq!(map_execution_to_state("INTERRUPTED"), MachineState::Fault);
        assert_eq!(map_execution_to_state("FEED_HOLD"), MachineState::Idle);
        assert_eq!(map_execution_to_state("OPTIONAL_STOP"), MachineState::Idle);
        assert_eq!(
            map_execution_to_state("PROGRAM_STOPPED"),
            MachineState::Idle
        );
        assert_eq!(
            map_execution_to_state("PROGRAM_COMPLETED"),
            MachineState::Idle
        );
        assert_eq!(map_execution_to_state(""), MachineState::Unknown);
        assert_eq!(map_execution_to_state("bogus"), MachineState::Unknown);
    }

    // ====== Health: live HTTP via mock Agent ======

    #[tokio::test]
    async fn start_against_valid_agent_reports_healthy() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Ok(streams_xml("READY", 0))).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("dmg-1", port));
        adapter.start().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Healthy);
        adapter.stop().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    #[tokio::test]
    async fn start_against_404_reports_unhealthy_with_status() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Status404).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("dmg-404", port));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Unhealthy { reason } => {
                assert!(reason.contains("404"), "{reason}");
            }
            other => panic!("expected Unhealthy, got {other:?}"),
        }
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn start_against_invalid_xml_reports_unhealthy_with_parse_error() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Ok("<not-mtconnect/>".to_string())).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("bad-xml", port));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Unhealthy { reason } => {
                assert!(reason.contains("parse"), "{reason}");
            }
            other => panic!("expected Unhealthy, got {other:?}"),
        }
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn start_against_slow_agent_reports_degraded() {
        let port = pick_free_port().await;
        // Sleep 300ms before responding — exceeds cfg slow_threshold of
        // 250ms, well within request_timeout of 500ms.
        let _mock = spawn_mock_agent(
            port,
            MockBehaviour::Slow(streams_xml("ACTIVE", 0), Duration::from_millis(300)),
        )
        .await;

        let adapter = MtconnectAdapter::new(cfg_for_test("slow", port));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Degraded { reason } => {
                assert!(reason.contains("slow response"), "{reason}");
            }
            other => panic!("expected Degraded, got {other:?}"),
        }
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn start_against_hung_agent_reports_unhealthy_on_timeout() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Hang).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("hung", port));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Unhealthy { reason } => {
                assert!(reason.contains("timed out"), "{reason}");
            }
            other => panic!("expected Unhealthy, got {other:?}"),
        }
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn start_against_closed_port_reports_unhealthy_on_connect() {
        let port = pick_free_port().await;
        // No mock — port should refuse on loopback.
        let adapter = MtconnectAdapter::new(cfg_for_test("closed", port));
        adapter.start().await.unwrap();
        match adapter.health() {
            AdapterHealth::Unhealthy { reason } => {
                assert!(
                    reason.contains("connect")
                        || reason.contains("error")
                        || reason.contains("refused"),
                    "{reason}"
                );
            }
            other => panic!("expected Unhealthy, got {other:?}"),
        }
        adapter.stop().await.unwrap();
    }

    // ====== Event emission ======

    #[tokio::test]
    async fn first_observation_emits_unknown_to_state_transition() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Ok(streams_xml("ACTIVE", 0))).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("first", port));
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        let evt = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("event arrives in time")
            .expect("channel still open");
        match evt {
            CanonicalEvent::MachineStateChanged {
                machine_id,
                previous_state,
                new_state,
                ..
            } => {
                assert_eq!(machine_id, "first");
                assert_eq!(previous_state, MachineState::Unknown);
                assert_eq!(new_state, MachineState::Running);
            }
            other => panic!("expected MachineStateChanged, got {other:?}"),
        }
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn execution_transition_between_polls_emits_event() {
        let port = pick_free_port().await;
        // First connection returns READY; subsequent connections return
        // ACTIVE — the second poll observes the transition.
        let seq = Arc::new(AsyncMutex::new(vec![
            streams_xml("READY", 0),
            streams_xml("ACTIVE", 1),
        ]));
        let _mock = spawn_mock_agent(port, MockBehaviour::OkSequence(seq)).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("trans", port));
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        // First event: Unknown → Idle (READY).
        let evt1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first event in time")
            .expect("channel still open");
        match evt1 {
            CanonicalEvent::MachineStateChanged {
                previous_state,
                new_state,
                ..
            } => {
                assert_eq!(previous_state, MachineState::Unknown);
                assert_eq!(new_state, MachineState::Idle);
            }
            other => panic!("expected MachineStateChanged, got {other:?}"),
        }

        // Second event: Idle → Running (after next poll tick).
        let evt2 = tokio::time::timeout(Duration::from_secs(3), rx.recv())
            .await
            .expect("second event in time")
            .expect("channel still open");
        match evt2 {
            CanonicalEvent::MachineStateChanged {
                previous_state,
                new_state,
                ..
            } => {
                assert_eq!(previous_state, MachineState::Idle);
                assert_eq!(new_state, MachineState::Running);
            }
            other => panic!("expected MachineStateChanged, got {other:?}"),
        }

        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn unchanged_state_across_polls_emits_no_duplicate_event() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Ok(streams_xml("ACTIVE", 0))).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("steady", port));
        let mut rx = adapter.subscribe();
        adapter.start().await.unwrap();

        // First event arrives (Unknown → Running).
        let _evt1 = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("first event in time")
            .expect("channel still open");

        // No second event within two poll intervals (300ms cfg).
        let next = tokio::time::timeout(Duration::from_millis(500), rx.recv()).await;
        assert!(
            next.is_err(),
            "no event expected on unchanged state, got: {next:?}"
        );

        adapter.stop().await.unwrap();
    }

    // ====== Lifecycle ======

    #[tokio::test]
    async fn start_is_idempotent() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Ok(streams_xml("READY", 0))).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("idem-start", port));
        adapter.start().await.unwrap();
        adapter.start().await.unwrap();
        adapter.start().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Healthy);
        adapter.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Ok(streams_xml("READY", 0))).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("idem-stop", port));
        adapter.stop().await.unwrap();
        adapter.start().await.unwrap();
        adapter.stop().await.unwrap();
        adapter.stop().await.unwrap();
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    #[tokio::test]
    async fn stop_cancels_in_flight_poll() {
        let port = pick_free_port().await;
        let _mock = spawn_mock_agent(port, MockBehaviour::Hang).await;

        let adapter = MtconnectAdapter::new(cfg_for_test("cancel-mid", port));
        // start() blocks on the initial poll for up to request_timeout
        // (500ms in the test cfg); after that the poll loop has spawned
        // and the next tick races a fresh hanging request.
        adapter.start().await.unwrap();
        // stop() should complete promptly — the cancel races the
        // in-flight request via tokio::select!, so the loop drains
        // within ms even though the agent is hung.
        let stop_start = std::time::Instant::now();
        adapter.stop().await.unwrap();
        let elapsed = stop_start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "stop took too long: {elapsed:?}"
        );
        assert_eq!(adapter.health(), AdapterHealth::Stopped);
    }

    // ====== Adapter trait surface ======

    #[tokio::test]
    async fn adapter_trait_metadata_fields_match_config() {
        let port = pick_free_port().await;
        let adapter = MtconnectAdapter::new(cfg_for_test("meta", port));
        assert_eq!(adapter.name(), "meta");
        assert_eq!(adapter.kind(), "cnc-machine");
        assert_eq!(adapter.endpoint_host(), Some("127.0.0.1".to_string()));
        assert_eq!(adapter.endpoint_port(), Some(port));
        assert_eq!(adapter.friendly_name(), "Test meta");
    }

    #[tokio::test]
    async fn mtconnect_adapter_is_dyn_safe() {
        let port = pick_free_port().await;
        let adapter: Arc<dyn Adapter> = Arc::new(MtconnectAdapter::new(cfg_for_test("dyn", port)));
        assert_eq!(adapter.name(), "dyn");
        assert_eq!(adapter.kind(), "cnc-machine");
    }
}
